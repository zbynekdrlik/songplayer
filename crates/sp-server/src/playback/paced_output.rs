//! The paced NDI submit side: the emit→submit handoff and the submit
//! consumer, cross-platform so Linux tests drive them over `MockNdiBackend`.
//!
//! **#168 output-side split.** The `genlock_pacing` emit thread used to perform
//! the audio-before-video NDI submit (`send_audio` + `send_video_async`) INLINE
//! at each grid boundary. Box test 5 (2026-09-15, stems child resident) proved
//! that submit stalls 25 ms median / 90 ms p99 under the child's memory
//! pressure, so a dedicated submit thread drains a bounded [`SharedHandoff`]:
//! the emit thread only stamps a frame and hands it over in ~µs.
//!
//! **#147 pipeline-lifetime consumer (design record 5845527884, Approach 1
//! (a)).** The submit thread used to be spawned and joined per song and per
//! idle stretch, so nobody serviced the grid between the old scope's join and
//! the next pre-roll (51–84 ms on the box, 1–3 skipped slots, each a
//! camera-box `stamp_gap`). Now ONE [`PacedOutput`] thread lives for the whole
//! pipeline (owned by the pipeline's `FrameSubmitter`, which drops it first).
//! A song or idle scope only ATTACHES a feeder ([`PacedFeed`]) for its
//! lifetime. While nothing is attached, the consumer services every boundary
//! itself once its deadline passes (`paced_grid.rs`): the last submitted
//! picture (or the standby black) + one silent block, stamped exactly on that
//! boundary, through the same submit + #209 program-bus path as any job. The
//! next pacer continues right after the last serviced stamp, so the stamps are
//! contiguous across every song change, pause and idle.
//!
//! The DECISIONS are pure and Linux-tested (`submit_handoff.rs`,
//! `paced_grid.rs`, the [`PacedConsumer`] methods over `MockNdiBackend`); the
//! blocking waits and the thread lifecycle are `mutants::skip` glue.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use sp_core::genlock::GENLOCK_GRID_FPS;
use sp_core::genlock::audio::samples_per_boundary;
use sp_ndi::{AudioFrame, NdiBackend};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::paced_grid::{GridStep, PacedGrid};
use crate::playback::pacer::{PacedFrame, PacedSink};
use crate::playback::program_bus;
use crate::playback::submit_handoff::{
    HandoffOutcome, PacedSubmitStats, SUBMIT_HANDOFF_BOUND, SubmitCounters, SubmitJob, SubmitQueue,
    paced_submit_snapshot, submit_late_100ns,
};
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::WallClock;

/// The submit thread polls the receiver connection count off the SDK once every
/// this many submitted frames (~1 s at 30 fps) — a cheap cached `timeout=0` read,
/// kept off the per-frame path just to bound its cost.
const CONN_POLL_EVERY: u32 = 30;

/// The audio rate of a fill's silent block (the paced grid's rate, #148).
const FILL_AUDIO_RATE_HZ: u32 = 48_000;

/// A fill's silence layout before any job carried audio: stereo, like the
/// pacer's standby silence (every playlist file decodes to stereo).
const FILL_DEFAULT_CHANNELS: u32 = 2;

/// What the submit consumer does next.
pub enum ConsumerStep {
    /// Submit this job a pacer handed over.
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
    /// #168 r2: the worst per-call `send_video_async` `(max, p99)` (µs) the submit
    /// thread has drained from its `FrameSubmitter.submit_times` since the last
    /// heartbeat — folded worst-of on the connection-poll cadence, drained +
    /// reset by `snapshot()` so it is a per-heartbeat window.
    paced_submit: PacedSubmitStats,
    /// Latest receiver connection count (the submit thread polls it off the SDK).
    connections: i32,
    /// `Instant` of the last real submit, for the heartbeat's staleness check
    /// (kept here, not in the pure counters, because `Instant` is not
    /// deterministically constructible in the Linux unit tests).
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

    /// The submit counters with the grid telemetry folded in.
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

/// Thread-safe wrapper around the pure handoff queue, the submit counters and
/// the #147 grid bookkeeping: one `Mutex` guarding all three, plus a
/// `not_empty` `Condvar` so the consumer blocks (never spins) while nothing is
/// queued and no fill is due.
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
                paced_submit: PacedSubmitStats::default(),
                connections: 0,
                last_submit_instant: None,
                stop: false,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Emit thread: hand a stamped frame to the submit thread (~µs). A full
    /// handoff coalesces to the freshest stamp and records one submit-side drop
    /// (`handoff_policy`). Poison → no-op (the submit thread is gone).
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

    /// Submit thread: block until there is a job to submit, a boundary to
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

    /// Block until a job is available, or return `None` once stop is set AND
    /// the queue is drained. The per-scope consumer's take (#168); it bypasses
    /// the grid. Poison → `None`.
    #[cfg_attr(test, mutants::skip)]
    fn take_blocking(&self) -> Option<SubmitJob> {
        let mut st = self.inner.lock().ok()?;
        loop {
            if let Some(job) = st.queue.take() {
                return Some(job);
            }
            if st.stop {
                return None;
            }
            st = self.not_empty.wait(st).ok()?;
        }
    }

    /// A pacer starts feeding (see [`PacedFeed`]). Returns the last serviced
    /// stamp: the pacer continues on the boundary right after it. Poison →
    /// `None` (the pacer then anchors on its own clock).
    pub fn attach(&self) -> Option<i64> {
        match self.inner.lock() {
            Ok(mut st) => {
                let last = st.grid.attach();
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

    /// Frames submitted so far (jobs + fills), the heartbeat's fps baseline.
    pub fn submitted(&self) -> u64 {
        self.inner
            .lock()
            .map(|st| st.counters.submitted)
            .unwrap_or(0)
    }

    /// Submit thread: record one submitted frame's honest lateness + SDK cost and
    /// stamp the last-submit instant. Poison → no-op.
    #[cfg_attr(test, mutants::skip)]
    fn record_submit(&self, late_100ns: i64, cost_100ns: i64, submit_done_100ns: i64) {
        if let Ok(mut st) = self.inner.lock() {
            st.counters
                .record_submit(late_100ns, cost_100ns, submit_done_100ns);
            st.last_submit_instant = Some(Instant::now());
        }
    }

    /// Submit thread: store the latest receiver connection count.
    #[cfg_attr(test, mutants::skip)]
    fn set_connections(&self, n: i32) {
        if let Ok(mut st) = self.inner.lock() {
            st.connections = n;
        }
    }

    /// Submit thread: fold one drained `(max, p99)` submit-call sub-window into
    /// the per-heartbeat gauge (worst-of, `paced_submit_snapshot`). Called on the
    /// connection-poll cadence right after draining `FrameSubmitter.submit_times`
    /// (#168 r2). Poison → no-op.
    #[cfg_attr(test, mutants::skip)]
    fn observe_submit_call(&self, max: u64, p99: u64) {
        if let Ok(mut st) = self.inner.lock() {
            st.paced_submit = paced_submit_snapshot(st.paced_submit, max, p99);
        }
    }

    /// Emit thread (heartbeat): snapshot the submit counters (with the #147
    /// grid telemetry), connection count, last-submit instant, and the
    /// per-window submit-call gauge for the merged health doc. The submit-call
    /// gauge is DRAINED (reset to default) so each heartbeat sees the window
    /// since the last one (#168 r2). Poison → defaults.
    #[cfg_attr(test, mutants::skip)]
    pub fn snapshot(&self) -> (SubmitCounters, i32, Option<Instant>, PacedSubmitStats) {
        match self.inner.lock() {
            Ok(mut st) => {
                let paced = st.paced_submit;
                st.paced_submit = PacedSubmitStats::default();
                (st.counters(), st.connections, st.last_submit_instant, paced)
            }
            Err(_) => (SubmitCounters::new(), 0, None, PacedSubmitStats::default()),
        }
    }

    /// The pipeline is ending: wake the consumer, which drains the queue,
    /// flushes and exits. IDEMPOTENT (a second call only re-notifies).
    #[cfg_attr(test, mutants::skip)]
    pub fn stop(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.stop = true;
            self.not_empty.notify_all();
        }
    }
}

/// RAII guard that signals the per-scope submit thread to stop if the emit
/// loop UNWINDS, so `thread::scope`'s join never blocks on a parked consumer
/// (#168 review). Idempotent with the normal-path `stop()`.
pub struct StopOnPanic<'a> {
    handoff: &'a SharedHandoff,
}

impl<'a> StopOnPanic<'a> {
    pub fn new(handoff: &'a SharedHandoff) -> Self {
        Self { handoff }
    }
}

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        self.handoff.stop();
    }
}

/// One pacer feeding the paced output for a scope (a song, or an idle
/// stretch, #147): attached on creation, detached on drop — also on unwind, so
/// the grid always goes back to the consumer. The pacer continues right after
/// [`last_serviced_100ns`](Self::last_serviced_100ns).
pub struct PacedFeed<'a> {
    handoff: &'a SharedHandoff,
    last_serviced_100ns: Option<i64>,
}

impl<'a> PacedFeed<'a> {
    /// Attach a feeder to `handoff` (the consumer stops filling).
    pub fn attach(handoff: &'a SharedHandoff) -> Self {
        Self {
            handoff,
            last_serviced_100ns: handoff.attach(),
        }
    }

    /// The output's last serviced stamp at attach time (`None` before any).
    pub fn last_serviced_100ns(&self) -> Option<i64> {
        self.last_serviced_100ns
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

/// The [`PacedSink`] the pacer emits through on the paced path (#168). Instead of
/// the inline blocking NDI submit, it packages the stamped frame + boundary audio
/// into a [`SubmitJob`] and hands it to the [`SharedHandoff`] in ~µs — so the
/// emit thread never blocks on `send_video_async`. The pacer keeps its own
/// `last_frame` clone for the starvation repeat, so the job takes the frame by
/// `Arc` clone (no pixel copy, #203 2b).
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
        self.handoff.offer(SubmitJob::from_paced(
            video,
            audio,
            video_tc_100ns,
            audio_tc_100ns,
        ));
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

/// The pipeline-lifetime submit consumer (#147): owns the submit side's
/// `FrameSubmitter` (built on a non-owning twin of the pipeline's NDI sender)
/// and its own wall clock, and remembers the last submitted picture so a
/// boundary it fills holds it.
pub struct PacedConsumer<B: NdiBackend> {
    submitter: FrameSubmitter<B>,
    playlist_id: i64,
    wall: WallClock,
    /// The last submitted picture; a fill holds it.
    held: Option<Picture>,
    /// The standby black a fill shows when nothing was submitted yet.
    black: Picture,
    /// The channel layout of the last submitted audio block (the fill's
    /// silence follows it).
    channels: u32,
    since_conn_poll: u32,
}

impl<B: NdiBackend> PacedConsumer<B> {
    /// A consumer submitting through `submitter` on `wall`, filling with
    /// `black` until the first job.
    pub fn new(
        submitter: FrameSubmitter<B>,
        playlist_id: i64,
        wall: WallClock,
        black: Picture,
    ) -> Self {
        Self {
            submitter,
            playlist_id,
            wall,
            held: None,
            black,
            channels: FILL_DEFAULT_CHANNELS,
            // Poll the connection count on the very first submit.
            since_conn_poll: CONN_POLL_EVERY,
        }
    }

    /// The consumer's wall clock (100 ns since the epoch).
    pub fn now_100ns(&self) -> i64 {
        self.wall.now_100ns()
    }

    /// Carry out one step: submit the job, or fill the boundary with the held
    /// picture + one silent block. `Wait` / `Exit` do nothing.
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
    /// audio layout, the audio stamped with the consumer's wall clock (§6).
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
            audio_tc_100ns: self.wall.now_100ns(),
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

    /// Submit one job (audio first, then the async NV12 send at its stamp),
    /// offer it to the #209 program bus, and record the honest submit-side
    /// lateness + SDK cost.
    fn submit(&mut self, handoff: &SharedHandoff, job: SubmitJob) {
        let submit_start = self.wall.now_100ns();
        let late = submit_late_100ns(job.stamp_boundary_100ns(), submit_start);
        // #209: the program bus gets the SAME boundary job (an Arc bump of the
        // frame + the audio block, same stamps) — copied before the submit below
        // moves the frame, offered right after it. A fill is offered like a
        // standby pair. Only a source that can own a program boundary pays.
        let pid = self.playlist_id;
        let program = program_bus::installed()
            .and_then(|bus| program_bus::program_copy(bus, pid, &job).map(|c| (bus, c)));
        self.hold(&job);
        // `job.video` is the shared frame (#203 2b): moved straight into the
        // submitter's async holdover, a refcount hold, no copy.
        self.submitter.submit_frame_at_boundary_owned(
            job.width,
            job.height,
            job.stride,
            job.video,
            &job.audio,
            job.video_tc_100ns,
            job.audio_tc_100ns,
        );
        let submit_done = self.wall.now_100ns();
        // `record_submit` floors a negative cost at 0.
        handoff.record_submit(late, submit_done - submit_start, submit_done);
        if let Some((bus, copy)) = program {
            bus.offer(pid, copy);
        }
        self.since_conn_poll += 1;
        if self.since_conn_poll >= CONN_POLL_EVERY {
            handoff.set_connections(self.submitter.sender().get_no_connections(0));
            // #168 r2: drain this ~1 s window's per-call send_video_async gauge
            // and fold it worst-of into the snapshot for the heartbeat.
            let (call_max, call_p99) = self.submitter.drain_submit_call_us();
            handoff.observe_submit_call(call_max, call_p99);
            self.since_conn_poll = 0;
        }
        self.wall.tick();
    }

    /// Release the async double-buffer (the pipeline is ending).
    #[cfg_attr(test, mutants::skip)]
    fn finish(mut self) {
        self.submitter.flush();
        info!(
            playlist_id = self.playlist_id,
            "paced submit consumer: drained + flushed"
        );
    }
}

/// The pipeline-lifetime submit thread's loop (#147): submit every job, fill
/// every detached boundary once its deadline passes, and on stop drain, flush
/// and exit.
#[cfg_attr(test, mutants::skip)]
pub fn run_paced_consumer<B: NdiBackend>(mut consumer: PacedConsumer<B>, handoff: &SharedHandoff) {
    loop {
        let step = handoff.next_blocking(|| consumer.now_100ns());
        match &step {
            ConsumerStep::Exit => break,
            ConsumerStep::Fill {
                stamp_100ns,
                skipped,
            } => {
                if *skipped > 0 {
                    warn!(
                        playlist_id = consumer.playlist_id,
                        skipped,
                        stamp_100ns,
                        "paced output: > 8 boundaries went unserviced between two scopes (grid resync)"
                    );
                }
                info!(
                    playlist_id = consumer.playlist_id,
                    stamp_100ns,
                    held = consumer.held.is_some(),
                    "paced output: serviced a boundary between two scopes (held picture + silence)"
                );
            }
            ConsumerStep::Submit(_) | ConsumerStep::Wait(_) => {}
        }
        consumer.serve(handoff, step);
    }
    consumer.finish();
}

/// The per-scope NDI SUBMIT consumer thread (#168): borrows the
/// [`FrameSubmitter`] via the emit thread's `thread::scope`, drains the
/// handoff on stop, flushes and returns.
#[cfg_attr(test, mutants::skip)]
pub fn run_submit_consumer<B: NdiBackend>(
    submitter: &mut FrameSubmitter<B>,
    handoff: &SharedHandoff,
    playlist_id: i64,
) {
    let mut wall = WallClock::system();
    let mut since_conn_poll: u32 = CONN_POLL_EVERY; // poll on the first frame
    while let Some(job) = handoff.take_blocking() {
        let submit_start = wall.now_100ns();
        let late = submit_late_100ns(job.stamp_boundary_100ns(), submit_start);
        let program = program_bus::installed()
            .and_then(|bus| program_bus::program_copy(bus, playlist_id, &job).map(|c| (bus, c)));
        submitter.submit_frame_at_boundary_owned(
            job.width,
            job.height,
            job.stride,
            job.video,
            &job.audio,
            job.video_tc_100ns,
            job.audio_tc_100ns,
        );
        let submit_done = wall.now_100ns();
        let cost = (submit_done - submit_start).max(0);
        handoff.record_submit(late, cost, submit_done);
        if let Some((bus, copy)) = program {
            bus.offer(playlist_id, copy);
        }
        since_conn_poll += 1;
        if since_conn_poll >= CONN_POLL_EVERY {
            handoff.set_connections(submitter.sender().get_no_connections(0));
            let (call_max, call_p99) = submitter.drain_submit_call_us();
            handoff.observe_submit_call(call_max, call_p99);
            since_conn_poll = 0;
        }
        wall.tick();
    }
    submitter.flush();
    info!(playlist_id, "paced submit consumer: drained + flushed");
}

/// The pipeline-lifetime paced submit thread (#147), owned by the pipeline's
/// `FrameSubmitter`. Dropping it stops the consumer (drain, flush) and joins
/// it — the `FrameSubmitter` drops it BEFORE its owning NDI sender.
pub struct PacedOutput {
    handoff: Arc<SharedHandoff>,
    join: Option<JoinHandle<()>>,
}

impl PacedOutput {
    /// Spawn the consumer thread `paced-submit-<playlist>`.
    #[cfg_attr(test, mutants::skip)]
    pub fn spawn<B: NdiBackend + 'static>(consumer: PacedConsumer<B>) -> Self {
        let handoff = Arc::new(SharedHandoff::new(SUBMIT_HANDOFF_BOUND));
        let thread_handoff = handoff.clone();
        let join = std::thread::Builder::new()
            .name(format!("paced-submit-{}", consumer.playlist_id))
            .spawn(move || run_paced_consumer(consumer, &thread_handoff))
            .expect("spawn paced submit thread");
        Self {
            handoff,
            join: Some(join),
        }
    }

    /// The handoff every scope's pacer feeds.
    pub fn handoff(&self) -> Arc<SharedHandoff> {
        self.handoff.clone()
    }
}

impl Drop for PacedOutput {
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        self.handoff.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
#[path = "paced_output_tests.rs"]
mod tests;
