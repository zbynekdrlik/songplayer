//! Emit→submit handoff — the PURE decision layer for the #168 submit-thread
//! split (the output-side twin of `pacer_queue.rs`).
//!
//! Box test 5 (2026-09-15, stems child resident) proved the NDI submit
//! (`send_audio` + `send_video_async`) stalls 25 ms median / 90 ms p99 INLINE on
//! the boundary-critical emit thread, so 27.6 % of emits land late even though
//! decode is already off-thread (#147). The fix moves the submit onto a dedicated
//! thread fed by this BOUNDED handoff: the emit thread stamps a frame and hands
//! it over in ~1 ms, and the submit thread absorbs the `send_video_async` stalls.
//! Because the median submit (25 ms) is below the 33.3 ms grid slot, the submit
//! thread has spare throughput to drain the p99 spikes out of a shallow queue.
//!
//! This module is the PURE, cross-platform, Linux-tested + mutation-scored
//! DECISION layer — the bound + coalesce policy (`handoff_policy`), the honest
//! submit-side lateness math (measured where the frame LEAVES the box, not at the
//! handoff), the telemetry counters, and the pacer/submit stats merge. The
//! `Mutex`/`Condvar` `SharedHandoff` and the submit thread that drive it live in
//! `paced_output.rs`.

use std::collections::VecDeque;

use sp_ndi::AudioFrame;

use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_health::PacingStats;
use crate::playback::pacer::PacedFrame;

/// Handoff depth (grid slots the submit thread may fall behind before a
/// coalesce). Box test 5: median submit 25 ms < the 33.3 ms slot, p99 ~90 ms
/// ≈ 3 slots; depth 2 holds the frames the emit thread produces during a ~2-slot
/// spike while the submit thread's spare throughput (~40 fps capacity vs 30 fps
/// demand) drains it back to zero. Deeper only adds output latency.
pub const SUBMIT_HANDOFF_BOUND: usize = 2;

/// A submitted frame counts as "late" only when it BEGAN leaving the box more
/// than this (100-ns units, 2 ms) past its stamp boundary — the same 2 ms floor
/// the pacer's emit-side `LATE_THRESHOLD_100NS` uses, so the two sides agree on
/// what "late" means. Measured at the SUBMIT thread (frame left the box after its
/// stamp), never at the handoff (#168).
pub const SUBMIT_LATE_THRESHOLD_100NS: i64 = 20_000;

/// Submit-cost percentile ring capacity (µs).
const SUBMIT_COST_RING: usize = 256;

/// One stamped frame handed from the emit thread to the submit thread. Carries
/// everything the submit thread needs to perform the audio-before-video NDI
/// submit at the pre-computed genlock timecodes, plus the `stamp_boundary_100ns`
/// (== `video_tc_100ns`) that the honest submit-side lateness is measured
/// against. `Clone` is an `Arc` bump of the frame + the audio block (the #209
/// program bus takes such a copy of an owned boundary).
#[derive(Clone)]
pub struct SubmitJob {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// The NV12 frame, shared by `Arc` — Arc-cloned from the paced frame at the
    /// handoff (no pixel copy, #203 2b) and moved into the submitter's async
    /// double-buffer holdover on the submit thread.
    pub video: SharedFrame,
    /// The boundary's audio chunk (0 or 1 frame of exactly
    /// `samples_per_boundary` samples, #148).
    pub audio: Vec<AudioFrame>,
    /// Floored on-grid video timecode (100 ns since epoch) — the frame's stamp.
    pub video_tc_100ns: i64,
    /// Raw wall-clock audio timecode (100 ns since epoch) at the boundary.
    pub audio_tc_100ns: i64,
}

impl SubmitJob {
    /// The deadline the honest submit-side lateness is measured against — the
    /// stamped video boundary.
    pub fn stamp_boundary_100ns(&self) -> i64 {
        self.video_tc_100ns
    }

    /// Build a submit job from a paced frame + its boundary audio, taking the
    /// video by `Arc` CLONE — a refcount bump, NO pixel copy (#203 2b, D6). The
    /// pacer keeps its own clone of the SAME allocation for the starvation
    /// repeat, so the handoff no longer needs to copy the pixels off it.
    pub fn from_paced(
        frame: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            video: frame.video.clone(),
            audio: audio.to_vec(),
            video_tc_100ns,
            audio_tc_100ns,
        }
    }
}

/// The outcome of a producer [`SubmitQueue::offer`].
#[derive(Debug, PartialEq, Eq)]
pub enum HandoffOutcome {
    /// Enqueued below the bound; `depth` is the depth after the push.
    Enqueued { depth: usize },
    /// The queue was at its bound (the submit thread is ≥ bound slots behind), so
    /// the OLDEST still-unsent job was DROPPED and this one enqueued — the
    /// freshest stamp is closest to live (`handoff_policy`). `depth` stays at the
    /// bound. The dropped job never reached the SDK, so it is memory-safe to drop
    /// (only the submit thread's own last-submitted buffer is retained by NDI).
    Coalesced { depth: usize },
}

/// A bounded FIFO of stamped submit jobs shared between the emit producer and the
/// submit consumer. PURE — no threading, no I/O. Generic over the payload so
/// tests drive it with a lightweight stand-in.
pub struct SubmitQueue<T> {
    buf: VecDeque<T>,
    bound: usize,
}

impl<T> SubmitQueue<T> {
    /// Build an empty handoff bounded to `bound` jobs.
    pub fn new(bound: usize) -> Self {
        assert!(bound >= 1, "handoff bound must be >= 1");
        Self {
            buf: VecDeque::with_capacity(bound),
            bound,
        }
    }

    /// Current handoff depth (jobs buffered for the submit thread).
    pub fn depth(&self) -> usize {
        self.buf.len()
    }

    /// The handoff is at its bound — the submit thread has not kept up.
    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.bound
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Producer (emit thread): hand off `job`. Below the bound it is appended
    /// ([`HandoffOutcome::Enqueued`]). At the bound the submit thread is ≥ bound
    /// slots behind, so the OLDEST still-unsent job — the one that would land
    /// furthest past its stamp — is dropped and `job` (the freshest stamp) takes
    /// its place ([`HandoffOutcome::Coalesced`]); the depth never exceeds the
    /// bound (`handoff_policy`).
    pub fn offer(&mut self, job: T) -> HandoffOutcome {
        if self.is_full() {
            self.buf.pop_front();
            self.buf.push_back(job);
            HandoffOutcome::Coalesced {
                depth: self.buf.len(),
            }
        } else {
            self.buf.push_back(job);
            HandoffOutcome::Enqueued {
                depth: self.buf.len(),
            }
        }
    }

    /// Consumer (submit thread): pop the oldest job, or `None` when empty.
    pub fn take(&mut self) -> Option<T> {
        self.buf.pop_front()
    }
}

/// Honest submit-side lateness: how long after its stamp boundary the frame
/// BEGAN leaving the box (read on the submit thread right before the SDK send),
/// floored at 0. This is the queueing + backpressure delay the fix drives toward
/// zero — NOT the handoff instant (#168).
pub fn submit_late_100ns(stamp_boundary_100ns: i64, submit_start_100ns: i64) -> i64 {
    (submit_start_100ns - stamp_boundary_100ns).max(0)
}

/// Whether a submit-side lateness counts as a late frame (strictly more than the
/// 2 ms floor).
pub fn is_submit_late(late_100ns: i64) -> bool {
    late_100ns > SUBMIT_LATE_THRESHOLD_100NS
}

/// Submit-thread telemetry, measured where the frame leaves the box (#168). The
/// pacer keeps the SCHEDULING counters (seq/repeats/resyncs/relatches/lag/prep);
/// these HONEST output-side counters replace the pacer's now-meaningless
/// emit-side late/cost in the reported `PacingStats` (see [`merge_pacing_stats`]).
#[derive(Clone, Debug)]
pub struct SubmitCounters {
    /// Frames actually submitted to the SDK.
    pub submitted: u64,
    /// Submitted frames that began leaving the box > 2 ms past their stamp.
    pub late_frames: u64,
    /// Worst submit-side lateness observed (µs).
    pub max_late_us: u64,
    /// Jobs dropped by the handoff coalesce (never reached the SDK).
    pub dropped: u64,
    /// Wall clock (100 ns) of the last real submit; 0 = none yet.
    pub last_submit_100ns: i64,
    // per-frame SDK submit cost ring (µs), for `submit_p99_us`.
    cost_ring: [u64; SUBMIT_COST_RING],
    cost_idx: usize,
    cost_len: usize,
}

impl Default for SubmitCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl SubmitCounters {
    pub fn new() -> Self {
        Self {
            submitted: 0,
            late_frames: 0,
            max_late_us: 0,
            dropped: 0,
            last_submit_100ns: 0,
            cost_ring: [0; SUBMIT_COST_RING],
            cost_idx: 0,
            cost_len: 0,
        }
    }

    /// Record one submitted frame: `late_100ns` = the honest stamp→submit-start
    /// lateness, `cost_100ns` = the SDK submit cost (submit-start → submit-done),
    /// `submit_done_100ns` = the wall clock after the send (for `last_submit`).
    pub fn record_submit(&mut self, late_100ns: i64, cost_100ns: i64, submit_done_100ns: i64) {
        self.submitted += 1;
        if is_submit_late(late_100ns) {
            self.late_frames += 1;
        }
        let late_us = (late_100ns.max(0) / 10) as u64;
        if late_us > self.max_late_us {
            self.max_late_us = late_us;
        }
        self.last_submit_100ns = submit_done_100ns;
        let cost_us = (cost_100ns.max(0) / 10) as u64;
        self.cost_ring[self.cost_idx] = cost_us;
        self.cost_idx = (self.cost_idx + 1) % SUBMIT_COST_RING;
        if self.cost_len < SUBMIT_COST_RING {
            self.cost_len += 1;
        }
    }

    /// Record one handoff-coalesce drop (a job dropped before reaching the SDK).
    pub fn record_drop(&mut self) {
        self.dropped += 1;
    }

    fn cost_percentile_us(&self, p: usize) -> u64 {
        if self.cost_len == 0 {
            return 0;
        }
        let mut v: Vec<u64> = self.cost_ring[..self.cost_len].to_vec();
        v.sort_unstable();
        let idx = ((self.cost_len * p) / 100).min(self.cost_len - 1);
        v[idx]
    }

    /// 99th-percentile SDK submit cost (µs) — the honest "can the submit keep
    /// up?" gauge, `>= interval` (≈ 33_333 µs @30 fps) meaning it cannot. This is
    /// what `iter_p99_us` reports on the paced path once the decode + submit are
    /// both off the emit thread.
    pub fn submit_p99_us(&self) -> u64 {
        self.cost_percentile_us(99)
    }
}

/// The paced submit-call cost gauge (µs) — the worst `send_video_async` call
/// `(max, p99)` drained from the submit thread's `FrameSubmitter.submit_times`
/// (round 3's `SubmitHist`, the SAME instance/drain — no second histogram) over
/// one heartbeat window (#168 round 2). The submit thread folds it worst-of on
/// its ~1 s connection-poll cadence and the heartbeat drains-and-resets it, then
/// it is carried into `PacingStats` + the `pipeline: loop-stats` line so a
/// pacing-ON box test can finally name the per-frame SDK submit cost on the
/// paced path (box test 6 showed 25 ms median / 75 ms p99 per 1440p frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacedSubmitStats {
    pub submit_call_us_max: u64,
    pub submit_call_us_p99: u64,
}

/// Fold one freshly drained `(max, p99)` sub-window (from `SubmitHist::drain`)
/// into the running per-heartbeat gauge, keeping the WORST of each via `.max()`
/// (the heartbeat drains-and-resets `prev`, so the window is one heartbeat — a
/// spike is never diluted by a quiet sub-window). Pure + mutation-tested; a
/// bounded `.max()` fold, never a running `while`.
pub fn paced_submit_snapshot(prev: PacedSubmitStats, max: u64, p99: u64) -> PacedSubmitStats {
    PacedSubmitStats {
        submit_call_us_max: prev.submit_call_us_max.max(max),
        submit_call_us_p99: prev.submit_call_us_p99.max(p99),
    }
}

/// Merge the pacer's SCHEDULING counters with the submit thread's HONEST
/// output-side counters into ONE `PacingStats` for the health doc (#168). The
/// emit thread no longer submits, so `late_frames` / `max_late_us` / `iter_p99_us`
/// come from the submit thread and `dropped` sums both drop kinds
/// (decode-decimation + handoff-coalesce); the `submit_call_us_*` gauge (#168
/// round 2) is the paced submit thread's drained `FrameSubmitter.submit_times`;
/// `seq` / `jitter` / `repeats` / `resyncs` / `relatches` / `lag_slots` /
/// `prep_p99_us` / `enabled` stay the pacer's.
pub fn merge_pacing_stats(
    pacer: PacingStats,
    submit: &SubmitCounters,
    paced_submit: PacedSubmitStats,
) -> PacingStats {
    let dropped = pacer.dropped + submit.dropped;
    PacingStats {
        late_frames: submit.late_frames,
        max_late_us: submit.max_late_us,
        iter_p99_us: submit.submit_p99_us(),
        dropped,
        submit_call_us_max: paced_submit.submit_call_us_max,
        submit_call_us_p99: paced_submit.submit_call_us_p99,
        ..pacer
    }
}

#[cfg(test)]
#[path = "submit_handoff_tests.rs"]
mod tests;
