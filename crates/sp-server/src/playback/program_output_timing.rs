//! #210: how late the `SP-program` sender served each program boundary, per
//! stage — the telemetry that names the stage when the FOH VBAN feed is late.
//!
//! The sender (`program_output::ProgramOutput::serve`) reads four instants
//! per boundary off its own wall, the stamps' timeline ([`BoundaryMarks`]):
//! the job taken from the bus, its audio block handed to VBAN, and the NDI
//! submit started and returned. [`BoundarySample::of`] turns them into three
//! µs figures:
//!
//! - `ready_late_us`: the job taken vs the boundary instant (a late source, a
//!   late release, or the sender still busy with the boundary before);
//! - `vban_feed_late_us`: the VBAN hand-off vs the boundary instant (what
//!   FOH hears — the VBAN thread sends a block's first packet two slots
//!   after its boundary);
//! - `submit_us`: the NDI submit call alone.
//!
//! [`BoundaryTiming`] (pure, Linux-tested, the `loop_stats.rs` pattern)
//! keeps each figure's worst over the last 60–120 s (two buckets of
//! [`TIMING_BUCKET_BOUNDARIES`]), counts per figure the boundaries over
//! [`STAGE_SLOW_US`] since start, and decides the ONE WARN for a boundary
//! whose VBAN hand-off was over [`VBAN_FEED_WARN_US`] late: at most one per
//! [`TIMING_WARN_EVERY_100NS`] of timeline, the next one carrying how many it
//! skipped. Served as `health.timing` on `GET /api/v1/program`
//! ([`BoundaryTimingStatus`]).

use serde::Serialize;
use sp_core::genlock::UNITS_PER_SECOND;

/// A figure over this (µs) is counted in its `*_over_5ms` total.
pub const STAGE_SLOW_US: u64 = 5_000;

/// A boundary whose VBAN hand-off is later than this (µs) is WARNed.
pub const VBAN_FEED_WARN_US: u64 = 10_000;

/// At most one WARN per this much timeline (100 ns; 5 s): a bad minute
/// writes at most 12 lines. The 10 s grid of the measured stalls is never
/// thinned by it.
pub const TIMING_WARN_EVERY_100NS: i64 = 50_000_000;

/// Boundaries per window bucket (60 s at the 30 fps grid).
pub const TIMING_BUCKET_BOUNDARIES: u32 = 1_800;

/// The instants (timeline, 100 ns, the sender's wall) one program boundary
/// was served at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundaryMarks {
    /// The boundary (the job's stamp).
    pub stamp_100ns: i64,
    /// The job was taken from the bus.
    pub taken_100ns: i64,
    /// Its audio block was handed to VBAN.
    pub fed_100ns: i64,
    /// Its NDI submit started.
    pub submit_start_100ns: i64,
    /// Its NDI submit returned.
    pub submitted_100ns: i64,
}

/// One boundary's figures, µs. An instant before its reference is 0 late.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundarySample {
    pub ready_late_us: u64,
    pub vban_feed_late_us: u64,
    pub submit_us: u64,
}

impl BoundarySample {
    /// The figures of the boundary served at `m`.
    pub fn of(m: &BoundaryMarks) -> Self {
        Self {
            ready_late_us: us_after(m.stamp_100ns, m.taken_100ns),
            vban_feed_late_us: us_after(m.stamp_100ns, m.fed_100ns),
            submit_us: us_after(m.submit_start_100ns, m.submitted_100ns),
        }
    }

    /// Each figure's worst of `self` and `other`.
    pub fn worst(self, other: Self) -> Self {
        Self {
            ready_late_us: self.ready_late_us.max(other.ready_late_us),
            vban_feed_late_us: self.vban_feed_late_us.max(other.vban_feed_late_us),
            submit_us: self.submit_us.max(other.submit_us),
        }
    }
}

/// Whole µs from `from_100ns` to `to_100ns`; 0 when `to` is not after `from`.
pub fn us_after(from_100ns: i64, to_100ns: i64) -> u64 {
    u64::try_from(to_100ns.saturating_sub(from_100ns) / 10).unwrap_or(0)
}

/// A boundary to WARN about: its VBAN hand-off was over
/// [`VBAN_FEED_WARN_US`] late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LateBoundary {
    /// The boundary (timeline, 100 ns).
    pub stamp_100ns: i64,
    pub sample: BoundarySample,
    /// Boundaries over [`VBAN_FEED_WARN_US`] the rate limit skipped since
    /// the WARN before this one.
    pub suppressed: u64,
}

/// `GET /api/v1/program` → `health.timing`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BoundaryTimingStatus {
    /// Boundaries measured since start.
    pub boundaries: u64,
    /// Each figure's worst over the last 60–120 s (µs).
    pub ready_late_us_max: u64,
    pub vban_feed_late_us_max: u64,
    pub submit_us_max: u64,
    /// Boundaries whose figure was over 5 ms, since start.
    pub ready_late_over_5ms: u64,
    pub vban_feed_late_over_5ms: u64,
    pub submit_over_5ms: u64,
    /// Boundaries whose VBAN hand-off was over 10 ms late, since start.
    pub vban_feed_late_over_10ms: u64,
    /// WARN lines written for them (the rest were rate-limited).
    pub warned: u64,
}

/// The `SP-program` sender's per-boundary timing window.
#[derive(Debug, Default)]
pub struct BoundaryTiming {
    boundaries: u64,
    /// The worst of the bucket being filled and of the last full one.
    current: BoundarySample,
    previous: BoundarySample,
    /// Boundaries in `current`.
    in_current: u32,
    ready_late_over_5ms: u64,
    vban_feed_late_over_5ms: u64,
    submit_over_5ms: u64,
    vban_feed_late_over_10ms: u64,
    warned: u64,
    /// The boundary of the last WARN; `None` before the first.
    last_warn_100ns: Option<i64>,
    /// Boundaries over 10 ms skipped by the rate limit since the last WARN.
    suppressed: u64,
}

impl BoundaryTiming {
    /// Fold in one boundary served at `marks`. Returns it when it is to be
    /// WARNed: its VBAN hand-off was over [`VBAN_FEED_WARN_US`] late and no
    /// WARN went out in the [`TIMING_WARN_EVERY_100NS`] before it.
    pub fn observe(&mut self, marks: &BoundaryMarks) -> Option<LateBoundary> {
        let sample = BoundarySample::of(marks);
        self.boundaries += 1;
        self.current = self.current.worst(sample);
        self.in_current += 1;
        if self.in_current == TIMING_BUCKET_BOUNDARIES {
            self.previous = std::mem::take(&mut self.current);
            self.in_current = 0;
        }
        self.ready_late_over_5ms += u64::from(sample.ready_late_us > STAGE_SLOW_US);
        self.vban_feed_late_over_5ms += u64::from(sample.vban_feed_late_us > STAGE_SLOW_US);
        self.submit_over_5ms += u64::from(sample.submit_us > STAGE_SLOW_US);
        if sample.vban_feed_late_us <= VBAN_FEED_WARN_US {
            return None;
        }
        self.vban_feed_late_over_10ms += 1;
        let stamp_100ns = marks.stamp_100ns;
        let quiet = self
            .last_warn_100ns
            .is_none_or(|last| stamp_100ns >= last + TIMING_WARN_EVERY_100NS);
        if !quiet {
            self.suppressed += 1;
            return None;
        }
        self.warned += 1;
        self.last_warn_100ns = Some(stamp_100ns);
        Some(LateBoundary {
            stamp_100ns,
            sample,
            suppressed: std::mem::take(&mut self.suppressed),
        })
    }

    /// The telemetry for the API.
    pub fn status(&self) -> BoundaryTimingStatus {
        let worst = self.current.worst(self.previous);
        BoundaryTimingStatus {
            boundaries: self.boundaries,
            ready_late_us_max: worst.ready_late_us,
            vban_feed_late_us_max: worst.vban_feed_late_us,
            submit_us_max: worst.submit_us,
            ready_late_over_5ms: self.ready_late_over_5ms,
            vban_feed_late_over_5ms: self.vban_feed_late_over_5ms,
            submit_over_5ms: self.submit_over_5ms,
            vban_feed_late_over_10ms: self.vban_feed_late_over_10ms,
            warned: self.warned,
        }
    }
}

/// An instant in 100 ns since the Unix epoch (a wire stamp) as UTC with
/// milliseconds, for the WARN line next to a dev1 capture. Every `i64` of
/// 100 ns is inside chrono's range; the empty string is never reached.
pub fn utc_label(t_100ns: i64) -> String {
    let secs = t_100ns.div_euclid(UNITS_PER_SECOND);
    let nanos = u32::try_from(t_100ns.rem_euclid(UNITS_PER_SECOND) * 100).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "program_output_timing_tests.rs"]
mod tests;
