//! #233: the ASIO output. One worker thread per entry owns its driver (every
//! driver call on it: `asio_win.rs` on Windows, a scripted fake in the tests)
//! and runs `AsioWorker::step`:
//!
//! - closed: open when the backoff allows (`asio_state::backoff_100ns`); the
//!   driver's CURRENT rate, preferred buffer, channels and sample type are
//!   read, never set (the [`AsioDevice`] trait has no setter); a new servo
//!   (`asrc_servo.rs`), resampler and splice (`asrc.rs`) for that rate; a
//!   ring sized for the target + 4 slots; start. A failed open releases the
//!   driver and shows its reason.
//! - running: each program block → the servo's observation (the ring + the
//!   splice's hold, the splice's pending skip, the hand-off lateness, the
//!   card's consumed frames) → its correction to the resampler (an offset is
//!   drained by the ratio, never spliced) → the ring. Only the first block's
//!   priming and a hard re-centre (too little buffered — the ring would run
//!   dry, or a delayed output played over 4 slots early — or too much, the
//!   ring would overflow: a fault, WARNed at most once per 5 s with what it
//!   held back, and shown as `last_hard_recentre`) go through the splice.
//!   Then the
//!   driver's messages: a reset
//!   (or a size change), a rate change or 2 s without a callback closes the
//!   output (`asio_state::close_reason`, `StallWatch`).
//! - parked (`Reason::Parked`, a driver callback that never returned): the
//!   output stays closed for good, with no next try.
//!
//! A closed output drops the blocks it is handed (they would be stale), and
//! so does a running one after its priming until the driver's first tick
//! since the priming (a driver that opens and never calls back would
//! otherwise count a hard re-centre every few slots; #233, found at PP).
//!
//! A driver that opens and gives no clock (no tick since the priming: DVS
//! not running, no Dante PTP clock on its network) is a calm, visible wait
//! (the owner's ruling, #233, 8.10.2026; `asio_state::clock_step`): from 2 s
//! after the open the output reads waiting, reason `no_clock` (one WARN),
//! the driver kept open; 60 s after an open with still no tick it is closed
//! and opened again at once (DEBUG, `clock_waits`, never a reset or a
//! backoff), on and on; its first tick ends the wait (one INFO), the servo
//! starting afresh. A driver that ticked and stops is a stall, as before.
//! The worker is not an MMCSS thread (iemmixer: a helper never pre-empts the
//! driver's own callback thread; it has two slots of cushion). Status:
//! `AsioOut::snapshot` → `GET /api/v1/program` `outputs[i].asio`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use sp_core::audio_outputs::OutputEntry;
use tracing::{debug, info, warn};

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_state::{
    ClockStep, DeviceEvents, POLL_100NS, Reason, StallWatch, admit_rate, asio_latency_ms,
    backoff_100ns, buffer_note, clock_step, close_reason, failures_after_close,
    ring_capacity_frames,
};
use crate::playback::asrc::{Asrc, Splice};
use crate::playback::asrc_servo::{
    BASE_LATENCY_100NS, Observation, Recentre, Servo, frames_from_100ns,
};
use crate::playback::audio_out::{STATE_OPENING, STATE_RUNNING, STATE_WAITING};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::{BlockQueue, RunGuard, Take, lock};
use crate::playback::stat_window::WarnLimiter;
use crate::playback::vban_out::{VbanClock, queue_bound, should_log};
use crate::playback::vban_packet::{VBAN_BLOCK_SAMPLES, VBAN_CHANNELS};

/// What the worker needs from a driver (`asio_win::WinAsioDevice`, the
/// fake). There is no setter: the output follows the driver. Not `Send`:
/// the device lives and dies on its worker thread (COM STA).
pub trait AsioDevice {
    /// Load the driver and read what it runs at (nothing is set).
    fn open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason>;
    /// Create the buffers and start; the callback reads `ring`.
    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason>;
    /// The driver's messages since the last poll (Windows: pumps its
    /// window messages too).
    fn poll(&mut self) -> DeviceEvents;
    /// Frames the card took since the start.
    fn consumed_frames(&self) -> u64;
    /// Callbacks that found the ring short since it was primed.
    fn underruns(&self) -> u64;
    /// The first block is in the ring: a short ring is an underrun from now.
    fn mark_primed(&mut self);
    /// The driver's output latency now, frames (read again after the
    /// driver's `kAsioLatenciesChanged`).
    fn output_latency_frames(&self) -> u32;
    /// Stop and release the driver (idempotent).
    fn close(&mut self);
}

/// What an open read from the driver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opened {
    pub rate: f64,
    pub buffer_frames: u32,
    pub out_channels: u32,
    pub sample: AsioSample,
}

/// What a start read from the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Started {
    pub output_latency_frames: u32,
}

/// `outputs[i].asio`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AsioStatus {
    pub driver: String,
    /// 0-based (the dashboard shows them 1-based).
    pub channels: [u32; 2],
    pub driver_rate: u32,
    pub buffer_frames: u32,
    pub out_channels: u32,
    /// The driver's sample type (`AsioSample::name`), "" before the first open.
    pub sample_type: &'static str,
    /// The correction applied to the resampler.
    pub ppm: f64,
    /// The card's rate against the program wall (the regression).
    pub rate_ppm: f64,
    pub locked: bool,
    /// The output's latency from the boundary, ms (`asio_latency_ms`; 0
    /// until the servo measured its first window, and while waiting).
    pub latency_ms: f64,
    /// The servo's last window: the offset the resampler's ratio drains,
    /// ms — the latency outside [target, target + cushion] (positive =
    /// later; 0 inside it and while waiting).
    pub offset_ms: f64,
    /// The seconds the drain still needs (`None` inside the calm zone, and
    /// while waiting).
    pub slew_eta_s: Option<f64>,
    /// The excess the card's underruns left, kept over the target, ms (#233
    /// comment 6056680979, Q1: at most one slot; it decays slowly through
    /// the level loop; 0 while waiting).
    pub cushion_ms: f64,
    /// Callbacks that found the ring short, since the output was built.
    pub underruns: u64,
    /// Closes (a reset, a rate change, a stall) since the output was built.
    pub resets: u64,
    /// Hard re-centres (a fade, then silence inserted or audio skipped,
    /// because too little was buffered — the ring would run dry, or a
    /// delayed output played more than 4 slots early — or too much, the ring
    /// would overflow), since the output was built: faults. An open's
    /// priming is none.
    pub hard_recentres: u64,
    /// The last hard re-centre, since the output was built.
    pub last_hard_recentre: Option<HardRecentre>,
    /// Frames the ring had no room for, since the output was built.
    pub overflows: u64,
    /// The driver's `kAsioOverload` messages, since the output was built.
    pub overloads: u64,
    /// While waiting: the seconds to the next open (None for a parked
    /// driver, which is never reopened).
    pub retry_in_s: Option<f64>,
    /// While waiting: the reason's stable code (`Reason::code`).
    pub reason_code: Option<&'static str>,
    /// The times the driver, giving no clock for 60 s after an open, was
    /// closed and opened again (`Reason::NoClock`), since the output was
    /// built: a calm wait, neither a reset nor a fault.
    pub clock_waits: u64,
}

/// A hard re-centre as the status shows it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HardRecentre {
    /// `deficit` (too little buffered: the ring would run dry, or a delayed
    /// output played more than 4 slots early) or `excess` (the ring would
    /// overflow).
    pub cause: &'static str,
    /// Inserted (> 0) or skipped (< 0), ms.
    pub ms: f64,
    /// The block's hand-off lateness (handled − its boundary), ms.
    pub lateness_ms: f64,
    /// How long ago, s (as of the status's last update).
    pub ago_s: f64,
}

/// A hard re-centre as the worker keeps it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hard {
    cause: Recentre,
    recentre_100ns: i64,
    lateness_100ns: i64,
    at_100ns: i64,
}

impl Hard {
    /// As the status shows it, at `now_100ns`.
    fn status(&self, now_100ns: i64) -> HardRecentre {
        HardRecentre {
            cause: self.cause.as_str(),
            ms: self.recentre_100ns as f64 / 10_000.0,
            lateness_ms: self.lateness_100ns as f64 / 10_000.0,
            ago_s: (now_100ns - self.at_100ns) as f64 / 1e7,
        }
    }
}

/// At most one hard re-centre WARN per this much of the wall (100 ns).
const HARD_WARN_EVERY_100NS: i64 = 50_000_000;

/// One read of an ASIO output's shared side.
#[derive(Clone, Debug, PartialEq)]
pub struct AsioSnapshot {
    pub state: &'static str,
    pub reason: Option<Reason>,
    pub status: AsioStatus,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
}

struct Live {
    state: &'static str,
    reason: Option<Reason>,
    status: AsioStatus,
    blocks_sent: u64,
}

/// One ASIO entry's shared side: its queue and its status.
pub struct AsioOut {
    id: String,
    queue: BlockQueue,
    driver: String,
    channels: [u32; 2],
    target_100ns: i64,
    live: Mutex<Live>,
    running: AtomicBool,
    start_error: Mutex<Option<String>>,
}

impl AsioOut {
    /// The output of an ASIO entry (its delay adds to the servo's target).
    pub fn for_entry(entry: &OutputEntry) -> Result<Self, String> {
        let a = entry
            .asio
            .as_ref()
            .ok_or_else(|| "not an ASIO entry".to_string())?;
        let delay_100ns = i64::from(entry.delay_ms) * 10_000;
        Ok(Self {
            id: entry.id.clone(),
            queue: BlockQueue::new(queue_bound(delay_100ns)),
            driver: a.driver.clone(),
            channels: a.channels,
            target_100ns: BASE_LATENCY_100NS + delay_100ns,
            live: Mutex::new(Live {
                state: STATE_OPENING,
                reason: None,
                status: AsioStatus {
                    driver: a.driver.clone(),
                    channels: a.channels,
                    ..AsioStatus::default()
                },
                blocks_sent: 0,
            }),
            running: AtomicBool::new(false),
            start_error: Mutex::new(None),
        })
    }

    /// The servo's latency target: two grid slots + the entry's delay.
    pub fn target_100ns(&self) -> i64 {
        self.target_100ns
    }

    /// Hand the output one block (never waits); a full queue drops its
    /// oldest block, WARNed on the first and every 1000th.
    pub fn push(&self, block: ProgramBlock) {
        if let Some(n) = self.queue.push(block)
            && should_log(n)
        {
            warn!(
                id = %self.id,
                blocks_dropped = n,
                bound = self.queue.bound(),
                thread_running = self.is_running(),
                "asio output: queue full — dropped the oldest block (the worker fell behind, or is not running)"
            );
        }
    }

    /// Blocks waiting for the worker.
    pub fn queued(&self) -> usize {
        self.queue.queued()
    }

    /// Stop the worker once its queue is taken (process shutdown).
    pub fn stop(&self) {
        self.queue.stop();
    }

    /// Stop the worker now, its queue dropped (a runtime replace or removal).
    pub fn discard(&self) {
        self.queue.discard();
    }

    /// The worker thread runs ([`run_asio_worker`]).
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Record why the worker thread could not start (the spawn failed).
    pub fn set_start_error(&self, why: String) {
        *lock(&self.start_error) = Some(why);
    }

    /// Why the worker thread could not start, if it could not.
    pub fn start_error(&self) -> Option<String> {
        lock(&self.start_error).clone()
    }

    /// The status, the state and the counters now.
    pub fn snapshot(&self) -> AsioSnapshot {
        let live = lock(&self.live);
        AsioSnapshot {
            state: live.state,
            reason: live.reason.clone(),
            status: live.status.clone(),
            blocks_sent: live.blocks_sent,
            blocks_dropped: self.queue.dropped(),
        }
    }

    /// Off Windows: never runs, says why.
    pub fn set_windows_only(&self) {
        self.update(|l| {
            l.state = STATE_WAITING;
            l.status.reason_code = Some(Reason::WindowsOnly.code());
            l.reason = Some(Reason::WindowsOnly);
        });
    }

    fn update(&self, f: impl FnOnce(&mut Live)) {
        // A guard does not deref-coerce into a generic closure's argument
        // (`rust-workspace.md`): bind the `&mut Live` first.
        let mut guard = lock(&self.live);
        let live: &mut Live = &mut guard;
        f(live);
    }
}

/// An open driver's running side, owned by the worker.
struct Run {
    opened: Opened,
    opened_at_100ns: i64,
    producer: rtrb::Producer<f32>,
    servo: Servo,
    asrc: Asrc,
    splice: Splice,
    zeros: Vec<f32>,
    stall: StallWatch,
    driver_latency_frames: u32,
    overflows: u64,
    /// The driver's overload count this run (its device counts per run).
    overloads: u64,
    primed: bool,
    /// The frames the card had taken when the first block primed the ring:
    /// the driver's ticks count from there (a burst of callbacks at the
    /// open, then nothing, is no clock).
    consumed_at_prime: u64,
}

enum State {
    Closed { retry_at_100ns: i64 },
    Running(Box<Run>),
}

/// The counters of the runs already closed: a run's device, servo and ring
/// count from 0 again, while `outputs[i].asio` counts since the output was
/// built.
#[derive(Clone, Copy, Debug, Default)]
struct Closed {
    underruns: u64,
    overloads: u64,
    overflows: u64,
    hard_recentres: u64,
}

impl Closed {
    /// Add a run's counters (before its device closes).
    fn add(&mut self, run: &Run, device: &dyn AsioDevice) {
        self.underruns += device.underruns();
        self.overloads += run.overloads;
        self.overflows += run.overflows;
        self.hard_recentres += run.servo.status().hard_recentres;
    }

    /// These counts as the output's status counters.
    fn write(&self, s: &mut AsioStatus) {
        s.underruns = self.underruns;
        s.overloads = self.overloads;
        s.overflows = self.overflows;
        s.hard_recentres = self.hard_recentres;
    }
}

/// A closed output's retry instant when it is never retried (a parked
/// driver, a shutdown).
const NEVER: i64 = i64::MAX;

/// The worker's state machine ([`run_asio_worker`] loops on [`Self::step`]).
pub struct AsioWorker {
    state: State,
    failures: u32,
    resets: u64,
    closed: Closed,
    /// The last hard re-centre (shown with its age), and its WARN's limit.
    last_hard: Option<Hard>,
    hard_warns: WarnLimiter,
    /// The driver gives no clock and the output waits for it (the owner's
    /// ruling, #233, 8.10.2026; `asio_state::clock_step`), across the
    /// reopens every 60 s, until it ticks or the output closes otherwise.
    waiting_for_clock: bool,
    /// Those reopens (`AsioStatus::clock_waits`).
    clock_waits: u64,
}

impl AsioWorker {
    /// Closed, due to open at `now_100ns`.
    pub fn new(now_100ns: i64) -> Self {
        Self {
            state: State::Closed {
                retry_at_100ns: now_100ns,
            },
            failures: 0,
            resets: 0,
            closed: Closed::default(),
            last_hard: None,
            hard_warns: WarnLimiter::default(),
            waiting_for_clock: false,
            clock_waits: 0,
        }
    }

    /// One step at `now_100ns`, with the block the queue gave (if any).
    /// Returns how long the loop may wait for the next block (100 ns).
    pub fn step(
        &mut self,
        out: &AsioOut,
        device: &mut dyn AsioDevice,
        now_100ns: i64,
        block: Option<ProgramBlock>,
    ) -> i64 {
        let run = match &mut self.state {
            State::Closed { retry_at_100ns } if now_100ns < *retry_at_100ns => {
                let at = *retry_at_100ns;
                let left = at - now_100ns;
                let retry_in_s = (at != NEVER).then_some(left as f64 / 1e7);
                // The last hard re-centre ages while the output waits too.
                let last_hard = self.last_hard.map(|h| h.status(now_100ns));
                out.update(|l| {
                    l.status.retry_in_s = retry_in_s;
                    l.status.last_hard_recentre = last_hard;
                });
                return left.min(POLL_100NS);
            }
            State::Closed { .. } => {
                self.open(out, device, now_100ns);
                return POLL_100NS;
            }
            State::Running(run) => run,
        };
        // The driver's clock (the owner's ruling, #233, 8.10.2026): one that
        // opens and does not tick is a calm wait, reopened every 60 s, and
        // runs by itself once it ticks. Its ticks count from the priming.
        let ticks = device
            .consumed_frames()
            .saturating_sub(run.consumed_at_prime);
        let clock = clock_step(
            ticks,
            self.waiting_for_clock,
            now_100ns - run.opened_at_100ns,
        );
        match clock {
            ClockStep::Ticking | ClockStep::Quiet => {}
            ClockStep::ClockArrived => {
                self.waiting_for_clock = false;
                // Its run so far observed one block, up to a minute ago: the
                // servo starts afresh, primed by the next block it observes.
                run.servo = new_servo(out, run.opened);
                info!(
                    id = %out.id,
                    driver = %out.driver,
                    "asio output: the driver gives a clock now - running"
                );
                out.update(|l| {
                    l.state = STATE_RUNNING;
                    l.reason = None;
                    l.status.reason_code = None;
                });
            }
            ClockStep::StartsWaiting => {
                self.waiting_for_clock = true;
                warn!(
                    id = %out.id,
                    driver = %out.driver,
                    "asio output: no callback since the driver opened - the output waits for its clock (the driver stays open, opened again every 60 s); e.g. DVS is not running or there is no Dante PTP clock"
                );
                out.update(|l| {
                    l.state = STATE_WAITING;
                    l.status.reason_code = Some(Reason::NoClock.code());
                    l.reason = Some(Reason::NoClock);
                    l.status.retry_in_s = None;
                });
            }
            ClockStep::Reopen => {
                self.clock_waits += 1;
                self.closed.add(run, &*device);
                let (closed, waits) = (self.closed, self.clock_waits);
                out.update(|l| {
                    closed.write(&mut l.status);
                    l.status.clock_waits = waits;
                });
                debug!(
                    id = %out.id,
                    driver = %out.driver,
                    waits,
                    "asio output: still no clock 60 s after the open - closing and opening the driver again"
                );
                self.state = State::Closed {
                    retry_at_100ns: now_100ns,
                };
                device.close();
                return POLL_100NS;
            }
        }
        let ticking = matches!(clock, ClockStep::Ticking | ClockStep::ClockArrived);
        // After the priming, a block waits for the driver's first tick: what
        // a card that takes nothing would pile up is no fault of the output
        // (#233, PP): neither observed nor pushed.
        let block = block.filter(|_| !run.primed || ticking);
        if let Some(hard) = block.and_then(|b| process(run, out, device, now_100ns, b)) {
            if let Some(held_back) = self.hard_warns.admit(now_100ns, HARD_WARN_EVERY_100NS) {
                log_hard(&out.id, &hard, held_back);
            }
            self.last_hard = Some(hard);
        }
        let ev = device.poll();
        run.overloads = ev.overloads;
        if ev.latencies_changed {
            run.driver_latency_frames = device.output_latency_frames();
            info!(
                id = %out.id,
                latency_frames = run.driver_latency_frames,
                "asio output: the driver's latency changed (read again)"
            );
        }
        // A stall is a driver that ticked and stops (a driver that never
        // ticked waits for its clock, above).
        let reason = close_reason(&ev, run.opened.rate).or_else(|| {
            (ticking && run.stall.stalled(ev.callbacks, now_100ns)).then_some(Reason::Stalled)
        });
        match reason {
            Some(reason) => {
                let ran = now_100ns - run.opened_at_100ns;
                self.closed.add(run, &*device);
                // The run's counts go out with its close, and its last hard
                // re-centre.
                let closed = self.closed;
                let last_hard = self.last_hard.map(|h| h.status(now_100ns));
                out.update(|l| {
                    closed.write(&mut l.status);
                    l.status.last_hard_recentre = last_hard;
                });
                self.close(out, device, now_100ns, reason, ran);
            }
            None => publish(
                run,
                out,
                &*device,
                &self.closed,
                self.last_hard.map(|h| h.status(now_100ns)),
            ),
        }
        POLL_100NS
    }

    fn open(&mut self, out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64) {
        match build(out, device, now_100ns) {
            Ok(run) => {
                let o = run.opened;
                let rate = o.rate.round() as u32;
                // While it waits for its clock, a reopen is a retry: DEBUG,
                // and the output keeps reading waiting.
                let waiting = self.waiting_for_clock;
                if waiting {
                    debug!(
                        id = %out.id,
                        driver = %out.driver,
                        "asio output: opened the driver again - still waiting for its clock"
                    );
                } else {
                    info!(
                        id = %out.id,
                        driver = %out.driver,
                        rate,
                        buffer_frames = o.buffer_frames,
                        out_channels = o.out_channels,
                        sample_type = o.sample.name(),
                        latency_frames = run.driver_latency_frames,
                        target_ms = out.target_100ns as f64 / 10_000.0,
                        "asio output: opened the driver (its rate, buffer and sample type are the driver's own)"
                    );
                    if let Some(note) = buffer_note(o.buffer_frames, rate) {
                        warn!(id = %out.id, driver = %out.driver, "asio output: {note}");
                    }
                }
                // Stale by the open's duration: the first block the servo
                // sees is a fresh one.
                log_stale(&out.id, out.queue.clear());
                let (state, reason) = if waiting {
                    (STATE_WAITING, Some(Reason::NoClock))
                } else {
                    (STATE_RUNNING, None)
                };
                out.update(|l| {
                    l.state = state;
                    l.status.reason_code = reason.as_ref().map(Reason::code);
                    l.reason = reason;
                    l.status.driver_rate = rate;
                    l.status.buffer_frames = o.buffer_frames;
                    l.status.out_channels = o.out_channels;
                    l.status.sample_type = o.sample.name();
                    l.status.retry_in_s = None;
                });
                self.state = State::Running(Box::new(run));
            }
            Err(reason) => {
                self.failures = self.failures.saturating_add(1);
                self.wait(
                    out,
                    now_100ns,
                    reason,
                    "asio output: opening the driver failed",
                );
                // Shown first: a vanished driver's release can block.
                device.close();
            }
        }
    }

    fn close(
        &mut self,
        out: &AsioOut,
        device: &mut dyn AsioDevice,
        now_100ns: i64,
        reason: Reason,
        ran_100ns: i64,
    ) {
        self.resets += 1;
        self.failures = failures_after_close(self.failures, ran_100ns);
        let resets = self.resets;
        out.update(|l| l.status.resets = resets);
        // "waiting" and the reason are shown BEFORE the device is closed: a
        // vanished driver can block its stop or release for seconds, and
        // the output must not read "running" meanwhile.
        self.wait(out, now_100ns, reason, "asio output: closing the driver");
        device.close();
    }

    fn wait(&mut self, out: &AsioOut, now_100ns: i64, reason: Reason, what: &str) {
        // Another close ends a wait for the clock (its own WARN says why).
        self.waiting_for_clock = false;
        // A parked driver never opens again in this process: no retry.
        let wait = (reason != Reason::Parked).then(|| backoff_100ns(self.failures));
        let retry_in_s = wait.map(|w| w as f64 / 1e7);
        warn!(
            id = %out.id,
            driver = %out.driver,
            reason = %reason.text(),
            failures = self.failures,
            retry_in_s = ?retry_in_s,
            "{what}"
        );
        self.state = State::Closed {
            retry_at_100ns: wait.map_or(NEVER, |w| now_100ns + w),
        };
        out.update(|l| {
            l.state = STATE_WAITING;
            l.status.retry_in_s = retry_in_s;
            l.status.reason_code = Some(reason.code());
            l.reason = Some(reason);
            // The closed run's figures go with it; the counters and the
            // last open's driver facts stay.
            l.status.ppm = 0.0;
            l.status.rate_ppm = 0.0;
            l.status.locked = false;
            l.status.latency_ms = 0.0;
            l.status.offset_ms = 0.0;
            l.status.slew_eta_s = None;
            l.status.cushion_ms = 0.0;
        });
    }

    /// Process shutdown: release the driver; no step opens it again.
    pub fn shutdown(&mut self, device: &mut dyn AsioDevice) {
        device.close();
        self.state = State::Closed {
            retry_at_100ns: NEVER,
        };
    }
}

/// Open, admit the rate, build the resampler, the ring and the servo, start.
fn build(out: &AsioOut, device: &mut dyn AsioDevice, now_100ns: i64) -> Result<Run, Reason> {
    let opened = device.open(&out.driver, out.channels)?;
    let rate = f64::from(admit_rate(opened.rate)?);
    let asrc = Asrc::new(rate).map_err(Reason::Failed)?;
    let capacity = ring_capacity_frames(rate, out.target_100ns, asrc.max_out_frames());
    let (producer, consumer) = rtrb::RingBuffer::new(capacity * VBAN_CHANNELS);
    let started = device.start(consumer)?;
    let opened = Opened { rate, ..opened };
    Ok(Run {
        opened,
        opened_at_100ns: now_100ns,
        producer,
        servo: new_servo(out, opened),
        splice: Splice::new(rate, capacity, asrc.max_out_frames()),
        asrc,
        zeros: vec![0.0; VBAN_BLOCK_SAMPLES],
        stall: StallWatch::default(),
        driver_latency_frames: started.output_latency_frames,
        overflows: 0,
        overloads: 0,
        primed: false,
        consumed_at_prime: 0,
    })
}

/// A drift servo for `out` on a driver opened as `opened` (its admitted
/// rate and callback period).
fn new_servo(out: &AsioOut, opened: Opened) -> Servo {
    Servo::new(opened.rate, out.target_100ns).with_callback_frames(opened.buffer_frames)
}

/// Frames in the ring now (what the producer filled and the card did not
/// take yet).
fn ring_frames(producer: &rtrb::Producer<f32>) -> usize {
    (producer.buffer().capacity() - producer.slots()) / VBAN_CHANNELS
}

/// One program block into the ring (see the module doc); a hard re-centre
/// it made, if any.
fn process(
    run: &mut Run,
    out: &AsioOut,
    device: &mut dyn AsioDevice,
    now_100ns: i64,
    block: ProgramBlock,
) -> Option<Hard> {
    let action = run.servo.observe(Observation {
        handled_100ns: now_100ns,
        stamp_100ns: block.due_100ns,
        buffered_frames: (ring_frames(&run.producer) + run.splice.held_frames()) as u64,
        pending_skip_frames: run.splice.pending_skip_frames() as u64,
        consumed_frames: device.consumed_frames(),
        // A short callback counts as a whole buffer: an overcount of at
        // most one buffer per event (#233 comment 6056680979, Q1).
        underrun_frames: device
            .underruns()
            .saturating_mul(u64::from(run.opened.buffer_frames)),
    });
    if let Err(e) = run.asrc.set_correction_ppm(action.correction_ppm) {
        warn!(id = %out.id, %e, "asio output: the resampler refused the servo's correction");
    }
    let hard = action
        .recentre
        .filter(|why| *why != Recentre::Prime)
        .map(|cause| Hard {
            cause,
            recentre_100ns: action.recentre_100ns,
            lateness_100ns: now_100ns - block.due_100ns,
            at_100ns: now_100ns,
        });
    let frames = frames_from_100ns(action.recentre_100ns, run.opened.rate);
    match frames.cmp(&0) {
        std::cmp::Ordering::Greater => run.splice.insert(frames.unsigned_abs() as usize),
        std::cmp::Ordering::Less => run.splice.skip(frames.unsigned_abs() as usize),
        std::cmp::Ordering::Equal => {}
    }
    let input = block
        .samples
        .as_deref()
        .filter(|s| s.len() == VBAN_BLOCK_SAMPLES)
        .unwrap_or(&run.zeros);
    let resampled = match run.asrc.process(input) {
        Ok(r) => r,
        Err(e) => {
            warn!(id = %out.id, %e, "asio output: the resampler refused a block");
            return hard;
        }
    };
    let spliced = run.splice.process(resampled);
    let (_, rest) = run.producer.push_partial_slice(spliced);
    run.overflows += (rest.len() / VBAN_CHANNELS) as u64;
    if !run.primed {
        device.mark_primed();
        run.primed = true;
        run.consumed_at_prime = device.consumed_frames();
    }
    out.update(|l| l.blocks_sent += 1);
    hard
}

/// A hard re-centre's WARN (`held_back`: those since the last WARN).
#[cfg_attr(test, mutants::skip)] // logging only; the decision is WarnLimiter's (tested)
fn log_hard(id: &str, h: &Hard, held_back: u64) {
    let s = h.status(h.at_100ns);
    warn!(
        id,
        cause = s.cause,
        ms = s.ms,
        lateness_ms = s.lateness_ms,
        held_back,
        "asio output: a hard re-centre (a fault) — too little buffered (deficit: the ring would run dry, or a delayed output played over 4 slots early) or too much (excess: the ring would overflow): silence inserted (+) or audio skipped (−) under fades"
    );
}

/// The blocks an open dropped, logged when there were any.
#[cfg_attr(test, mutants::skip)] // logging only; the drop is pinned by its test
fn log_stale(id: &str, stale: usize) {
    if stale > 0 {
        info!(
            id,
            stale, "asio output: dropped the blocks queued while the driver opened"
        );
    }
}

/// The running output's numbers into its status (its counters: the closed
/// runs' + this run's).
fn publish(
    run: &Run,
    out: &AsioOut,
    device: &dyn AsioDevice,
    closed: &Closed,
    last_hard: Option<HardRecentre>,
) {
    let servo = run.servo.status();
    let mut counters = *closed;
    counters.add(run, device);
    let latency = asio_latency_ms(
        servo.latency_ms,
        run.asrc.delay_frames(),
        run.driver_latency_frames,
        run.opened.rate,
    );
    out.update(|l| {
        l.status.ppm = servo.correction_ppm;
        l.status.rate_ppm = servo.rate_ppm;
        l.status.locked = servo.locked;
        l.status.latency_ms = latency;
        l.status.offset_ms = servo.offset_ms;
        l.status.slew_eta_s = servo.slew_eta_s;
        l.status.cushion_ms = servo.cushion_ms;
        l.status.last_hard_recentre = last_hard;
        counters.write(&mut l.status);
    });
}

/// The worker thread's loop: wait for a block (bounded by the step's
/// answer), step, until stopped; then release the driver.
#[cfg_attr(test, mutants::skip)] // a blocking loop around AsioWorker::step (tested step by step)
pub fn run_asio_worker(out: &AsioOut, device: &mut dyn AsioDevice, clock: &mut dyn VbanClock) {
    // #233 release review: cleared at the end, and on a panic the start
    // error names the stop, so the outputs task rebuilds the output.
    let running = RunGuard::start(&out.running, &out.start_error, "ASIO");
    let mut worker = AsioWorker::new(clock.now_100ns());
    let mut wait_100ns = 0;
    loop {
        let wait = Duration::from_nanos(wait_100ns.max(0) as u64 * 100);
        let block = match out.queue.take_timeout(wait) {
            Take::Block(b) => Some(b),
            Take::Idle => None,
            Take::Stopped => break,
        };
        wait_100ns = worker.step(out, device, clock.now_100ns(), block);
    }
    worker.shutdown(device);
    drop(running);
    info!(id = %out.id, driver = %out.driver, "asio output: stopped");
}

#[cfg(test)]
#[path = "asio_out_fake.rs"]
pub(crate) mod fake;
#[cfg(test)]
#[path = "asio_out_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "asio_out_tests_clock.rs"]
mod tests_clock;
#[cfg(test)]
#[path = "asio_out_tests_cushion.rs"]
mod tests_cushion;
#[cfg(test)]
#[path = "asio_out_tests_silent.rs"]
mod tests_silent;
