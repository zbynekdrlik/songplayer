//! Boundary-paced emission telemetry ([`PacingStats`], #147), split out of
//! `ndi_health.rs` to keep that file under the 1000-line cap. Re-exported from
//! `ndi_health`, so every `ndi_health::PacingStats` path stays valid.

use serde::{Deserialize, Serialize};

/// Boundary-paced emission telemetry (#147), surfaced on
/// `GET /api/v1/ndi/health` as `pacing`. `enabled=false` + all-zero is what an
/// SDK-clocked (flag-OFF) or idle pipeline reports; the `Pacer`
/// (`playback/pacer.rs`) fills real values on the paced path.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PacingStats {
    /// Whether boundary-paced emission is active for this pipeline.
    pub enabled: bool,
    /// Monotonic count of boundaries serviced (one frame emitted per boundary).
    pub seq: u64,
    /// Emits that landed a full interval or more past their boundary.
    pub late_frames: u64,
    /// Worst emit lateness observed (µs).
    pub max_late_us: u64,
    /// 99th-percentile emit jitter (emit − boundary, µs) over the recent window.
    pub jitter_p99_us: u64,
    /// Last-frame repeats emitted on decoder underrun (one per starved boundary).
    pub repeats: u64,
    /// Grid resyncs — a lag beyond the catch-up bound with nothing buffered.
    pub resyncs: u64,
    /// Backward-clock-step re-latches.
    pub relatches: u64,
    /// Decoded frames dropped as older-than-boundary (e.g. 60→30 decimation).
    pub dropped: u64,
    /// Current lag at the last emit: whole grid slots the serviced boundary sat
    /// behind `floor(now)` (#147 lane 3). A slow file decoder (`iter_cost >=
    /// interval`) drives this up until the playback re-anchor bounds it; a
    /// growing `lag_slots` is the direct signal of the box-test-1 failure, where
    /// `jitter_p99_us` (which measures emit − boundary lateness) merely mirrored
    /// it. Signed because it is a gauge, always `>= 0` in practice.
    pub lag_slots: i64,
    /// 99th-percentile per-iteration decode+submit cost (µs) over the recent
    /// window (#147 lane 3). `iter_cost >= interval` (≈ 33_333 µs @30 fps) is the
    /// condition under which catch-up can never gain on the wall clock, so this
    /// is the honest "can the decoder keep up?" signal, distinct from
    /// `jitter_p99_us` (emit − boundary lateness).
    pub iter_p99_us: u64,
    /// 99th-percentile pre-decode (`prepare`) duration (µs) over the recent
    /// window (#147 lane 4). Decode moved AHEAD of the boundary — `prepare`
    /// decodes the next due frame right after each emit, off the critical path,
    /// so `late_frames` collapses to ~0 while this gauges whether the decoder can
    /// still produce a frame inside one slot. `prep_p99_us >= interval`
    /// (≈ 33_333 µs @30 fps) means it cannot keep up and lag will grow until the
    /// re-anchor bounds it (the same signal `iter_p99_us` was, now measured where
    /// the decode actually happens).
    pub prep_p99_us: u64,
    pub submit_call_us_max: u64,
    pub submit_call_us_p99: u64,
    /// #148: audio − picture media offset (ms) at the last paced boundary, before
    /// its correction; + = audio ahead.
    pub av_align_err_ms: f64,
    /// #148: boundaries whose audio was dropped/padded onto the picture (cumulative).
    pub av_corrections: u64,
    /// #148: samples dropped + padded by that alignment (cumulative).
    pub av_corrected_samples: u64,
    /// #147: largest monotonic↔UTC re-anchor delta (µs) the pacer's wall clock
    /// MEASURED, i.e. the step an unbounded re-anchor would have taken. The
    /// applied step is capped at 1 ms (`WallAnchorStats::max_step_us`).
    pub wall_anchor_max_step_us: u64,
    /// #147: anchor samples whose best bracket was still wider than 200 µs
    /// (every attempt preempted), cumulative.
    pub wall_anchor_wide_brackets: u64,
    /// #147: correction (µs) slewed in through clamped (> 1 ms) re-anchors
    /// instead of stepped, cumulative.
    pub wall_anchor_slewed_us: u64,
}
