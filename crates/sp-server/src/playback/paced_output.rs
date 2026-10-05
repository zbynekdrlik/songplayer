//! The paced output of a playlist's pipeline: the emit→output handoff and the
//! pipeline-lifetime consumer that delivers every boundary to the program bus,
//! cross-platform so Linux tests drive them.
//!
//! **#221 lane 3: a playlist has no NDI output of its own.** Every consumer
//! takes SongPlayer's PROGRAM (`SP-program`, `SP-program-MAX`, VBAN), so the
//! consumer's last hop is the #209 program bus alone ([`BoundaryOut`]; in
//! production [`InstalledBus`]). Before, it sent each boundary to the
//! playlist's own NDI sender first and offered the same job to the bus after.
//!
//! **#168 output-side split.** The `genlock_pacing` emit thread used to
//! perform the NDI submit INLINE at each grid boundary, and it stalled under a
//! resident heavy child; so the emit thread only stamps a frame and hands it
//! over in ~µs through a bounded [`SharedHandoff`], and a dedicated consumer
//! thread takes it from there.
//!
//! **#147 pipeline-lifetime consumer (design record 5845527884, Approach 1
//! (a)).** ONE consumer thread lives for the whole pipeline
//! ([`PipelineOutput`] spawns it on the first paced scope). A song or idle
//! scope only ATTACHES a feeder ([`PacedFeed`]) for its lifetime. While nothing
//! is attached, the consumer services every boundary itself once its deadline
//! passes (`paced_grid.rs`): the last delivered picture (or the standby black)
//! + one silent block, stamped exactly on that boundary, delivered like any
//! job. The next pacer continues right after the last serviced stamp, so the
//! stamps are contiguous across every song change, pause and idle.
//!
//! The DECISIONS are pure and Linux-tested (`submit_handoff.rs`,
//! `paced_grid.rs`, the [`PacedConsumer`] methods over a recording output); the
//! blocking waits and the thread lifecycle are `mutants::skip` glue.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use sp_core::genlock::GENLOCK_GRID_FPS;
use sp_core::genlock::audio::samples_per_boundary;
use sp_ndi::AudioFrame;

use crate::playback::fleet_shift;
use crate::playback::frame_buf::{BlackNv12, SharedFrame};
use crate::playback::paced_grid::{GridStep, PacedGrid};
use crate::playback::pacer::{PacedFrame, PacedSink};
use crate::playback::program_bus::{self, ProgramBus};
use crate::playback::submit_handoff::{
    HandoffOutcome, SUBMIT_HANDOFF_BOUND, SubmitCounters, SubmitJob, SubmitQueue, submit_late_100ns,
};
use crate::playback::wallclock::WallClock;

/// The audio rate of a fill's silent block (the paced grid's rate, #148).
const FILL_AUDIO_RATE_HZ: u32 = 48_000;

/// A fill's silence layout before any job carried audio: stereo, like the
/// pacer's standby silence (every playlist file decodes to stereo).
const FILL_DEFAULT_CHANNELS: u32 = 2;

/// What the consumer does next.
pub enum ConsumerStep {
    /// Deliver this job a pacer handed over.
    Submit(SubmitJob),
    /// Service this boundary itself (no pacer attached, its deadline passed).
    /// `skipped` > 0 only after a > 8-slot resync.
    Fill { stamp_100ns: i64, skipped: u64 },
    /// Nothing to do yet: wait for a job, until the fill deadline if `Some`.
    Wait(Option<i64>),
    /// Stopped and drained: exit.
    Exit,
}

/// The shared state behind the handoff `Mutex`.
struct HandoffState {
    queue: SubmitQueue<SubmitJob>,
    counters: SubmitCounters,
    /// #147: the output's own boundary clock (last serviced stamp, attach
    /// state, fill telemetry).
    grid: PacedGrid,
    /// `Instant` of the last delivered boundary, for the heartbeat's
    /// staleness check (kept here, not in the pure counters, because
    /// `Instant` is not deterministically constructible in the Linux unit
    /// tests).
    last_submit_instant: Option<Instant>,
    /// The pipeline is ending: drain the queue, then exit.
    stop: bool,
}

impl HandoffState {
    /// The consumer's next step at wall time `now_100ns`: a queued job first
    /// (a stale one — at or before the last serviced stamp — is dropped and
    /// counted), then exit once stopped, then the grid's fill decision.
    fn next_step(&mut self, now_100ns: i64) -> ConsumerStep {
        while let Some(job) = self.queue.take() {
            if self.grid.accept_job(job.video_tc_100ns) {
                return ConsumerStep::Submit(job);
            }
            self.counters.record_drop();
        }
        if self.stop {
            return ConsumerStep::Exit;
        }
        match self.grid.step(now_100ns) {
            GridStep::Idle => ConsumerStep::Wait(None),
            GridStep::WaitUntil(deadline) => ConsumerStep::Wait(Some(deadline)),
            GridStep::Fill(stamp_100ns) => ConsumerStep::Fill {
                stamp_100ns,
                skipped: self.grid.commit_fill(stamp_100ns),
            },
        }
    }

    /// A pacer starts feeding: the grid stops filling, and the pacer is told
    /// the newest stamp already on its way out — the last one the consumer
    /// serviced OR a job still queued (at a natural song end the previous
    /// pacer's EOS-tail boundary can sit behind the consumer when the idle
    /// scope attaches). Continuing after anything less would re-emit a
    /// queued stamp: a stale drop, or a coalesce into a real stamp hole.
    fn attach(&mut self) -> Option<i64> {
        let queued = self.queue.newest().map(|job| job.video_tc_100ns);
        self.grid.attach_with_queued(queued)
    }

    /// The counters with the grid telemetry folded in.
    fn counters(&self) -> SubmitCounters {
        let mut counters = self.counters.clone();
        counters.consumer_fill_pairs = self.grid.fill_pairs();
        counters.song_change_unserviced_slots = self.grid.unserviced_slots();
        counters
    }
}

/// How long the consumer waits for a job before the fill deadline
/// `deadline_100ns` at wall time `now_100ns` (zero once it is due).
pub fn wait_before_fill(deadline_100ns: i64, now_100ns: i64) -> Duration {
    let ns = (deadline_100ns - now_100ns).max(0).saturating_mul(100);
    Duration::from_nanos(ns as u64)
}

/// Thread-safe wrapper around the pure handoff queue, the counters and the
/// #147 grid bookkeeping: one `Mutex` guarding all three, plus a `not_empty`
/// `Condvar` so the consumer blocks (never spins) while nothing is queued and
/// no fill is due.
pub struct SharedHandoff {
    inner: Mutex<HandoffState>,
    not_empty: Condvar,
}

impl SharedHandoff {
    /// A handoff bounded to `bound` jobs on the genlock grid.
    pub fn new(bound: usize) -> Self {
        Self {
            inner: Mutex::new(HandoffState {
                queue: SubmitQueue::new(bound),
                counters: SubmitCounters::new(),
                grid: PacedGrid::new(GENLOCK_GRID_FPS),
                last_submit_instant: None,
                stop: false,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Emit thread: hand a stamped frame to the consumer (~µs). A full handoff
    /// coalesces to the freshest stamp and records one drop
    /// (`handoff_policy`). Poison → no-op (the consumer is gone).
    #[cfg_attr(test, mutants::skip)]
    pub fn offer(&self, job: SubmitJob) {
        if let Ok(mut st) = self.inner.lock() {
            if let HandoffOutcome::Coalesced { .. } = st.queue.offer(job) {
                st.counters.record_drop();
            }
            self.not_empty.notify_one();
        }
    }

    /// The consumer's next step at `now_100ns`, without blocking (a
    /// [`ConsumerStep::Wait`] is returned as is). Poison → `Exit`.
    pub fn step_now(&self, now_100ns: i64) -> ConsumerStep {
        match self.inner.lock() {
            Ok(mut st) => st.next_step(now_100ns),
            Err(_) => ConsumerStep::Exit,
        }
    }

    /// Consumer thread: block until there is a job to deliver, a boundary to
    /// fill, or stop + drained (`Exit`). A pending fill deadline bounds the
    /// wait (`wait_timeout`); `now` reads the consumer's wall clock. Never
    /// returns [`ConsumerStep::Wait`]. Poison → `Exit`.
    #[cfg_attr(test, mutants::skip)]
    pub fn next_blocking<F: Fn() -> i64>(&self, now: F) -> ConsumerStep {
        let Ok(mut st) = self.inner.lock() else {
            return ConsumerStep::Exit;
        };
        loop {
            match st.next_step(now()) {
                ConsumerStep::Wait(None) => match self.not_empty.wait(st) {
                    Ok(guard) => st = guard,
                    Err(_) => return ConsumerStep::Exit,
                },
                ConsumerStep::Wait(Some(deadline)) => {
                    let wait = wait_before_fill(deadline, now());
                    match self.not_empty.wait_timeout(st, wait) {
                        Ok((guard, _)) => st = guard,
                        Err(_) => return ConsumerStep::Exit,
                    }
                }
                step => return step,
            }
        }
    }

    /// A pacer starts feeding (see [`PacedFeed`]). Returns the newest stamp
    /// serviced or still queued: the pacer continues on the boundary right
    /// after it. Poison → `None` (the pacer then anchors on its own clock).
    pub fn attach(&self) -> Option<i64> {
        match self.inner.lock() {
            Ok(mut st) => {
                let last = st.attach();
                self.not_empty.notify_all();
                last
            }
            Err(_) => None,
        }
    }

    /// The pacer stopped feeding; the consumer now services the grid itself.
    pub fn detach(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.grid.detach();
            self.not_empty.notify_all();
        }
    }

    /// Boundaries delivered so far (jobs + fills), the heartbeat's fps
    /// baseline.
    pub fn submitted(&self) -> u64 {
        self.inner
            .lock()
            .map(|st| st.counters.submitted)
            .unwrap_or(0)
    }

    /// Consumer thread: record one delivered boundary's honest lateness + its
    /// delivery cost and stamp the last-delivery instant. Poison → no-op.
    #[cfg_attr(test, mutants::skip)]
    fn record_submit(&self, late_100ns: i64, cost_100ns: i64, submit_done_100ns: i64) {
        if let Ok(mut st) = self.inner.lock() {
            st.counters
                .record_submit(late_100ns, cost_100ns, submit_done_100ns);
            st.last_submit_instant = Some(Instant::now());
        }
    }

    /// Emit thread (heartbeat): snapshot the counters (with the #147 grid
    /// telemetry) and the last-delivery instant for the merged health doc.
    /// Poison → defaults.
    #[cfg_attr(test, mutants::skip)]
    pub fn snapshot(&self) -> (SubmitCounters, Option<Instant>) {
        match self.inner.lock() {
            Ok(st) => (st.counters(), st.last_submit_instant),
            Err(_) => (SubmitCounters::new(), None),
        }
    }

    /// The pipeline is ending: wake the consumer, which drains the queue and
    /// exits. IDEMPOTENT (a second call only re-notifies).
    #[cfg_attr(test, mutants::skip)]
    pub fn stop(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.stop = true;
            self.not_empty.notify_all();
        }
    }
}

/// One pacer feeding the paced output for a scope (a song, or an idle
/// stretch, #147): attached on creation, detached on drop — also on unwind, so
/// the grid always goes back to the consumer. The pacer continues right after
/// [`continue_after_100ns`](Self::continue_after_100ns) (the newest stamp
/// serviced or still queued).
pub struct PacedFeed<'a> {
    handoff: &'a SharedHandoff,
    continue_after_100ns: Option<i64>,
}

impl<'a> PacedFeed<'a> {
    /// Attach a feeder to `handoff` (the consumer stops filling).
    pub fn attach(handoff: &'a SharedHandoff) -> Self {
        Self {
            handoff,
            continue_after_100ns: handoff.attach(),
        }
    }

    /// The newest stamp serviced or still queued at attach time (`None`
    /// before any).
    pub fn continue_after_100ns(&self) -> Option<i64> {
        self.continue_after_100ns
    }

    /// The sink this scope's pacer emits through.
    pub fn sink(&self) -> HandoffSink<'a> {
        HandoffSink::new(self.handoff)
    }
}

impl Drop for PacedFeed<'_> {
    fn drop(&mut self) {
        self.handoff.detach();
    }
}

/// The [`PacedSink`] the pacer emits through on the paced path (#168): it
/// packages the stamped frame + boundary audio into a [`SubmitJob`] and hands
/// it to the [`SharedHandoff`] in ~µs. The pacer keeps its own `last_frame`
/// clone for the starvation repeat, so the job takes the frame by `Arc` clone
/// (no pixel copy, #203 2b). A decoder pair ([`PacedSink::emit`]) is marked
/// `live`, a standby pair ([`PacedSink::emit_standby`]) is not (#215 cue gate).
pub struct HandoffSink<'a> {
    handoff: &'a SharedHandoff,
}

impl<'a> HandoffSink<'a> {
    pub fn new(handoff: &'a SharedHandoff) -> Self {
        Self { handoff }
    }
}

impl PacedSink for HandoffSink<'_> {
    fn emit(
        &mut self,
        video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        let job = SubmitJob::from_paced(video, audio, video_tc_100ns, audio_tc_100ns, true);
        self.handoff.offer(job);
    }

    fn emit_standby(
        &mut self,
        video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        let job = SubmitJob::from_paced(video, audio, video_tc_100ns, audio_tc_100ns, false);
        self.handoff.offer(job);
    }
}

/// A picture the consumer can hold on a fill: an NV12 frame by shared
/// reference (a refcount bump per fill, no pixel copy).
#[derive(Clone, Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub video: SharedFrame,
}

/// Where the consumer delivers each serviced boundary (#221 lane 3): the
/// program bus in production ([`InstalledBus`]), a recorder in the tests.
pub trait BoundaryOut {
    /// Deliver playlist `playlist_id`'s boundary `job`.
    fn deliver(&mut self, playlist_id: i64, job: SubmitJob);
}

/// Offer `job` to `bus` when playlist `playlist_id` can own a program
/// boundary. Every paced source reports its progress on every boundary
/// (`ProgramBus::touch`, program candidate or not, so a source that is cut to
/// is already known to be live); a source that cannot own one pays that one
/// lock and nothing else. Returns what the bus did, `None` when it was not
/// offered.
pub fn offer_to_bus(
    bus: &ProgramBus,
    playlist_id: i64,
    job: SubmitJob,
) -> Option<program_bus::OfferOutcome> {
    if !bus.touch(playlist_id, job.video_tc_100ns) {
        return None;
    }
    Some(bus.offer(playlist_id, job))
}

/// The production [`BoundaryOut`]: the process-wide program bus. It is
/// installed by `start_program`, after the startup pipelines exist, so it is
/// looked up on every boundary; before it is installed a boundary goes
/// nowhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct InstalledBus;

impl BoundaryOut for InstalledBus {
    /// mutants::skip — the process-wide bus is a `OnceLock` no unit test can
    /// install for itself (one per test binary); the decision is
    /// [`offer_to_bus`], tested on a bus of its own.
    #[cfg_attr(test, mutants::skip)]
    fn deliver(&mut self, playlist_id: i64, job: SubmitJob) {
        if let Some(bus) = program_bus::installed() {
            offer_to_bus(bus, playlist_id, job);
        }
    }
}

/// The pipeline-lifetime consumer (#147): delivers every job and every fill
/// to its [`BoundaryOut`] on its own wall clock, and remembers the last
/// delivered picture so a boundary it fills holds it.
pub struct PacedConsumer<O: BoundaryOut> {
    out: O,
    playlist_id: i64,
    wall: WallClock,
    /// The last delivered picture; a fill holds it.
    held: Option<Picture>,
    /// The standby black a fill shows when nothing was delivered yet.
    black: Picture,
    /// The channel layout of the last delivered audio block (the fill's
    /// silence follows it).
    channels: u32,
}

impl<O: BoundaryOut> PacedConsumer<O> {
    /// A consumer of playlist `playlist_id` delivering to `out` on `wall`,
    /// filling with `black` until the first job.
    pub fn new(out: O, playlist_id: i64, wall: WallClock, black: Picture) -> Self {
        Self {
            out,
            playlist_id,
            wall,
            held: None,
            black,
            channels: FILL_DEFAULT_CHANNELS,
        }
    }

    /// The consumer's wall clock (100 ns since the epoch).
    pub fn now_100ns(&self) -> i64 {
        self.wall.now_100ns()
    }

    /// Carry out one step: deliver the job, or fill the boundary with the
    /// held picture + one silent block. `Wait` / `Exit` do nothing.
    pub fn serve(&mut self, handoff: &SharedHandoff, step: ConsumerStep) {
        match step {
            ConsumerStep::Submit(job) => self.submit(handoff, job),
            ConsumerStep::Fill { stamp_100ns, .. } => {
                let job = self.fill_job(stamp_100ns);
                self.submit(handoff, job);
            }
            ConsumerStep::Wait(_) | ConsumerStep::Exit => {}
        }
    }

    /// The fill for boundary `stamp_100ns`: the held picture (else the
    /// standby black) + one silent `samples_per_boundary` block in the last
    /// audio layout, BOTH stamped exactly on that boundary (design record
    /// 5845527884): every paced source stamps its audio block on its boundary
    /// (#224). The fill is the boundary's own slot of silence, never a live
    /// pair (#215 cue gate).
    fn fill_job(&self, stamp_100ns: i64) -> SubmitJob {
        let picture = self.held.as_ref().unwrap_or(&self.black);
        let samples = samples_per_boundary(FILL_AUDIO_RATE_HZ as i64, GENLOCK_GRID_FPS);
        SubmitJob {
            width: picture.width,
            height: picture.height,
            stride: picture.stride,
            video: picture.video.clone(),
            audio: vec![AudioFrame {
                data: vec![0.0; samples * self.channels as usize],
                channels: self.channels,
                sample_rate: FILL_AUDIO_RATE_HZ,
                timecode_100ns: None,
            }],
            video_tc_100ns: stamp_100ns,
            audio_tc_100ns: stamp_100ns,
            live: false,
        }
    }

    /// Remember `job`'s picture and audio layout for a later fill.
    fn hold(&mut self, job: &SubmitJob) {
        self.held = Some(Picture {
            width: job.width,
            height: job.height,
            stride: job.stride,
            video: job.video.clone(),
        });
        if let Some(channels) = job.audio.first().map(|a| a.channels).filter(|&c| c > 0) {
            self.channels = channels;
        }
    }

    /// Deliver one job to the output and record its honest lateness (stamp →
    /// delivery start) + the delivery's cost.
    fn submit(&mut self, handoff: &SharedHandoff, job: SubmitJob) {
        let submit_start = self.wall.now_100ns();
        let late = submit_late_100ns(job.stamp_boundary_100ns(), submit_start);
        self.hold(&job);
        self.out.deliver(self.playlist_id, job);
        let submit_done = self.wall.now_100ns();
        // `record_submit` floors a negative cost at 0.
        handoff.record_submit(late, submit_done - submit_start, submit_done);
        self.wall.tick();
    }
}

/// The pipeline-lifetime consumer thread's loop (#147): deliver every job,
/// fill every detached boundary once its deadline passes, and on stop drain
/// and exit. Logging stays bounded however long a window lasts: ONE INFO line
/// at a window's first fill, one when the next job ends it (with the count),
/// DEBUG per fill in between, and a WARN on a > 8-slot resync.
#[cfg_attr(test, mutants::skip)]
pub fn run_paced_consumer<O: BoundaryOut>(mut consumer: PacedConsumer<O>, handoff: &SharedHandoff) {
    let pid = consumer.playlist_id;
    let mut window_fills: u64 = 0;
    loop {
        let step = handoff.next_blocking(|| consumer.now_100ns());
        match &step {
            ConsumerStep::Exit => break,
            ConsumerStep::Fill {
                stamp_100ns,
                skipped,
            } => {
                // #224 part 2: a log shows the WIRE stamp, the one receivers see.
                let wire_stamp_100ns = fleet_shift::wire_100ns(*stamp_100ns);
                if *skipped > 0 {
                    warn!(
                        playlist_id = pid,
                        skipped,
                        stamp_100ns = wire_stamp_100ns,
                        "paced output: > 8 boundaries went unserviced between two scopes (grid resync)"
                    );
                }
                if window_fills == 0 {
                    info!(
                        playlist_id = pid,
                        stamp_100ns = wire_stamp_100ns,
                        held = consumer.held.is_some(),
                        "paced output: no pacer attached — servicing boundaries (held picture + silence)"
                    );
                } else {
                    tracing::debug!(
                        playlist_id = pid,
                        stamp_100ns = wire_stamp_100ns,
                        "paced output: fill"
                    );
                }
                window_fills += 1;
            }
            ConsumerStep::Submit(job) => {
                if window_fills > 0 {
                    info!(
                        playlist_id = pid,
                        fills = window_fills,
                        next_stamp_100ns = fleet_shift::wire_100ns(job.video_tc_100ns),
                        "paced output: a pacer feeds again after the consumer's fills"
                    );
                    window_fills = 0;
                }
            }
            ConsumerStep::Wait(_) => {}
        }
        consumer.serve(handoff, step);
    }
    info!(playlist_id = pid, "paced output: consumer drained + stopped");
}

/// The pipeline-lifetime consumer thread (#147). Dropping it stops the
/// consumer (drain) and joins it.
pub struct PacedOutput {
    handoff: Arc<SharedHandoff>,
    join: Option<JoinHandle<()>>,
}

impl PacedOutput {
    /// Spawn the consumer thread `paced-output-<playlist>`.
    #[cfg_attr(test, mutants::skip)]
    pub fn spawn<O: BoundaryOut + Send + 'static>(consumer: PacedConsumer<O>) -> Self {
        let handoff = Arc::new(SharedHandoff::new(SUBMIT_HANDOFF_BOUND));
        let thread_handoff = handoff.clone();
        let join = std::thread::Builder::new()
            .name(format!("paced-output-{}", consumer.playlist_id))
            .spawn(move || run_paced_consumer(consumer, &thread_handoff))
            .expect("spawn paced output thread");
        Self {
            handoff,
            join: Some(join),
        }
    }

    /// The handoff every scope's pacer feeds.
    pub fn handoff(&self) -> Arc<SharedHandoff> {
        self.handoff.clone()
    }

    /// Whether the consumer thread has exited: it only does on stop — or on a
    /// panic, which the pipeline's next scope then recovers from by
    /// respawning ([`PipelineOutput::handoff`]).
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for PacedOutput {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        self.handoff.stop();
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            tracing::error!(
                "paced output thread panicked (#147) — the playlist fed nothing until respawn"
            );
        }
    }
}

/// A playlist pipeline's paced output (#147, #221 lane 3): the
/// pipeline-lifetime consumer thread, spawned on the first paced scope and
/// respawned when it is gone, plus the ONE standby black the idle fill, the
/// pre-roll and the consumer's own fills show. Owned by the pipeline thread
/// for its whole life; dropping it stops and joins the consumer.
pub struct PipelineOutput<O: BoundaryOut + Clone + Send + 'static> {
    playlist_id: i64,
    out: O,
    paced: Option<PacedOutput>,
    black: BlackNv12,
}

impl<O: BoundaryOut + Clone + Send + 'static> PipelineOutput<O> {
    /// Playlist `playlist_id`'s output, delivering to `out` (a fresh clone per
    /// consumer thread).
    pub fn new(playlist_id: i64, out: O) -> Self {
        Self {
            playlist_id,
            out,
            paced: None,
            black: BlackNv12::default(),
        }
    }

    /// The standby NV12 black for `width`×`height`, built once for the
    /// pipeline's life (#147) and handed out by `Arc` clone.
    pub fn standby_black_nv12(&mut self, width: u32, height: u32) -> SharedFrame {
        self.black.get(width, height)
    }

    /// The handoff of this pipeline's consumer thread, spawning the thread on
    /// the first call. Its fill black is the cached `black_w`×`black_h`
    /// standby black; every later call returns the same handoff. A thread that
    /// is gone (it only exits on stop, so: it panicked) is joined, logged and
    /// respawned here, so a dead consumer costs at most the rest of one scope,
    /// never the playlist.
    pub fn handoff(&mut self, black_w: u32, black_h: u32) -> Arc<SharedHandoff> {
        if let Some(output) = &self.paced {
            if !output.is_finished() {
                return output.handoff();
            }
            tracing::error!(
                playlist_id = self.playlist_id,
                "paced output thread is gone — respawning it for this scope (#147)"
            );
        }
        // Dropping a finished output joins it (and logs its panic, if any).
        self.paced = None;
        let black = Picture {
            width: black_w,
            height: black_h,
            stride: black_w,
            video: self.standby_black_nv12(black_w, black_h),
        };
        let consumer = PacedConsumer::new(
            self.out.clone(),
            self.playlist_id,
            WallClock::system(),
            black,
        );
        let output = PacedOutput::spawn(consumer);
        let handoff = output.handoff();
        self.paced = Some(output);
        handoff
    }
}

#[cfg(test)]
#[path = "paced_output_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "paced_output_tests_bus.rs"]
mod tests_bus;
