//! Windows-only NDI submit consumer thread (#168 output-side split).
//!
//! The `genlock_pacing` emit thread used to perform the audio-before-video NDI
//! submit (`send_audio` + `send_video_async`) INLINE at each grid boundary. Box
//! test 5 (2026-09-15, stems child resident) proved that submit stalls 25 ms
//! median / 90 ms p99 under the child's memory pressure — `send_video_async`
//! blocks until the prior async frame drains — so 27.6 % of emits land late even
//! though decode is already off-thread (#147).
//!
//! This module is the symmetric OUTPUT-side split: a dedicated submit thread owns
//! the [`FrameSubmitter`] (borrowed for the song via the emit thread's
//! `std::thread::scope`, so SDK per-instance affinity + the async double-buffer
//! holdover stay single-threaded) and drains the bounded [`SharedHandoff`]. The
//! emit thread only stamps a frame and hands it over in ~µs, so it stays on the
//! grid; the submit thread absorbs the `send_video_async` stalls.
//!
//! The DECISIONS (bound, coalesce policy, honest lateness math, counters, stats
//! merge) are the PURE, Linux-tested [`crate::playback::submit_handoff`] layer;
//! this file is the `Mutex`/`Condvar` + thread plumbing — Windows-consumed,
//! box-verified, `mutants::skip` glue (mirrors `pipeline_paced::run_decode_producer`).

use std::sync::{Condvar, Mutex};
use std::time::Instant;

use tracing::info;

use sp_ndi::AudioFrame;

use crate::playback::ndi_health::{AudioStats, PacingStats, PlaybackStateLabel};
use crate::playback::pacer::{PacedFrame, PacedSink};
use crate::playback::pipeline::{PipelineEvent, classify_bad_poll};
use crate::playback::submit_handoff::{
    HandoffOutcome, SubmitCounters, SubmitJob, SubmitQueue, merge_pacing_stats, submit_late_100ns,
};
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::WallClock;

/// The submit thread polls the receiver connection count off the SDK once every
/// this many submitted frames (~1 s at 30 fps) — a cheap cached `timeout=0` read,
/// kept off the per-frame path just to bound its cost.
const CONN_POLL_EVERY: u32 = 30;

/// The shared state behind the handoff `Mutex`.
struct HandoffState {
    queue: SubmitQueue<SubmitJob>,
    counters: SubmitCounters,
    /// Latest receiver connection count (the submit thread polls it off the SDK).
    connections: i32,
    /// `Instant` of the last real submit, for the heartbeat's staleness check
    /// (kept here, not in the pure counters, because `Instant` is not
    /// deterministically constructible in the Linux unit tests).
    last_submit_instant: Option<Instant>,
    /// The emit thread signalled end-of-song — drain, then exit.
    stop: bool,
    /// EOS audio tail to submit after the queue drains (set at stop).
    eos_tail: Option<(Vec<AudioFrame>, i64)>,
}

/// Thread-safe wrapper around the pure #168 handoff queue + submit counters: a
/// `Mutex` guarding both, plus a `not_empty` `Condvar` so the submit consumer
/// blocks (never spins) while the emit thread has produced nothing. The pure
/// DECISIONS live in `submit_handoff`; these methods are `mutants::skip` glue.
pub(crate) struct SharedHandoff {
    inner: Mutex<HandoffState>,
    not_empty: Condvar,
}

impl SharedHandoff {
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn new(bound: usize) -> Self {
        Self {
            inner: Mutex::new(HandoffState {
                queue: SubmitQueue::new(bound),
                counters: SubmitCounters::new(),
                connections: 0,
                last_submit_instant: None,
                stop: false,
                eos_tail: None,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Emit thread: hand a stamped frame to the submit thread (~µs). A full
    /// handoff coalesces to the freshest stamp and records one submit-side drop
    /// (`handoff_policy`). Poison → no-op (the submit thread is gone).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn offer(&self, job: SubmitJob) {
        if let Ok(mut st) = self.inner.lock() {
            if let HandoffOutcome::Coalesced { .. } = st.queue.offer(job) {
                st.counters.record_drop();
            }
            self.not_empty.notify_one();
        }
    }

    /// Submit thread: block until a job is available, or return `None` once stop
    /// is set AND the queue is drained (so the last frames still ship). Poison →
    /// `None`.
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

    /// Emit thread (heartbeat): snapshot the submit counters, connection count,
    /// and last-submit instant for the merged health doc. Poison → defaults.
    #[cfg_attr(test, mutants::skip)]
    fn snapshot(&self) -> (SubmitCounters, i32, Option<Instant>) {
        match self.inner.lock() {
            Ok(st) => (st.counters.clone(), st.connections, st.last_submit_instant),
            Err(_) => (SubmitCounters::new(), 0, None),
        }
    }

    /// Emit thread: signal end-of-song. `tail` is the EOS audio tail to submit
    /// after the queue drains (or `None`). Wakes the submit thread. IDEMPOTENT:
    /// the FIRST stop wins the tail — a second call (e.g. the [`StopOnPanic`]
    /// guard firing after a normal exit already stopped) never clobbers the tail
    /// already handed over, it only re-notifies. This lets the panic guard call
    /// it unconditionally (#168 review 🟡).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn stop_with_tail(&self, tail: Option<(Vec<AudioFrame>, i64)>) {
        if let Ok(mut st) = self.inner.lock() {
            if !st.stop {
                st.eos_tail = tail;
                st.stop = true;
            }
            self.not_empty.notify_all();
        }
    }

    /// Submit thread: take the EOS tail (once) after draining.
    #[cfg_attr(test, mutants::skip)]
    fn take_eos_tail(&self) -> Option<(Vec<AudioFrame>, i64)> {
        self.inner.lock().ok().and_then(|mut st| st.eos_tail.take())
    }
}

/// RAII guard that signals the submit thread to stop if the emit loop UNWINDS
/// (a panic in `pacer.service` / `sleep_to_boundary` / `event_tx.send`, none of
/// which hold the handoff lock). Without it a panic would leave `stop` false, the
/// submit thread parked forever in `not_empty.wait()`, and `thread::scope`'s join
/// blocking on that parked consumer → the pipeline thread HANGS (a dark,
/// non-recovering wall) instead of unwinding and letting the pipeline restart —
/// exactly the regression the #168 review flagged. On the NORMAL path the emit
/// loop already called `stop_with_tail(tail)` (which wins the tail, being first),
/// so this guard's drop is an idempotent no-op that only re-notifies. Held for
/// the whole `thread::scope` closure; its Drop runs during unwind BEFORE the
/// scope joins the submit thread.
pub(crate) struct StopOnPanic<'a> {
    handoff: &'a SharedHandoff,
}

impl<'a> StopOnPanic<'a> {
    pub(crate) fn new(handoff: &'a SharedHandoff) -> Self {
        Self { handoff }
    }
}

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        self.handoff.stop_with_tail(None);
    }
}

/// The [`PacedSink`] the pacer emits through on the paced path (#168). Instead of
/// the inline blocking NDI submit, it packages the stamped frame + boundary audio
/// into a [`SubmitJob`] and hands it to the [`SharedHandoff`] in ~µs — so the
/// emit thread never blocks on `send_video_async`. The pacer keeps its own
/// `last_frame` clone for the starvation repeat, so cloning the pixels here is
/// required (the same copy that used to live in `submit_frame_at_boundary`, now
/// off the SDK-blocking path).
pub(crate) struct HandoffSink<'a> {
    handoff: &'a SharedHandoff,
}

impl<'a> HandoffSink<'a> {
    pub(crate) fn new(handoff: &'a SharedHandoff) -> Self {
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
        self.handoff.offer(SubmitJob {
            width: video.width,
            height: video.height,
            stride: video.stride,
            video: video.video.clone(),
            audio: audio.to_vec(),
            video_tc_100ns,
            audio_tc_100ns,
        });
    }
}

/// The NDI SUBMIT consumer thread (#168). Owns the [`FrameSubmitter`] for the
/// song (borrowed via the emit thread's `thread::scope`) and performs the
/// blocking `send_audio` + `send_video_async` OFF the boundary-critical emit
/// thread. Reads its own [`WallClock`] to measure the HONEST submit-side lateness
/// (stamp → submit-start) and the SDK submit cost (submit-start → submit-done).
/// Drains the handoff on stop, submits the EOS tail, flushes, and returns (the
/// scope joins it before the next song).
#[cfg_attr(test, mutants::skip)]
pub(crate) fn run_submit_consumer(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    handoff: &SharedHandoff,
    playlist_id: i64,
) {
    let mut wall = WallClock::system();
    let mut since_conn_poll: u32 = CONN_POLL_EVERY; // poll on the first frame
    while let Some(job) = handoff.take_blocking() {
        let submit_start = wall.now_100ns();
        let late = submit_late_100ns(job.stamp_boundary_100ns(), submit_start);
        // Wrap the handoff's owned pixels in a `SharedFrame` (a small Arc header,
        // no pixel copy) so the submitter's holdover is a refcount hold (#203).
        // The handoff clone itself (`HandoffSink::emit`) stays a Vec — the
        // cross-crate NV12 pool that removes it is round 2b.
        submitter.submit_frame_at_boundary_owned(
            job.width,
            job.height,
            job.stride,
            crate::playback::frame_buf::SharedFrame::new(job.video),
            &job.audio,
            job.video_tc_100ns,
            job.audio_tc_100ns,
        );
        let submit_done = wall.now_100ns();
        let cost = (submit_done - submit_start).max(0);
        handoff.record_submit(late, cost, submit_done);

        since_conn_poll += 1;
        if since_conn_poll >= CONN_POLL_EVERY {
            handoff.set_connections(submitter.sender().get_no_connections(0));
            since_conn_poll = 0;
        }
        wall.tick();
    }

    // Drained + stopped: flush the last <1 boundary of audio, then release the
    // async double-buffer before the scope joins us (so `prev_frame` is safe to
    // drop and the next song starts clean).
    if let Some((frames, tc)) = handoff.take_eos_tail() {
        if !frames.is_empty() {
            submitter.submit_audio_tail(&frames, tc);
        }
    }
    submitter.flush();
    info!(playlist_id, "paced submit consumer: drained + flushed");
}

/// Emit a health heartbeat on the PACED path (#168), reading the submit-thread
/// snapshot instead of the [`FrameSubmitter`] (which the submit thread owns for
/// the song). Same `HealthSnapshot` event + bad-poll classification as
/// `pipeline::emit_heartbeat`, but `late_frames` / `max_late_us` / `iter_p99` /
/// `dropped` come from the merged pacer+submit stats, and the frame / connection
/// / last-submit fields come from the submit snapshot. `nominal_fps` is the fixed
/// genlock grid (the paced submitter carries `GENLOCK_GRID_FPS/1`).
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_heartbeat_paced(
    handoff: &SharedHandoff,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    state: PlaybackStateLabel,
    last_heartbeat: &mut Instant,
    consecutive_bad_polls: &mut u32,
    pacer_stats: PacingStats,
    audio: AudioStats,
    prev_total: &mut u64,
    prev_instant: &mut Instant,
) {
    let (submit, connections, last_submit_ts) = handoff.snapshot();
    let pacing = merge_pacing_stats(pacer_stats, &submit);

    let total = submit.submitted;
    let now = Instant::now();
    let window_secs = now.duration_since(*prev_instant).as_secs_f32().max(0.001);
    let window_frames = total.saturating_sub(*prev_total);
    let observed_fps = window_frames as f32 / window_secs;
    let nominal_fps = sp_core::genlock::GENLOCK_GRID_FPS as f32;

    let bad = classify_bad_poll(
        &state,
        connections,
        observed_fps,
        nominal_fps,
        last_submit_ts,
        now,
    );
    if bad {
        *consecutive_bad_polls = consecutive_bad_polls.saturating_add(1);
    } else {
        *consecutive_bad_polls = 0;
    }

    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_total: total,
            frames_submitted_last_5s: window_frames as u32,
            observed_fps,
            nominal_fps,
            last_submit_ts,
            last_heartbeat_ts: now,
            consecutive_bad_polls: *consecutive_bad_polls,
            reported_state: state,
            pacing,
            audio,
            // #192 round 3: the SDK-clocked decode loop's stage/submit gauges do
            // not apply to the paced submit-thread path (genlock_pacing is OFF in
            // production); default here.
            loop_stats: crate::playback::loop_stats::LoopStats::default(),
        },
    ));
    *prev_total = total;
    *prev_instant = now;
    *last_heartbeat = now;
}
