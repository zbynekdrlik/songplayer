//! `SP-program-MAX` in the program output (#223 S2; design record revision 3,
//! comment 5979609879, R3-1 and R3-2).
//!
//! `SP-program-MAX` is a fixed 3840×2160 picture of what is on `SP-program`,
//! composed on the GPU (`sp-gpu`'s `Compositor`) and shared with Resolume
//! Arena over Spout (`sp-gpu`'s `SpoutSender`, `SP-program-MAX`; Arena lists
//! it as `SPOUT_SP-program-MAX`). What it shows (R3-1) is the NATIVE picture
//! of each boundary, never the 1920×1080 canvas:
//!
//! - a forwarded source boundary → its own picture, the one the decoder or
//!   the NDI input produced, before `SP-program`'s canvas fit;
//! - a fade boundary → both native pictures and the boundary's Q8 weight;
//! - the program's standby → black.
//!
//! The `SP-program` sender (`program_output.rs`) offers each boundary as a
//! [`MaxJob`] right AFTER it handed the boundary's audio to VBAN and BEFORE
//! its own canvas fit and NDI submit: `serve` = `split` → `limit` →
//! `feed_vban` → the MAX offer → `submit_video`. The offer is `Arc` bumps
//! under one short lock ([`MaxOut::offer_with`]) into a 2-deep queue that
//! drops the OLDEST job when it is full and counts it (the `SubmitQueue`
//! hand-off of #168/#209). Nothing on the program thread, VBAN or the
//! genlock timing ever waits for MAX: a stalled MAX consumer only coalesces.
//!
//! The consumer is the `program-max` thread (`program_max_worker.rs`): it
//! builds the compositor and the Spout sender on itself (neither is `Send`),
//! composes each job and sends it. Off Windows there is no Direct3D and no
//! Spout, so no thread runs and the telemetry says `unsupported`.
//!
//! The setting `program_max_enabled` (`sp_core::config`, ON unless it says
//! `"false"`) is read before the thread starts ([`start_max`]) and every
//! [`MAX_SETTINGS_POLL`] after ([`run_max_settings_task`]). While it is off
//! the program offers nothing and the thread holds no compositor and no
//! sender (the Spout name is unregistered).
//!
//! Telemetry: [`MaxStatus`], served as `max` on `GET /api/v1/program`.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use sp_core::config::{SETTING_PROGRAM_MAX_ENABLED, program_max_enabled};
use sp_gpu::{
    CANVAS_HEIGHT, CANVAS_WIDTH, ComposeStats, GpuError, Nv12Picture, SPOUT_SENDER_NAME,
    SpoutSendStats,
};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::loop_stats::percentile_ceil;
use crate::playback::submit_handoff::{HandoffOutcome, SubmitJob, SubmitQueue};

/// How many MAX jobs wait for the `program-max` thread: two, then the oldest
/// is dropped (coalesced) — revision 2's D4 hand-off.
pub const MAX_HANDOFF_BOUND: usize = 2;

/// How often the settings task re-reads `program_max_enabled` (the VBAN
/// settings task's cadence).
pub const MAX_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// How many of the last sent frames the `*_us_p99` figures cover (30 s at
/// 30 frames/s).
pub const MAX_STAT_WINDOW: usize = 900;

/// The phase text while no `program-max` thread takes jobs (before it
/// starts, or after it ended).
pub const MAX_NOT_RUNNING: &str = "the program-max thread is not running";

/// One native NV12 picture of a boundary, as `SP-program` received it (an
/// `Arc` of the source's own buffer, never a copy).
#[derive(Clone, Debug)]
pub struct MaxPicture {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub video: SharedFrame,
}

impl MaxPicture {
    /// The picture of a source's boundary job: its own frame and layout.
    pub fn of(job: &SubmitJob) -> Self {
        Self {
            width: job.width,
            height: job.height,
            stride: job.stride,
            video: job.video.clone(),
        }
    }

    /// The picture as the compositor reads it, labelled `id`.
    pub fn nv12(&self, id: u64) -> Nv12Picture<'_> {
        Nv12Picture {
            id,
            width: self.width,
            height: self.height,
            stride: self.stride,
            data: &self.video,
        }
    }
}

/// What one `SP-program` boundary shows on `SP-program-MAX`, stamped like
/// `SP-program` (Spout carries no timecode; the stamp names the boundary).
#[derive(Clone, Debug)]
pub enum MaxJob {
    /// The program's standby: the black canvas.
    Black { stamp_100ns: i64 },
    /// A forwarded boundary: its native picture.
    Picture {
        stamp_100ns: i64,
        picture: MaxPicture,
    },
    /// A fade boundary: both native pictures (a missing side is black) and
    /// the incoming side's Q8 weight (`MixJob::weight_q8`).
    Fade {
        stamp_100ns: i64,
        from: Option<MaxPicture>,
        to: Option<MaxPicture>,
        weight_q8: u32,
    },
}

impl MaxJob {
    /// The boundary the job is stamped on.
    pub fn stamp_100ns(&self) -> i64 {
        match self {
            MaxJob::Black { stamp_100ns }
            | MaxJob::Picture { stamp_100ns, .. }
            | MaxJob::Fade { stamp_100ns, .. } => *stamp_100ns,
        }
    }
}

/// What the `program-max` thread does next ([`MaxOut::next`]).
#[derive(Debug)]
pub enum MaxNext {
    /// Compose and send this boundary.
    Job(MaxJob),
    /// MAX was switched off: drop the sender and the compositor.
    Release,
    /// The process stops: exit.
    Stop,
}

/// Where the `program-max` thread is, as the telemetry names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaxPhase {
    /// It takes jobs and its last boundary went out (or none came yet).
    Running,
    /// Its last boundary did not go out, and why.
    Failed(String),
    /// No Direct3D / Spout here (off Windows): it does nothing.
    Unsupported,
}

/// The `state` of [`MaxStatus`]: `unsupported` wins (the platform), then
/// `off` (the setting), then the thread's phase (`running`, or `error: …`).
pub fn state_label(phase: &MaxPhase, enabled: bool) -> String {
    match phase {
        MaxPhase::Unsupported => "unsupported".to_string(),
        _ if !enabled => "off".to_string(),
        MaxPhase::Running => "running".to_string(),
        MaxPhase::Failed(why) => format!("error: {why}"),
    }
}

/// `GET /api/v1/program` → `max` (#223 S2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaxStatus {
    /// The setting `program_max_enabled`, as last applied.
    pub enabled: bool,
    /// `running`, `off`, `unsupported` or `error: <why>` ([`state_label`]).
    pub state: String,
    /// The fixed canvas: 3840×2160.
    pub width: u32,
    pub height: u32,
    /// Boundaries composed and sent to Spout.
    pub submitted: u64,
    /// Jobs dropped because the thread was a full queue behind.
    pub coalesced: u64,
    /// Jobs the thread took that did not go out (a failed build or frame, a
    /// backoff). `submitted + failed` = the jobs it took.
    pub failed: u64,
    /// The p99 over the last [`MAX_STAT_WINDOW`] sent frames, µs: the plane
    /// uploads, the draw until the GPU finished it, Spout's `SendTexture`.
    pub upload_us_p99: u64,
    pub draw_us_p99: u64,
    pub send_us_p99: u64,
    /// Lost devices: each drops the compositor and the sender, rebuilt on
    /// the next job.
    pub device_resets: u64,
    /// Spout senders refused (the name taken, not listed, not registered):
    /// each waits a backoff before a new one.
    pub sender_backoffs: u64,
    /// The Spout sender name (Arena: `SPOUT_<name>`).
    pub spout_name: &'static str,
}

/// The hand-off's shared state (one lock, shared with the program thread).
struct Queue {
    jobs: SubmitQueue<MaxJob>,
    /// The setting, as last applied.
    enabled: bool,
    /// A `program-max` thread takes jobs.
    consumer: bool,
    stop: bool,
    coalesced: u64,
}

impl Queue {
    /// Whether an offered job would be taken.
    fn accepting(&self) -> bool {
        self.enabled && self.consumer && !self.stop
    }

    /// The thread's next step, if one is ready now: a stop wins; while off,
    /// `Release` when it holds GPU objects; else the oldest job.
    fn step(&mut self, holding: bool) -> Option<MaxNext> {
        if self.stop {
            return Some(MaxNext::Stop);
        }
        if !self.enabled {
            return holding.then_some(MaxNext::Release);
        }
        self.jobs.take().map(MaxNext::Job)
    }
}

/// The last sent frames' costs, µs (at most [`MAX_STAT_WINDOW`]).
#[derive(Clone, Default)]
struct Window(VecDeque<u64>);

impl Window {
    fn push(&mut self, us: u64) {
        self.0.push_back(us);
        if self.0.len() > MAX_STAT_WINDOW {
            self.0.pop_front();
        }
    }

    fn p99(&self) -> u64 {
        percentile_ceil(&self.0, 99)
    }
}

/// What the thread reports (its own lock: an API read never contends with
/// the program thread's offer).
struct Stats {
    phase: MaxPhase,
    submitted: u64,
    failed: u64,
    device_resets: u64,
    sender_backoffs: u64,
    upload: Window,
    draw: Window,
    send: Window,
}

/// The `SP-program-MAX` hand-off between the `SP-program` sender and the
/// `program-max` thread, and the MAX telemetry. One per process, held by the
/// program bus (`ProgramBus::max`).
pub struct MaxOut {
    queue: Mutex<Queue>,
    ready: Condvar,
    stats: Mutex<Stats>,
    /// Tests: run inside every offer, before its lock — what the program
    /// thread has done by then.
    #[cfg(test)]
    on_offer: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Default for MaxOut {
    fn default() -> Self {
        Self::new()
    }
}

/// While a `program-max` thread takes jobs ([`MaxOut::attach`]); dropped, it
/// stops the offers and drops what is queued.
pub struct Consumer<'a>(&'a MaxOut);

impl Drop for Consumer<'_> {
    fn drop(&mut self) {
        {
            let mut queue = self.0.lock_queue();
            queue.consumer = false;
            while queue.jobs.take().is_some() {}
        }
        let mut stats = self.0.lock_stats();
        if stats.phase != MaxPhase::Unsupported {
            stats.phase = MaxPhase::Failed(MAX_NOT_RUNNING.to_string());
        }
    }
}

impl MaxOut {
    /// Off (the setting is applied at startup), no thread yet.
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                jobs: SubmitQueue::new(MAX_HANDOFF_BOUND),
                enabled: false,
                consumer: false,
                stop: false,
                coalesced: 0,
            }),
            ready: Condvar::new(),
            stats: Mutex::new(Stats {
                phase: MaxPhase::Failed(MAX_NOT_RUNNING.to_string()),
                submitted: 0,
                failed: 0,
                device_resets: 0,
                sender_backoffs: 0,
                upload: Window::default(),
                draw: Window::default(),
                send: Window::default(),
            }),
            #[cfg(test)]
            on_offer: Mutex::new(None),
        }
    }

    fn lock_queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_stats(&self) -> MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Tests: run `hook` inside every later offer.
    #[cfg(test)]
    pub(crate) fn set_on_offer(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_offer.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Tests: the offer hook, if one is set.
    #[cfg(test)]
    fn run_on_offer(&self) {
        if let Some(hook) = &*self.on_offer.lock().unwrap_or_else(|p| p.into_inner()) {
            hook();
        }
    }

    /// Apply the setting; returns whether it changed. Off drops the queued
    /// jobs; the thread is woken either way (off: it releases the sender).
    pub fn set_enabled(&self, on: bool) -> bool {
        let changed = {
            let mut queue = self.lock_queue();
            let changed = queue.enabled != on;
            queue.enabled = on;
            if !on {
                while queue.jobs.take().is_some() {}
            }
            changed
        };
        self.ready.notify_all();
        changed
    }

    /// The setting, as last applied.
    pub fn enabled(&self) -> bool {
        self.lock_queue().enabled
    }

    /// Whether an offer now would be taken: on, a thread takes jobs, not
    /// stopped.
    pub fn accepting(&self) -> bool {
        self.lock_queue().accepting()
    }

    /// How many jobs wait for the thread (at most [`MAX_HANDOFF_BOUND`]).
    pub fn queued(&self) -> usize {
        self.lock_queue().jobs.depth()
    }

    /// The `SP-program` sender: offer the boundary `make` builds (called
    /// only when it would be taken: no `Arc` bump while MAX is off). Never
    /// waits for the thread: a full queue drops its OLDEST job and counts it.
    /// Returns whether the job was queued.
    pub fn offer_with(&self, make: impl FnOnce() -> MaxJob) -> bool {
        #[cfg(test)]
        self.run_on_offer();
        {
            let mut queue = self.lock_queue();
            if !queue.accepting() {
                return false;
            }
            if let HandoffOutcome::Coalesced { .. } = queue.jobs.offer(make()) {
                queue.coalesced += 1;
            }
        }
        self.ready.notify_one();
        true
    }

    /// The `program-max` thread: wait for its next step. `holding` = it
    /// holds a compositor or a sender, which an off setting makes it drop
    /// ([`MaxNext::Release`]); a stop wins over everything.
    pub fn next(&self, holding: bool) -> MaxNext {
        let mut queue = self.lock_queue();
        loop {
            if let Some(step) = queue.step(holding) {
                return step;
            }
            queue = self.ready.wait(queue).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// The step [`next`](Self::next) would return now, without waiting
    /// (`None`: it would wait).
    pub fn try_next(&self, holding: bool) -> Option<MaxNext> {
        self.lock_queue().step(holding)
    }

    /// The `program-max` thread starts taking jobs (until the returned
    /// guard drops).
    pub fn attach(&self) -> Consumer<'_> {
        self.lock_queue().consumer = true;
        let mut stats = self.lock_stats();
        if stats.phase != MaxPhase::Unsupported {
            stats.phase = MaxPhase::Running;
        }
        Consumer(self)
    }

    /// Stop the `program-max` thread (process shutdown).
    pub fn stop(&self) {
        self.lock_queue().stop = true;
        self.ready.notify_all();
    }

    /// A boundary went out: its costs, and `running`. Returns whether the
    /// thread was not running before (it recovered).
    pub fn record_sent(&self, compose: ComposeStats, send: SpoutSendStats) -> bool {
        let mut stats = self.lock_stats();
        stats.submitted += 1;
        stats.upload.push(compose.upload_us);
        stats.draw.push(compose.draw_us);
        stats.send.push(send.send_us);
        let recovered = stats.phase != MaxPhase::Running;
        stats.phase = MaxPhase::Running;
        recovered
    }

    /// A boundary did not go out because of `error`. Returns whether the
    /// state's text changed (a new failure, worth one WARN; the same failure
    /// on every boundary is not).
    pub fn record_failed(&self, error: &GpuError) -> bool {
        let mut stats = self.lock_stats();
        stats.failed += 1;
        let phase = MaxPhase::Failed(error.to_string());
        let changed = stats.phase != phase;
        stats.phase = phase;
        changed
    }

    /// A boundary was skipped while the thread waits out a backoff (the
    /// state keeps the failure that started it).
    pub fn record_skipped(&self) {
        self.lock_stats().failed += 1;
    }

    /// No Direct3D / Spout here: MAX does nothing.
    pub fn record_unsupported(&self) {
        self.lock_stats().phase = MaxPhase::Unsupported;
    }

    /// A lost device: the compositor and the sender are rebuilt.
    pub fn record_device_reset(&self) {
        self.lock_stats().device_resets += 1;
    }

    /// A refused Spout sender: a new one after the backoff.
    pub fn record_sender_backoff(&self) {
        self.lock_stats().sender_backoffs += 1;
    }

    /// The telemetry. The windows are copied under the lock and sorted
    /// after it, so the thread's next record never waits for a sort.
    pub fn status(&self) -> MaxStatus {
        let (enabled, coalesced) = {
            let queue = self.lock_queue();
            (queue.enabled, queue.coalesced)
        };
        let (phase, counts, upload, draw, send) = {
            let stats = self.lock_stats();
            (
                stats.phase.clone(),
                [
                    stats.submitted,
                    stats.failed,
                    stats.device_resets,
                    stats.sender_backoffs,
                ],
                stats.upload.clone(),
                stats.draw.clone(),
                stats.send.clone(),
            )
        };
        let [submitted, failed, device_resets, sender_backoffs] = counts;
        MaxStatus {
            enabled,
            state: state_label(&phase, enabled),
            width: CANVAS_WIDTH,
            height: CANVAS_HEIGHT,
            submitted,
            coalesced,
            failed,
            upload_us_p99: upload.p99(),
            draw_us_p99: draw.p99(),
            send_us_p99: send.p99(),
            device_resets,
            sender_backoffs,
            spout_name: SPOUT_SENDER_NAME,
        }
    }
}

/// Read `program_max_enabled` (ON unless it says `"false"`).
pub async fn load_max_enabled(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let raw = crate::db::models::get_setting(pool, SETTING_PROGRAM_MAX_ENABLED).await?;
    Ok(program_max_enabled(raw.as_deref()))
}

/// Apply the stored setting to `max` (an unreadable one changes nothing).
async fn apply_max_setting(pool: &SqlitePool, max: &MaxOut) {
    match load_max_enabled(pool).await {
        Ok(on) => {
            if max.set_enabled(on) {
                info!(enabled = on, "program max: setting applied");
            }
        }
        Err(e) => warn!(%e, "program max: reading the setting failed"),
    }
}

/// Re-read the setting every `poll` and apply a change; on shutdown, stop
/// the `program-max` thread.
pub async fn run_max_settings_task(
    pool: SqlitePool,
    max: Arc<MaxOut>,
    mut shutdown: broadcast::Receiver<()>,
    poll: Duration,
) {
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(poll) => apply_max_setting(&pool, &max).await,
        }
    }
    max.stop();
    info!("program max: settings task stopped");
}

/// Start MAX (`start_program`, before the `SP-program` thread): apply the
/// setting FIRST (so an off setting never builds a sender), start the
/// settings task, then the `program-max` thread on Windows. Off Windows no
/// thread runs and the telemetry says `unsupported`.
pub async fn start_max(pool: SqlitePool, max: Arc<MaxOut>, shutdown: &broadcast::Sender<()>) {
    apply_max_setting(&pool, &max).await;
    tokio::spawn(run_max_settings_task(
        pool,
        max.clone(),
        shutdown.subscribe(),
        MAX_SETTINGS_POLL,
    ));
    #[cfg(windows)]
    spawn_max_thread(max);
    #[cfg(not(windows))]
    max.record_unsupported();
}

/// Windows: the `program-max` thread on the production GPU (the picked
/// adapter, the sender `SP-program-MAX`). `mutants::skip`: Windows-only
/// spawn glue; the loop is `run_max_loop`, tested with a fake GPU.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn spawn_max_thread(max: Arc<MaxOut>) {
    use crate::playback::program_max_worker::{SpoutGpu, run_max_loop};
    let spawned = std::thread::Builder::new()
        .name("program-max".into())
        .spawn(move || {
            info!(
                spout_name = SPOUT_SENDER_NAME,
                width = CANVAS_WIDTH,
                height = CANVAS_HEIGHT,
                "program max thread started"
            );
            run_max_loop(&max, SpoutGpu);
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "program max: spawning the thread failed — no SP-program-MAX");
    }
}

#[cfg(test)]
#[path = "program_max_tests.rs"]
mod tests;
// Two `cfg`s, not `cfg(all(test, windows))`: cargo-mutants skips a module
// only by a plain `#[cfg(test)]`, and this one never runs on its Linux runner.
#[cfg(test)]
#[cfg(windows)]
#[path = "program_max_tests_warp.rs"]
mod tests_warp;
