//! Monotonic-to-UTC wall clock for genlocked NDI timecodes (#146).
//!
//! The genlock contract (camera-box#1294 §1) takes timecode stamps from the
//! realtime clock (dantesync-disciplined UTC) but schedules on the monotonic
//! clock, re-sampling the monotonic-to-realtime anchor at least every ~100
//! frames. [`WallClock`] holds that anchor and hands out
//! `100 ns since the Unix epoch` readings.
//!
//! The clock source is injectable so tests drive it deterministically — no
//! `sleep`. Production uses [`SystemClock`] (`Instant::now` + `Utc::now`).
//!
//! Anchoring (#147: the re-anchor must not step the genlock wall): every anchor
//! is a BRACKETED sample (the narrowest of up to 8 `m1 / utc / m2` reads, paired
//! at the midpoint), and one resample moves the wall by at most 1 ms. A larger
//! correction slews in over the following resamples, unless two narrow
//! resamples confirm the same step: then it is followed in ONE event (a
//! relabel plus its remainder, #224 part 2, below). A backward correction is
//! always a hold, never a backward step. The pure math is in
//! `wallclock_anchor.rs`.
//!
//! Step probe (#224): every tick also takes ONE bracketed read and measures it
//! against the line the wall runs on. A step over 2 ms, confirmed in the same
//! tick by a full anchor sample within 1 ms, is followed in that ONE event, so
//! a dantesync date step reaches the stamps the boundary it lands, as it
//! reaches every fleet receiver, instead of two resamples (3.3–6.7 s) later.
//!
//! A followed step RELABELS (#224 part 2, `fleet_shift.rs`): the UTC anchor
//! takes the whole step S, but [`WallClock::now_100ns`] reads the internal
//! TIMELINE `UTC − D(K_w)`, and the follow moves K_w by the fleet's N = ⌊S/P⌋
//! whole slots. So the timeline moves only by the remainder r (0 ≤ r ≤ one
//! slot, forward; a residue hold under 4 ms at worst): a date step never
//! holds it by S and never jumps it by S. The wire stamps get the labels back
//! at the submit edge (`FrameSubmitter`). A wall built now, or ticking again
//! after more than 10 s idle, JOINS the fleet's line instead of reading its
//! own drift as a step (`WALL_REJOIN_IDLE`, `FleetShift::join`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use sp_core::genlock::should_resample_mono_to_real_offset;

use crate::playback::fleet_shift::{
    FleetLine, FleetShift, WallShift, global, relabel_100ns, shift_100ns,
};

#[path = "wallclock_anchor.rs"]
mod wallclock_anchor;
pub use wallclock_anchor::{
    ANCHOR_MAX_ATTEMPTS, ANCHOR_MAX_STEP_100NS, ANCHOR_TIGHT_BRACKET, ANCHOR_WIDE_BRACKET, Anchor,
    AnchorDecision, AnchorSample, AnchorStep, BracketedRead, FollowedStep, PendingStep,
    ProbeDecision, ProbeFollow, STEP_DETECT_100NS, StepDirection, StepProbeStats, WALL_REJOIN_IDLE,
    WallAnchorStats, apply_anchor_step, bounded_anchor_update, choose_bracketed_sample,
    decide_anchor_step, decide_step_probe, to_us, wall_at,
};

/// Source of paired `(monotonic instant, utc_100ns)` samples. Production reads
/// the system clocks; tests inject a deterministic fake.
pub trait ClockSource: Send + Sync {
    /// Sample both clocks "at the same instant". [`WallClock`] anchors through
    /// [`read_bracketed`](ClockSource::read_bracketed) instead. A fake that only
    /// implements this method gets zero-width brackets (an exact pairing).
    fn sample(&self) -> (Instant, i64);

    /// One bracketed read: monotonic, then realtime, then monotonic again. Called
    /// at anchor time (construction, every resample and a step probe's
    /// confirmation, up to [`ANCHOR_MAX_ATTEMPTS`] times each) and ONCE per
    /// tick by the step probe (#224), never on the hot read path. The default
    /// wraps [`sample`](ClockSource::sample) as a zero-width bracket.
    /// [`SystemClock`] reads the real clocks, and a test fake overrides it to
    /// inject a preempted (wide) read (#147).
    fn read_bracketed(&self) -> BracketedRead {
        let (inst, utc_100ns) = self.sample();
        BracketedRead {
            m1: inst,
            utc_100ns,
            m2: inst,
        }
    }

    /// Monotonic instant only, for the hot `now_100ns` read path — it MUST NOT
    /// touch the realtime clock (#146 follow-up: the read path must not call
    /// `Utc::now()`). The default pairs it out of
    /// [`sample`](ClockSource::sample); [`SystemClock`] and any source that
    /// must keep the read path off the realtime clock override it.
    fn now_monotonic(&self) -> Instant {
        self.sample().0
    }

    /// Current wall time in 100-ns units, for the hot read path. The default
    /// derives it from the monotonic clock: elapsed since the
    /// `anchor_instant` plus `anchor_utc_100ns` — exactly the production formula,
    /// so [`SystemClock`] keeps its `Utc::now()`-free read path. The wall
    /// passes its TIMELINE anchor (the UTC anchor minus D(K_w), #224 part 2).
    /// A fully synthetic test clock (e.g. the boundary-paced `Pacer` fake)
    /// overrides this to return a directly controllable value, so a test can
    /// advance the wall clock between the scheduling read and the emit read
    /// (#147); it ignores the anchor, so it never sees a relabel.
    fn read_100ns(&self, anchor_instant: Instant, anchor_utc_100ns: i64) -> i64 {
        wall_at(anchor_instant, anchor_utc_100ns, self.now_monotonic())
    }
}

/// Production clock source: `Instant::now()` paired with the UTC wall clock in
/// 100-ns units since the Unix epoch.
pub struct SystemClock;

impl ClockSource for SystemClock {
    fn sample(&self) -> (Instant, i64) {
        let s = choose_bracketed_sample(|| self.read_bracketed());
        (s.instant, s.utc_100ns)
    }

    fn read_bracketed(&self) -> BracketedRead {
        // Bracket the realtime read between two monotonic reads (#147). A
        // preemption between them widens the bracket and loses the vote in
        // `choose_bracketed_sample`, instead of pairing a stale instant.
        let m1 = Instant::now();
        let utc_100ns = utc_now_100ns();
        let m2 = Instant::now();
        BracketedRead { m1, utc_100ns, m2 }
    }

    fn now_monotonic(&self) -> Instant {
        // Hot read path: monotonic ONLY — never `Utc::now()` (#146 follow-up).
        Instant::now()
    }
}

/// UTC now as 100-ns units since the Unix epoch. Uses the non-panicking
/// `timestamp_nanos_opt`; returns 0 only for times outside the ~1677..2262
/// representable window (never in practice).
pub fn utc_now_100ns() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .map(|ns| ns / 100)
        .unwrap_or(0)
}

/// A monotonic-to-UTC anchor plus a frame counter. `now_100ns()` reads the
/// internal timeline (UTC less the fleet relabel D(K_w), #224 part 2);
/// `tick()` advances the counter, re-anchors every
/// `OFFSET_RESAMPLE_INTERVAL_FRAMES` frames so long-run drift between the
/// monotonic and realtime clocks stays bounded (contract §1), and probes for
/// a UTC step every tick (#224).
///
/// One `WallClock` is owned per pipeline thread (by `FrameSubmitter`).
pub struct WallClock {
    source: Box<dyn ClockSource>,
    anchor: Anchor,
    frames_since_resample: u64,
    stats: WallAnchorStats,
    /// A clamped resample (either direction) the next one may confirm (#147).
    pending: Option<PendingStep>,
    /// The step probe's telemetry (#224).
    probe_stats: StepProbeStats,
    /// Where the first over-2 ms probe that is not followed yet was taken
    /// (#224): the detection instant of `last_detect_to_follow_us`. Cleared by
    /// a quiet probe and by the follow.
    suspect_since: Option<Instant>,
    /// The fleet relabel registry this wall follows (#224 part 2): the
    /// process-wide one for a production wall, a test's own otherwise.
    fleet: Arc<FleetShift>,
    /// This wall's relabel K_w and its last regrid (#224 part 2).
    shift: WallShift,
    /// The monotonic instant of the last tick (the construction sample's
    /// before the first): a tick more than [`WALL_REJOIN_IDLE`] later
    /// rejoins the fleet's line (#224 part 2).
    last_tick: Instant,
}

/// One anchor sample from `source`: the narrowest of up to
/// [`ANCHOR_MAX_ATTEMPTS`] bracketed reads (#147).
fn anchor_sample(source: &dyn ClockSource) -> AnchorSample {
    choose_bracketed_sample(|| source.read_bracketed())
}

impl WallClock {
    /// Build a wall clock over `source` with a relabel registry of its own (a
    /// test's wall never shares the process-wide one), seeding the anchor
    /// from one bracketed sample.
    pub fn new(source: Box<dyn ClockSource>) -> Self {
        Self::with_fleet(source, Arc::default())
    }

    /// Build a wall clock over `source` that follows date steps through the
    /// relabel registry `fleet` (#224 part 2). It JOINS the fleet
    /// ([`FleetShift::join`]): the line a wall published at its last tick,
    /// with that wall's K — so a wall built between a date step and its
    /// registration reads the step itself at its first tick — else its own
    /// sample at the current K.
    pub fn with_fleet(source: Box<dyn ClockSource>, fleet: Arc<FleetShift>) -> Self {
        let sample = anchor_sample(&*source);
        let mut stats = WallAnchorStats::default();
        stats.record_sample(&sample);
        let (anchor, shift) = fleet.join(&sample);
        Self {
            source,
            anchor,
            frames_since_resample: 0,
            stats,
            pending: None,
            probe_stats: StepProbeStats::default(),
            suspect_since: None,
            shift,
            fleet,
            last_tick: sample.instant,
        }
    }

    /// Production constructor over [`SystemClock`], on the process-wide
    /// relabel registry every production wall shares (#224 part 2).
    pub fn system() -> Self {
        Self::with_fleet(Box::new(SystemClock), global().clone())
    }

    /// The internal TIMELINE now (#224 part 2): `anchor_utc +
    /// elapsed_monotonic − D(K_w)`, in 100-ns units. Every paced sender runs
    /// on it; the NDI wire stamps get the labels back at the submit edge.
    ///
    /// Hot read path: it reads the MONOTONIC clock only (`now_monotonic`) —
    /// never `Utc::now()` (#146 follow-up). The realtime clock is sampled only
    /// in [`tick`](Self::tick) (every resample and the step probe, #224) and
    /// at construction.
    pub fn now_100ns(&self) -> i64 {
        let timeline_utc = self.anchor.utc_100ns - shift_100ns(self.shift.slots);
        self.source.read_100ns(self.anchor.instant, timeline_utc)
    }

    /// The internal TIMELINE's line now (#224 part 2): [`now_100ns`] without
    /// a hold in progress — through a hold it reads below the frozen value by
    /// the hold still to go ([`Anchor::line_at`]). VBAN's clock slews every
    /// regrid's movement off it, a hold's too, so a residue hold never stops
    /// its packets. Reads the monotonic clock only; a synthetic test clock's
    /// `read_100ns` override does not apply.
    ///
    /// [`now_100ns`]: Self::now_100ns
    pub fn line_100ns(&self) -> i64 {
        let line = self.anchor.line_at(self.source.now_monotonic());
        line - shift_100ns(self.shift.slots)
    }

    /// Advance the frame counter, re-anchor the monotonic-to-UTC mapping every
    /// `OFFSET_RESAMPLE_INTERVAL_FRAMES` frames, then probe for a UTC step
    /// (#224). The probe runs after the resample, so a step the resample saw
    /// first (1 ms applied and armed) is followed in this same tick. A tick
    /// more than [`WALL_REJOIN_IDLE`] after the last one first rejoins the
    /// fleet's line ([`rejoin`](Self::rejoin)); every tick then publishes
    /// this wall's line for the next wall to join (#224 part 2).
    pub fn tick(&mut self) {
        let now = self.source.now_monotonic();
        let idle = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        if idle > WALL_REJOIN_IDLE {
            self.rejoin(idle);
        }
        self.frames_since_resample = self.frames_since_resample.saturating_add(1);
        if should_resample_mono_to_real_offset(self.frames_since_resample) {
            self.reanchor();
            self.frames_since_resample = 0;
        }
        self.probe_step();
        self.fleet.publish(FleetLine {
            at: now,
            anchor: self.anchor,
            slots: self.shift.slots,
            epochs: self.shift.epochs,
        });
    }

    /// Rejoin the fleet after `idle` without a tick (#224 part 2): this wall
    /// was not watching the clock, so its line's drift over the gap (±36 ms
    /// after 20 min at ±30 ppm) cannot be told from a date step, and
    /// registering it would relabel every other wall. It takes a fresh
    /// sample and moves its timeline onto the line [`FleetShift::join`]
    /// gives (a fresh wall's, else the realtime clock at the current K), a
    /// jump ahead or ONE hold — never a step back — and registers nothing.
    /// VBAN's clock owes the movement like a regrid's.
    fn rejoin(&mut self, idle: Duration) {
        let sample = anchor_sample(&*self.source);
        self.stats.record_sample(&sample);
        let (line, joined) = self.fleet.join(&sample);
        let current = self.anchor.wall_at(sample.instant) - shift_100ns(self.shift.slots);
        let target = line.line_at(sample.instant) - shift_100ns(joined.slots);
        let moved = target - current;
        let (instant, t) = apply_anchor_step(sample.instant, current, moved);
        self.anchor = Anchor {
            instant,
            utc_100ns: t + shift_100ns(joined.slots),
        };
        self.shift = WallShift {
            slots: joined.slots,
            epochs: joined.epochs,
            regrids: self.shift.regrids + 1,
            last_remainder_100ns: self.shift.last_remainder_100ns,
            last_jump_100ns: moved,
            moved_100ns: self.shift.moved_100ns + moved,
        };
        self.pending = None; // armed before the gap: nothing left to confirm
        self.suspect_since = None;
        info!(
            idle_ms = u64::try_from(idle.as_millis()).unwrap_or(u64::MAX),
            moved_us = to_us(moved),
            shift_slots = self.shift.slots,
            bracket_us = sample.bracket.as_micros() as u64,
            "wallclock: ticked again after over 10 s idle — rejoined the fleet's line; its own drift is never read as a date step (#224)"
        );
    }

    /// The per-boundary step probe (#224, `decide_step_probe`): one bracketed
    /// read against the line the wall runs on. A step over 2 ms, confirmed by
    /// a full anchor sample within 1 ms, is followed now in ONE event; a wide
    /// or unconfirmed probe is rejected and counted, and nothing is armed, so
    /// the next boundary's probe follows a real step at once.
    fn probe_step(&mut self) {
        let probe = self.source.read_bracketed();
        let source = &*self.source;
        let confirm = || anchor_sample(source);
        match decide_step_probe(self.anchor, self.pending, probe, confirm) {
            ProbeDecision::Quiet => self.suspect_since = None,
            ProbeDecision::RejectedWide {
                delta_100ns,
                bracket,
            } => {
                self.reject_probe(probe.midpoint());
                debug!(
                    delta_us = to_us(delta_100ns),
                    bracket_us = bracket.as_micros() as u64,
                    rejected = self.probe_stats.rejected,
                    "wallclock: a step probe over 2 ms from a wide bracket — rejected (#224)"
                );
            }
            ProbeDecision::Unconfirmed {
                delta_100ns,
                confirm,
                confirm_delta_100ns,
            } => {
                self.stats.record_sample(&confirm);
                self.reject_probe(probe.midpoint());
                info!(
                    delta_us = to_us(delta_100ns),
                    confirm_delta_us = to_us(confirm_delta_100ns),
                    confirm_bracket_us = confirm.bracket.as_micros() as u64,
                    rejected = self.probe_stats.rejected,
                    "wallclock: a step probe over 2 ms was not confirmed by the anchor sample — rejected (#224)"
                );
            }
            ProbeDecision::Follow(follow) => self.follow_probed_step(probe, &follow),
        }
    }

    /// Count a rejected probe and remember where the first unfollowed one was.
    fn reject_probe(&mut self, at: Instant) {
        self.probe_stats.rejected += 1;
        self.suspect_since.get_or_insert(at);
    }

    /// Record a follow at `followed_at` (#224): the time since the first
    /// over-2 ms probe not followed yet, else since `detected_at` (the
    /// follow's own first reading). It ends the detection; returns the µs.
    fn record_detect_to_follow(&mut self, detected_at: Instant, followed_at: Instant) -> u64 {
        let detected = self.suspect_since.take().unwrap_or(detected_at);
        let latency = followed_at.saturating_duration_since(detected);
        let latency_us = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.probe_stats.last_detect_to_follow_us = latency_us;
        latency_us
    }

    /// Apply a step the probe confirmed (#224): the telemetry, the regrid
    /// (the relabel + the timeline's remainder, never a step back), one INFO
    /// line.
    fn follow_probed_step(&mut self, probe: BracketedRead, follow: &ProbeFollow) {
        self.stats.record_sample(&follow.sample);
        self.stats.record_step(follow.delta_100ns, &follow.step);
        self.pending = None; // the whole step is followed: nothing left to confirm
        // A fresh anchor: the next resample is 100 ticks away, so it never lands
        // inside a hold this follow may start (a submit consumer ticks per job,
        // also inside its own hold) and re-measures it as a new step.
        self.frames_since_resample = 0;
        let latency_us = self.record_detect_to_follow(probe.midpoint(), follow.sample.instant);
        let (at, applied) = (follow.sample.instant, follow.step.applied_100ns);
        let timeline = self.regrid(at, follow.wall_100ns, applied, &follow.followed);
        info!(
            delta_us = to_us(follow.delta_100ns),
            step_us = to_us(follow.followed.total_100ns),
            applied_us = to_us(follow.step.applied_100ns),
            direction = follow.followed.direction.as_str(),
            shift_slots = self.shift.slots,
            remainder_us = to_us(self.shift.last_remainder_100ns),
            timeline_us = to_us(timeline),
            probe_delta_us = to_us(follow.probe_delta_100ns),
            probe_bracket_us = probe.width().as_micros() as u64,
            bracket_us = follow.sample.bracket.as_micros() as u64,
            detect_to_follow_us = latency_us,
            "wallclock: UTC step followed at once by the boundary probe (#224)"
        );
    }

    /// Follow a confirmed step (#224 part 2, `fleet_shift.rs`). The UTC
    /// anchor takes the whole step (`applied_utc` against `wall_utc`, what
    /// the wall reads at `at`); the registry says how many whole slots N the
    /// labels move; the timeline moves only by the rest. That is a step ahead
    /// of the remainder r, or ONE hold of a residue under 4 ms: when a
    /// resample's armed 1 ms already moved the wall further than r, when the
    /// wall adopted another wall's N for a step it read up to 3 ms smaller,
    /// or when it applied a backward step under 3 ms on its own (no epoch).
    /// Never a step back. Returns the timeline's movement (100 ns).
    fn regrid(
        &mut self,
        at: Instant,
        wall_utc: i64,
        applied_utc: i64,
        followed: &FollowedStep,
    ) -> i64 {
        let relabel = self.fleet.follow(self.shift.epochs, followed.total_100ns);
        let k = self.shift.slots;
        let moved = relabel_100ns(k, relabel.slots);
        let timeline = applied_utc - moved;
        let (instant, t) = apply_anchor_step(at, wall_utc - shift_100ns(k), timeline);
        self.shift = WallShift {
            slots: k + relabel.slots,
            epochs: relabel.epochs,
            regrids: self.shift.regrids + 1,
            last_remainder_100ns: followed.total_100ns - moved,
            last_jump_100ns: timeline,
            moved_100ns: self.shift.moved_100ns + timeline,
        };
        self.anchor = Anchor {
            instant,
            utc_100ns: t + shift_100ns(self.shift.slots),
        };
        let step = AnchorStep {
            applied_100ns: timeline,
            carry_100ns: 0,
        };
        self.stats.record_follow(followed, &step);
        timeline
    }

    /// Re-anchor from a fresh bracketed sample (#147). `delta` is what an
    /// unbounded re-anchor would step the wall by at the sample instant. Within
    /// ±1 ms it applies as-is (normal dantesync slewing). Beyond that ±1 ms
    /// applies now, UNLESS this resample confirms the previous one's step
    /// (`decide_anchor_step`: both brackets narrow, the same delta ±1 ms): then
    /// the rest of the step is followed in this ONE event, as every other fleet
    /// sender follows a dantesync date step, through the same [`regrid`]
    /// as the probe (#224 part 2: a relabel plus the remainder). A lone outlier
    /// stays bounded at 1 ms; a backward correction is a hold, never a step
    /// back.
    ///
    /// [`regrid`]: Self::regrid
    fn reanchor(&mut self) {
        let sample = anchor_sample(&*self.source);
        self.stats.record_sample(&sample);
        let wall = self.anchor.wall_at(sample.instant);
        let delta = sample.utc_100ns.saturating_sub(wall);
        let decision = decide_anchor_step(self.pending, delta, !sample.is_wide());
        self.pending = decision.pending;
        let step = decision.step;
        self.stats.record_step(delta, &step);
        if let Some(followed) = decision.followed {
            // #224: the resample follows only a step no probe followed (every
            // probe of it rejected); the follow ends that detection.
            let latency_us = self.record_detect_to_follow(sample.instant, sample.instant);
            let timeline = self.regrid(sample.instant, wall, step.applied_100ns, &followed);
            info!(
                delta_us = to_us(delta),
                step_us = to_us(followed.total_100ns),
                direction = followed.direction.as_str(),
                shift_slots = self.shift.slots,
                remainder_us = to_us(self.shift.last_remainder_100ns),
                timeline_us = to_us(timeline),
                bracket_us = sample.bracket.as_micros() as u64,
                detect_to_follow_us = latency_us,
                "wallclock: confirmed UTC step followed in one re-anchor (#147)"
            );
            return;
        }
        if step.is_clamped() {
            warn!(
                delta_us = to_us(delta),
                bracket_us = sample.bracket.as_micros() as u64,
                applied_us = to_us(step.applied_100ns),
                carry_us = to_us(step.carry_100ns),
                "wallclock: re-anchor delta over 1 ms — 1 ms applied; the rest is followed once confirmed (by the step probe over 2 ms, else the next resample), else slewed (#147, #224)"
            );
        }
        let (instant, utc) = apply_anchor_step(sample.instant, wall, step.applied_100ns);
        self.anchor = Anchor {
            instant,
            utc_100ns: utc,
        };
    }

    /// Anchor telemetry (#147): the largest measured re-anchor delta, the
    /// wide-bracket count, the slewed total and the followed steps / holds.
    /// Surfaced on `PacingStats`.
    pub fn anchor_stats(&self) -> WallAnchorStats {
        self.stats
    }

    /// The step probe's telemetry (#224): rejected probes and the last
    /// follow's detect-to-follow time. Surfaced on `PacingStats`.
    pub fn probe_stats(&self) -> StepProbeStats {
        self.probe_stats
    }

    /// This wall's relabel K_w and its last regrid (#224 part 2). Surfaced
    /// on `PacingStats`; VBAN's clock reads the timeline's jump off it.
    pub fn shift(&self) -> WallShift {
        self.shift
    }

    /// The relabel registry this wall follows (#224 part 2). Its K is the
    /// fleet shift K_F every wire stamp is put on with.
    pub fn fleet(&self) -> &Arc<FleetShift> {
        &self.fleet
    }

    /// Frames elapsed since the last anchor re-sample (0 immediately after a
    /// resample). Mainly for tests.
    pub fn frames_since_resample(&self) -> u64 {
        self.frames_since_resample
    }
}

#[cfg(test)]
impl WallClock {
    /// Test-only: a clock frozen at `utc_100ns`. It captures one `Instant` and
    /// always returns it, so elapsed is always 0 and `now_100ns()`
    /// deterministically equals `utc_100ns`.
    pub fn fixed(utc_100ns: i64) -> Self {
        struct FixedClock {
            inst: Instant,
            utc: i64,
        }
        impl ClockSource for FixedClock {
            fn sample(&self) -> (Instant, i64) {
                (self.inst, self.utc)
            }
        }
        WallClock::new(Box::new(FixedClock {
            inst: Instant::now(),
            utc: utc_100ns,
        }))
    }

    /// Test-only: a fully synthetic clock whose `now_100ns()` returns whatever
    /// the returned [`SettableClock`] handle was last `set` to. Unlike
    /// [`fixed`](Self::fixed), it is driven directly (not off the monotonic
    /// clock), so a test can advance the wall clock between two reads inside one
    /// `Pacer::service` call — the scheduling read and the emit read — to prove
    /// lateness includes decode time (#147). The anchor, the resample and the
    /// step probe (#224) read the set value on ONE synthetic line (see
    /// [`SettableClock`]), so a set forward never looks like a UTC step. A set
    /// BACK across a resample still reads as a backward delta against the
    /// frozen wall (a phantom hold in `wall_anchor_*` only; the reads return
    /// the set value): never pin the anchor telemetry after a set back.
    pub fn settable(initial_100ns: i64) -> (Self, SettableClock) {
        let handle = SettableClock::new(initial_100ns);
        (WallClock::new(Box::new(handle.clone())), handle)
    }
}

/// Test-only handle over a settable wall clock (see [`WallClock::settable`]).
/// Cloneable so a `pull` closure and the test body can both drive it. Its
/// realtime read pairs the set value with a SYNTHETIC monotonic instant on
/// one line (`origin` + the value's offset from the start), never with the
/// real `Instant::now()`: a set that outruns real time would otherwise read as
/// a UTC step, and the step probe (#224) would follow it (review round 2: it
/// restarted the resample count of `the_sender_thread_ticks_its_wall_once_per_boundary_passed`).
#[cfg(test)]
#[derive(Clone)]
pub struct SettableClock {
    value: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// The instant the start value pairs with: a day ahead of the real clock,
    /// so a value set back by up to a day still maps to an instant.
    origin: Instant,
    start_100ns: i64,
}

#[cfg(test)]
impl SettableClock {
    fn new(start_100ns: i64) -> Self {
        Self {
            value: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(start_100ns)),
            origin: Instant::now() + std::time::Duration::from_secs(86_400),
            start_100ns,
        }
    }

    /// Set the wall clock's current 100-ns reading.
    pub fn set(&self, v_100ns: i64) {
        self.value
            .store(v_100ns, std::sync::atomic::Ordering::SeqCst);
    }

    /// Advance the wall clock by `delta_100ns` (may be negative to step back).
    pub fn advance(&self, delta_100ns: i64) {
        self.value
            .fetch_add(delta_100ns, std::sync::atomic::Ordering::SeqCst);
    }

    /// Current 100-ns reading.
    pub fn get(&self) -> i64 {
        self.value.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The synthetic monotonic instant the reading `v_100ns` pairs with: on
    /// the line through (`origin`, the start value).
    fn instant_of(&self, v_100ns: i64) -> Instant {
        let offset = v_100ns.saturating_sub(self.start_100ns);
        let d = std::time::Duration::from_nanos(offset.unsigned_abs().saturating_mul(100));
        let at = if offset >= 0 {
            self.origin.checked_add(d)
        } else {
            self.origin.checked_sub(d)
        };
        at.expect("a settable clock moved more than a day back, or out of Instant range")
    }
}

#[cfg(test)]
impl ClockSource for SettableClock {
    fn sample(&self) -> (Instant, i64) {
        let v = self.get();
        (self.instant_of(v), v)
    }

    fn read_100ns(&self, _anchor_instant: Instant, _anchor_utc_100ns: i64) -> i64 {
        self.get()
    }
}

// Test-only virtual-time source for the #147 anchor tests (wallclock + pacer).
#[cfg(test)]
#[path = "wallclock_test_clock.rs"]
mod wallclock_test_clock;
#[cfg(test)]
pub use wallclock_test_clock::VirtualClock;

#[cfg(test)]
#[path = "wallclock_tests.rs"]
mod wallclock_tests;

#[cfg(test)]
#[path = "wallclock_tests_anchor.rs"]
mod wallclock_tests_anchor;

#[cfg(test)]
#[path = "wallclock_tests_mutants.rs"]
mod wallclock_tests_mutants;

#[cfg(test)]
#[path = "wallclock_tests_confirm.rs"]
mod wallclock_tests_confirm;

#[cfg(test)]
#[path = "wallclock_tests_confirm_backward.rs"]
mod wallclock_tests_confirm_backward;

#[cfg(test)]
#[path = "wallclock_tests_probe.rs"]
mod wallclock_tests_probe;

#[cfg(test)]
#[path = "wallclock_tests_probe_rule.rs"]
mod wallclock_tests_probe_rule;

#[cfg(test)]
#[path = "wallclock_tests_regrid.rs"]
mod wallclock_tests_regrid;
