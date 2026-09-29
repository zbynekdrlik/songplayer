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
//!   following resamples, unless [`decide_anchor_step`] sees the same step
//!   confirmed by two narrow resamples: then it is followed in ONE event, in
//!   either direction.
//!   [`apply_anchor_step`] applies a BACKWARD correction as a hold, never as a
//!   backward step. A followed backward step is ONE hold of its remaining size.
//!
//! A dantesync date step is followed at the boundary it lands (#224, design
//! record 5890605448): [`decide_step_probe`] judges one cheap bracketed read
//! per boundary against the line the wall runs on. Over 2 ms
//! ([`STEP_DETECT_100NS`]) from a narrow bracket it takes a full anchor sample
//! at once and, when both agree within 1 ms, follows the step in ONE event.
//! The 100-frame resample above keeps slewing everything smaller.
//!
//! Everything here is pure (no clock reads), so the tests inject reads.
//!
//! [`WallClock`]: super::WallClock

use std::time::{Duration, Instant};

/// Most bracketed reads one anchor sample takes (#147 design).
pub const ANCHOR_MAX_ATTEMPTS: usize = 8;

/// A bracket this narrow bounds the pairing error to ≤ 10 µs, so sampling stops
/// early. A clean read on the box brackets well under 1 µs, so the normal cost
/// is one read.
pub const ANCHOR_TIGHT_BRACKET: Duration = Duration::from_micros(20);

/// A chosen bracket wider than this means every attempt was disturbed (heavy
/// preemption). It is still used, because it is the best available, but it is
/// counted in [`WallAnchorStats::wide_brackets`].
pub const ANCHOR_WIDE_BRACKET: Duration = Duration::from_micros(200);

/// The most one resample may move the wall: 1 ms in 100-ns units. Normal
/// dantesync slewing is ≤ ~313 µs per ~3.3 s resample (≤ 94 ppm).
pub const ANCHOR_MAX_STEP_100NS: i64 = 10_000;

/// A UTC offset the per-boundary probe treats as a STEP (#224): 2 ms in
/// 100-ns units, the fleet receivers' wall-step threshold (camera-box sender
/// contract, issue 1294). dantesync slewing (≤ 94 ppm) moves ≤ ~313 µs per
/// 100-frame resample, far below it, and stays on the ±1 ms resample bound.
pub const STEP_DETECT_100NS: i64 = 20_000;

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

/// Which way a UTC step moves the wall (#147). A FORWARD step (UTC ahead of
/// the wall) is followed as one step ahead; a BACKWARD step (UTC behind the
/// wall) is followed as one HOLD, because the wall never goes backward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepDirection {
    Forward,
    Backward,
}

impl StepDirection {
    /// The direction of a measured `delta_100ns` (`sample.utc − wall`):
    /// forward when positive. Only a clamped (|delta| > 1 ms) delta is ever
    /// classified, so 0 never reaches it.
    pub fn of(delta_100ns: i64) -> Self {
        if delta_100ns.is_positive() {
            Self::Forward
        } else {
            Self::Backward
        }
    }

    /// The `direction=` value of the follow log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }
}

/// A clamped resample from a narrow bracket, waiting for the next resample to
/// confirm it (#147 confirmed date step): the delta it measured, the ≤ 1 ms it
/// already applied (a step ahead, or a hold), and its direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingStep {
    pub delta_100ns: i64,
    pub applied_100ns: i64,
    pub direction: StepDirection,
}

/// A confirmed step followed in one re-anchor (#147).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowedStep {
    /// The whole step: the arming resample's applied part plus the rest
    /// applied now. Signed, 100-ns units (negative = backward).
    pub total_100ns: i64,
    /// Forward (one step ahead) or backward (one hold).
    pub direction: StepDirection,
}

/// What one resample does to the wall under the confirm-then-follow rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorDecision {
    /// The correction applied now. For a followed step it is the whole rest:
    /// a step ahead, or ONE hold of `|applied|` when backward.
    pub step: AnchorStep,
    /// `Some` when this resample FOLLOWED a confirmed step.
    pub followed: Option<FollowedStep>,
    /// The step the NEXT resample may confirm.
    pub pending: Option<PendingStep>,
}

/// The confirm-then-follow anchor rule (#147, design records 5845527884 and
/// 5850063723), ONE rule for both directions.
///
/// dantesync steps the fleet date (a coordinated step, since 1.12.0 once a
/// night at 04:00 of up to ~±1.5 s), and every camera-box sender follows
/// `CLOCK_REALTIME` at once. Slewing that step at 1 ms per resample kept our
/// stamps ~300 ppm off the fleet: ~2.7 min for +50 ms, ~83 min for −1.5 s. So:
///
/// - A resample measuring a `delta` over 1 ms either way from a narrow bracket
///   (`narrow` = not wider than [`ANCHOR_WIDE_BRACKET`]) applies the bounded
///   1 ms (a step ahead, or a 1 ms hold) and ARMS the step ([`PendingStep`]).
/// - The NEXT resample follows the rest of it in ONE event when it is narrow
///   too, still over 1 ms, and `delta + applied₁` lies within ±1 ms of the
///   armed `delta₁`: the same step, seen twice. Forward, the wall steps ahead
///   by the rest. Backward, [`apply_anchor_step`] turns the rest into ONE hold:
///   the wall freezes for `|delta|`, then runs on the corrected UTC line.
///   The tolerance alone already implies the same direction: an armed step
///   is over 1 ms, so a reading within ±1 ms of it that is itself over 1 ms
///   has the same sign.
/// - Everything else is the plain [`bounded_anchor_update`]: a lone outlier
///   moves the wall 1 ms (a step, or a hold) and the next read takes it back
///   out, and a wide (preempted) sample never arms or confirms.
pub fn decide_anchor_step(
    pending: Option<PendingStep>,
    delta_100ns: i64,
    narrow: bool,
) -> AnchorDecision {
    let step = bounded_anchor_update(delta_100ns);
    // A clamp either way arms and follows; the tolerance picks the same step.
    let armable = narrow && step.is_clamped();
    let confirms = |p: &PendingStep| same_step(p.delta_100ns, p.applied_100ns, delta_100ns);
    if armable && let Some(p) = pending.filter(confirms) {
        return AnchorDecision {
            step: AnchorStep {
                applied_100ns: delta_100ns,
                carry_100ns: 0,
            },
            followed: Some(FollowedStep {
                total_100ns: p.applied_100ns + delta_100ns,
                direction: p.direction,
            }),
            pending: None,
        };
    }
    let pending = armable.then_some(PendingStep {
        delta_100ns,
        applied_100ns: step.applied_100ns,
        direction: StepDirection::of(delta_100ns),
    });
    AnchorDecision {
        step,
        followed: None,
        pending,
    }
}

/// Whether `delta_100ns` sees the same step as an earlier reading
/// `armed_delta_100ns` of which `applied_100ns` is already applied: the rest
/// plus what was applied lies within ±1 ms ([`ANCHOR_MAX_STEP_100NS`]) of the
/// earlier reading. ONE rule for the resample's confirmation
/// ([`decide_anchor_step`]) and the probe's ([`decide_step_probe`]). Within the
/// tolerance two readings of a step over 1 ms have the same sign, so the rule
/// never compares directions.
fn same_step(armed_delta_100ns: i64, applied_100ns: i64, delta_100ns: i64) -> bool {
    (delta_100ns + applied_100ns - armed_delta_100ns).abs() <= ANCHOR_MAX_STEP_100NS
}

/// A monotonic→UTC anchor (#224 names the pair [`WallClock`] reads through):
/// the wall reads `utc_100ns` at `instant` and runs with the monotonic clock
/// from there. An `instant` still in the future is a HOLD in progress, and
/// the wall reads `utc_100ns` until then.
///
/// [`WallClock`]: super::WallClock
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub instant: Instant,
    pub utc_100ns: i64,
}

impl Anchor {
    /// What the wall reads at `at`: frozen at `utc_100ns` before `instant`
    /// ([`wall_at`]).
    pub fn wall_at(&self, at: Instant) -> i64 {
        wall_at(self.instant, self.utc_100ns, at)
    }

    /// The UTC line the wall runs on, also through a hold in progress: before
    /// `instant` it reads BELOW `utc_100ns` by the time left to `instant`.
    /// The step probe measures against it, so a hold the wall is still taking
    /// is never read as a new step.
    pub fn line_at(&self, at: Instant) -> i64 {
        match at.checked_duration_since(self.instant) {
            Some(after) => self.utc_100ns.saturating_add(to_100ns(after)),
            None => self
                .utc_100ns
                .saturating_sub(to_100ns(self.instant.duration_since(at))),
        }
    }
}

/// A duration in 100-ns units (truncating), saturating at `i64::MAX`.
fn to_100ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX)
}

/// A step the per-boundary probe confirmed (#224), to be applied at the
/// confirming `sample`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeFollow {
    /// The confirming anchor sample. The new anchor is set at its instant.
    pub sample: AnchorSample,
    /// The step the probe measured against the line.
    pub probe_delta_100ns: i64,
    /// The step the confirming sample measured against the line.
    pub delta_100ns: i64,
    /// What the wall reads at `sample.instant` (frozen during a hold).
    pub wall_100ns: i64,
    /// Applied now, whole (`sample.utc − wall`): a step ahead, or ONE hold
    /// ([`apply_anchor_step`]).
    pub step: AnchorStep,
    /// The step followed. Its total includes the 1 ms a resample already
    /// applied when that resample armed this same step.
    pub followed: FollowedStep,
}

/// What the per-boundary step probe decided (#224).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeDecision {
    /// Within ±2 ms of the line: nothing to follow. The 100-frame resample
    /// slews it (normal dantesync slewing, or what is left of a small step).
    Quiet,
    /// Over 2 ms, but the probe's own bracket was wide (a read preempted
    /// between its monotonic and its UTC half): rejected by width. Nothing is
    /// armed, so the next boundary's probe follows a real step at once.
    RejectedWide { delta_100ns: i64, bracket: Duration },
    /// Over 2 ms from a narrow probe, but the confirming sample was wide or
    /// measured another step (not within ±1 ms): rejected, nothing armed.
    Unconfirmed {
        delta_100ns: i64,
        confirm: AnchorSample,
        confirm_delta_100ns: i64,
    },
    /// Confirmed: follow the step in ONE event.
    Follow(ProbeFollow),
}

/// The per-boundary step probe (#224, design record 5890605448): one
/// bracketed `probe` read against the line the wall runs on (`anchor`).
///
/// - Within ±2 ms ([`STEP_DETECT_100NS`]) it is [`ProbeDecision::Quiet`]:
///   normal slewing stays on the 100-frame resample and its ±1 ms bound. A
///   `pending` step the resample armed and the probe sees again counts its
///   applied 1 ms: the probe reads only the rest, the step is the whole.
/// - Over 2 ms from a bracket wider than [`ANCHOR_WIDE_BRACKET`] it is
///   rejected by width. A preempted read only ever errs by half its bracket,
///   so a narrow probe cannot fake a 2 ms step.
/// - Over 2 ms from a narrow probe, `confirm` takes a full anchor sample (the
///   best-of-N bracketed read) in the same tick. It confirms when it is narrow
///   too and within ±1 ms of the probe (`same_step`): the step is then
///   followed in ONE event at that sample, a step ahead when forward and ONE
///   hold when backward, like the resample's confirmed follow. A `pending`
///   step the resample armed with its bounded 1 ms counts into the total.
///
/// `confirm` runs only on that path, so a quiet boundary costs one read.
pub fn decide_step_probe<F: FnOnce() -> AnchorSample>(
    anchor: Anchor,
    pending: Option<PendingStep>,
    probe: BracketedRead,
    confirm: F,
) -> ProbeDecision {
    // The part of this step a resample already applied (it armed it with its
    // bounded 1 ms), when `delta` reads the same step's rest.
    let armed = |delta_100ns: i64| {
        pending
            .filter(|p| same_step(p.delta_100ns, p.applied_100ns, delta_100ns))
            .map_or(0, |p| p.applied_100ns)
    };
    let delta = probe
        .utc_100ns
        .saturating_sub(anchor.line_at(probe.midpoint()));
    if (delta + armed(delta)).abs() <= STEP_DETECT_100NS {
        return ProbeDecision::Quiet;
    }
    if probe.width() > ANCHOR_WIDE_BRACKET {
        return ProbeDecision::RejectedWide {
            delta_100ns: delta,
            bracket: probe.width(),
        };
    }
    let sample = confirm();
    let confirm_delta = sample
        .utc_100ns
        .saturating_sub(anchor.line_at(sample.instant));
    if sample.is_wide() || !same_step(delta, 0, confirm_delta) {
        return ProbeDecision::Unconfirmed {
            delta_100ns: delta,
            confirm: sample,
            confirm_delta_100ns: confirm_delta,
        };
    }
    let wall = anchor.wall_at(sample.instant);
    ProbeDecision::Follow(ProbeFollow {
        sample,
        probe_delta_100ns: delta,
        delta_100ns: confirm_delta,
        wall_100ns: wall,
        step: AnchorStep {
            applied_100ns: sample.utc_100ns.saturating_sub(wall),
            carry_100ns: 0,
        },
        followed: FollowedStep {
            total_100ns: armed(confirm_delta) + confirm_delta,
            direction: StepDirection::of(confirm_delta),
        },
    })
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
/// * Forward (`applied ≥ 0`): the wall steps ahead by `applied` (≤ 1 ms, or a
///   whole confirmed step).
/// * Backward (`applied < 0`): the anchor instant moves `|applied|` into the
///   future at the CURRENT wall value. The saturating read then holds the wall
///   for `|applied|` — ≤ 1 ms, or the whole rest of a confirmed backward step
///   (~1.5 s for a dantesync nightly date step), in ONE hold — after which it
///   runs exactly on the corrected line. The wall never goes backward, so the
///   pacer can never re-serve a boundary it already emitted (the relatch the
///   box showed).
pub fn apply_anchor_step(at: Instant, wall_100ns: i64, applied_100ns: i64) -> (Instant, i64) {
    // At most one of the two is non-zero: a forward step, or a backward hold.
    let forward_100ns = applied_100ns.max(0);
    let hold_100ns = applied_100ns.min(0).unsigned_abs();
    let hold = Duration::from_nanos(hold_100ns.saturating_mul(100));
    (at + hold, wall_100ns.saturating_add(forward_100ns))
}

/// Anchor telemetry of one [`WallClock`](super::WallClock). It is surfaced on
/// `PacingStats` as `wall_anchor_*` and on the per-minute `ndi: genlock` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallAnchorStats {
    /// Largest |delta| (µs) a resample or a confirmed step probe MEASURED:
    /// the step an unbounded re-anchor would have taken. A resample applies
    /// at most 1 ms of it.
    pub max_step_us: u64,
    /// Anchor samples (construction, resamples and the step probe's
    /// confirming samples) whose chosen bracket was wider than
    /// [`ANCHOR_WIDE_BRACKET`].
    pub wide_brackets: u64,
    /// Cumulative correction (µs) applied through CLAMPED resamples (|delta| >
    /// 1 ms), i.e. slewed in rather than stepped.
    pub slewed_us: u64,
    /// Confirmed UTC steps followed in ONE event, both directions: by the
    /// step probe at the boundary the step lands (#224), or by a second
    /// resample (#147).
    pub steps_followed: u64,
    /// The total step (µs) of the last followed step, signed (negative =
    /// backward); 0 before any.
    pub last_step_us: i64,
    /// Confirmed BACKWARD steps followed as ONE hold (#147), a subset of
    /// `steps_followed`.
    pub holds_followed: u64,
    /// How long (µs) the last followed hold froze the wall from the follow:
    /// the whole step when the probe follows it (also right after a resample
    /// armed it with a 1 ms hold, which the follow's hold covers); the step
    /// minus the elapsed arming 1 ms on the resample path; 0 before any.
    pub last_hold_us: u64,
}

/// The per-boundary step probe's own telemetry (#224). Surfaced on
/// `PacingStats` as `wall_anchor_probes_rejected` and
/// `wall_anchor_detect_to_follow_us`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepProbeStats {
    /// Probes over 2 ms that were rejected: a wide probe, or a confirming
    /// sample that was wide or measured another step. Cumulative.
    pub rejected: u64,
    /// Monotonic time (µs) from the first over-2 ms probe of the last
    /// followed step to its follow (by the probe, or by a resample after every
    /// probe was rejected). About the width of two clock reads when the first
    /// probe confirms at once; one boundary per rejected probe before it; 0
    /// when no over-2 ms probe preceded the follow, and before any.
    pub last_detect_to_follow_us: u64,
}

impl WallAnchorStats {
    /// Record one anchor sample's bracket.
    pub fn record_sample(&mut self, sample: &AnchorSample) {
        if sample.is_wide() {
            self.wide_brackets += 1;
        }
    }

    /// Record a `followed` step whose rest was applied now as `step`: a step
    /// ahead, or (backward) one hold of `|step.applied_100ns|`.
    pub fn record_follow(&mut self, followed: &FollowedStep, step: &AnchorStep) {
        self.steps_followed += 1;
        self.last_step_us = to_us(followed.total_100ns);
        if followed.direction == StepDirection::Backward {
            self.holds_followed += 1;
            self.last_hold_us = step.applied_100ns.unsigned_abs() / 10;
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
