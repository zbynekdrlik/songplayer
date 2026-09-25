//! Anchor sampling and the bounded anchor update for [`WallClock`] (#147: the
//! re-anchor must not step the genlock wall).
//!
//! The monotonic→UTC anchor used to be one unbracketed pair
//! `(Instant::now(), Utc::now())`. A preemption between the two reads paired a
//! stale `Instant` with a later UTC read, so the wall jumped FORWARD by the
//! preemption time. The next clean re-anchor then jumped it BACK by the same
//! amount, and the pacer relatched. Two pure pieces fix it:
//!
//! * [`choose_bracketed_sample`] reads `m1 = Instant::now(); u = utc; m2 =
//!   Instant::now()` up to [`ANCHOR_MAX_ATTEMPTS`] times and keeps the NARROWEST
//!   bracket, pairing `u` with the bracket MIDPOINT. The pairing error is at
//!   most half the bracket, so a preempted read is simply outvoted.
//! * [`bounded_anchor_update`] caps what one resample may change the wall by, at
//!   [`ANCHOR_MAX_STEP_100NS`] (1 ms). A larger correction is slewed in over the
//!   following resamples.
//!   [`apply_anchor_step`] applies a BACKWARD correction as a short hold, never
//!   as a backward step.
//!
//! Everything here is pure (no clock reads), so the tests inject reads.
//!
//! [`WallClock`]: super::WallClock

use std::time::{Duration, Instant};

/// Most bracketed reads one anchor sample takes (#147 design).
pub const ANCHOR_MAX_ATTEMPTS: usize = 1;

/// A bracket this narrow bounds the pairing error to ≤ 10 µs, so sampling stops
/// early. A clean read on the box brackets well under 1 µs, so the normal cost
/// is one read.
pub const ANCHOR_TIGHT_BRACKET: Duration = Duration::from_micros(20);

/// A chosen bracket wider than this means every attempt was disturbed (heavy
/// preemption). It is still used, because it is the best available, but it is
/// counted in [`WallAnchorStats::wide_brackets`].
pub const ANCHOR_WIDE_BRACKET: Duration = Duration::from_micros(200);

/// The most one resample may move the wall: 1 ms in 100-ns units. Normal
/// dantesync slewing is ≤ 100 µs per ~3.3 s resample (30 ppm).
pub const ANCHOR_MAX_STEP_100NS: i64 = i64::MAX;

/// One bracketed read of the realtime clock: `m1` is the monotonic clock read
/// just before the UTC read, `m2` the one just after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketedRead {
    pub m1: Instant,
    pub utc_100ns: i64,
    pub m2: Instant,
}

impl BracketedRead {
    /// The bracket width `m2 − m1`. The UTC read happened somewhere inside it.
    pub fn width(&self) -> Duration {
        self.m2.saturating_duration_since(self.m1)
    }

    /// The bracket midpoint. Pairing the UTC read with it errs by at most
    /// `width / 2`.
    pub fn midpoint(&self) -> Instant {
        self.m1 + self.width() / 2
    }
}

/// The monotonic↔UTC pair an anchor is set from, plus the bracket it came
/// from (for the wide-bracket counter and the re-anchor log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorSample {
    pub instant: Instant,
    pub utc_100ns: i64,
    pub bracket: Duration,
}

impl AnchorSample {
    /// Whether the chosen bracket is wider than [`ANCHOR_WIDE_BRACKET`].
    pub fn is_wide(&self) -> bool {
        self.bracket > ANCHOR_WIDE_BRACKET
    }
}

/// Take up to [`ANCHOR_MAX_ATTEMPTS`] bracketed reads from `read` and keep the
/// narrowest. Stop early once a bracket is within [`ANCHOR_TIGHT_BRACKET`].
/// On a tie the earlier read wins. The UTC read is paired with that bracket's
/// midpoint.
pub fn choose_bracketed_sample<F: FnMut() -> BracketedRead>(mut read: F) -> AnchorSample {
    let mut best = read();
    let mut attempts = 1;
    while attempts < ANCHOR_MAX_ATTEMPTS && best.width() > ANCHOR_TIGHT_BRACKET {
        let next = read();
        attempts += 1;
        if next.width() < best.width() {
            best = next;
        }
    }
    AnchorSample {
        instant: best.midpoint(),
        utc_100ns: best.utc_100ns,
        bracket: best.width(),
    }
}

/// What one resample applies to the wall, and what it leaves for later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorStep {
    /// Correction applied now, within `±ANCHOR_MAX_STEP_100NS`.
    pub applied_100ns: i64,
    /// Remainder not applied (0 when `|delta| ≤ 1 ms`). It is NOT carried
    /// explicitly: the next resample re-measures the full remaining offset, so
    /// this remainder is already inside its `delta`. It is reported for the
    /// log only.
    pub carry_100ns: i64,
}

impl AnchorStep {
    /// Whether the bound engaged (`|delta| > 1 ms`). The resample is then logged
    /// and counted as slewed.
    pub fn is_clamped(&self) -> bool {
        self.carry_100ns != 0
    }
}

/// 100-ns units → whole µs (truncating toward zero), for the re-anchor log.
pub fn to_us(v_100ns: i64) -> i64 {
    v_100ns / 10
}

/// Bound one resample's anchor correction `delta_100ns` (the new sample's UTC
/// minus the wall this clock shows at that instant). Within ±1 ms it applies
/// as-is. Beyond that only ±1 ms applies now and the rest is slewed in by the
/// following resamples (≤ 1 ms per ~3.3 s ≈ 300 ppm), so a genuine UTC step
/// is followed without ever stepping the wall.
pub fn bounded_anchor_update(delta_100ns: i64) -> AnchorStep {
    let applied = delta_100ns.clamp(-ANCHOR_MAX_STEP_100NS, ANCHOR_MAX_STEP_100NS);
    AnchorStep {
        applied_100ns: applied,
        carry_100ns: delta_100ns - applied,
    }
}

/// The wall reading of an anchor at monotonic instant `at`:
/// `anchor_utc + (at − anchor_instant)`, with an instant before the anchor
/// reading as the anchor itself (saturating). This is exactly the default
/// `ClockSource::read_100ns` formula.
pub fn wall_at(anchor_instant: Instant, anchor_utc_100ns: i64, at: Instant) -> i64 {
    let elapsed = at.saturating_duration_since(anchor_instant);
    anchor_utc_100ns.saturating_add((elapsed.as_nanos() / 100) as i64)
}

/// The new `(anchor_instant, anchor_utc_100ns)` after applying `applied_100ns`
/// at monotonic instant `at`, where the current wall reads `wall_100ns`.
///
/// * Forward (`applied ≥ 0`): the wall steps ahead by `applied` (≤ 1 ms).
/// * Backward (`applied < 0`): the anchor instant moves `|applied|` into the
///   future at the CURRENT wall value. The saturating read then holds the wall
///   for `|applied|` (≤ 1 ms), after which it runs exactly on the corrected
///   line. The wall never goes backward, so the pacer can never re-serve a
///   boundary it already emitted (the relatch the box showed).
pub fn apply_anchor_step(at: Instant, wall_100ns: i64, applied_100ns: i64) -> (Instant, i64) {
    (at, wall_100ns + applied_100ns)
}

/// Anchor telemetry of one [`WallClock`](super::WallClock). It is surfaced on
/// `PacingStats` as `wall_anchor_*` and on the per-minute `ndi: genlock` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallAnchorStats {
    /// Largest |delta| (µs) a resample MEASURED: the step an unbounded
    /// re-anchor would have taken. The applied step is capped at 1 ms.
    pub max_step_us: u64,
    /// Anchor samples (construction + resamples) whose chosen bracket was wider
    /// than [`ANCHOR_WIDE_BRACKET`].
    pub wide_brackets: u64,
    /// Cumulative correction (µs) applied through CLAMPED resamples (|delta| >
    /// 1 ms), i.e. slewed in rather than stepped.
    pub slewed_us: u64,
}

impl WallAnchorStats {
    /// Record one anchor sample's bracket.
    pub fn record_sample(&mut self, sample: &AnchorSample) {
        if sample.is_wide() {
            self.wide_brackets += 1;
        }
    }

    /// Record one resample's measured `delta_100ns` and its bounded `step`.
    pub fn record_step(&mut self, delta_100ns: i64, step: &AnchorStep) {
        self.max_step_us = self.max_step_us.max(delta_100ns.unsigned_abs() / 10);
        if step.is_clamped() {
            self.slewed_us += step.applied_100ns.unsigned_abs() / 10;
        }
    }
}
