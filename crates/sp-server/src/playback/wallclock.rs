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
//! correction slews in over the following resamples, and a backward correction
//! is a ≤ 1 ms hold, never a backward step. The pure math is in
//! `wallclock_anchor.rs`.

use std::time::Instant;

use sp_core::genlock::should_resample_mono_to_real_offset;

#[path = "wallclock_anchor.rs"]
mod wallclock_anchor;
pub use wallclock_anchor::{
    ANCHOR_MAX_ATTEMPTS, ANCHOR_MAX_STEP_100NS, ANCHOR_TIGHT_BRACKET, ANCHOR_WIDE_BRACKET,
    AnchorSample, AnchorStep, BracketedRead, WallAnchorStats, apply_anchor_step,
    bounded_anchor_update, choose_bracketed_sample, to_us, wall_at,
};

/// Source of paired `(monotonic instant, utc_100ns)` samples. Production reads
/// the system clocks; tests inject a deterministic fake.
pub trait ClockSource: Send + Sync {
    /// Sample both clocks "at the same instant". [`WallClock`] anchors through
    /// [`read_bracketed`](ClockSource::read_bracketed) instead. A fake that only
    /// implements this method gets zero-width brackets (an exact pairing).
    fn sample(&self) -> (Instant, i64);

    /// One bracketed read: monotonic, then realtime, then monotonic again. Called
    /// only at anchor time (construction + every resample, up to
    /// [`ANCHOR_MAX_ATTEMPTS`] times), never on the hot read path. The default
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

    /// Current wall time in 100-ns units since the Unix epoch, for the hot read
    /// path. The default derives it from the monotonic clock: elapsed since the
    /// `anchor_instant` plus `anchor_utc_100ns` — exactly the production formula,
    /// so [`SystemClock`] keeps its `Utc::now()`-free read path. A fully
    /// synthetic test clock (e.g. the boundary-paced `Pacer` fake) overrides
    /// this to return a directly controllable value, so a test can advance the
    /// wall clock between the scheduling read and the emit read (#147).
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
/// current UTC; `tick()` advances the counter and re-anchors every
/// `OFFSET_RESAMPLE_INTERVAL_FRAMES` frames so long-run drift between the
/// monotonic and realtime clocks stays bounded (contract §1).
///
/// One `WallClock` is owned per pipeline thread (by `FrameSubmitter`).
pub struct WallClock {
    source: Box<dyn ClockSource>,
    anchor_instant: Instant,
    anchor_utc_100ns: i64,
    frames_since_resample: u64,
    stats: WallAnchorStats,
}

/// One anchor sample from `source`: the narrowest of up to
/// [`ANCHOR_MAX_ATTEMPTS`] bracketed reads (#147).
fn anchor_sample(source: &dyn ClockSource) -> AnchorSample {
    choose_bracketed_sample(|| source.read_bracketed())
}

impl WallClock {
    /// Build a wall clock over `source`, seeding the anchor from one bracketed
    /// sample.
    pub fn new(source: Box<dyn ClockSource>) -> Self {
        let sample = anchor_sample(&*source);
        let mut stats = WallAnchorStats::default();
        stats.record_sample(&sample);
        Self {
            source,
            anchor_instant: sample.instant,
            anchor_utc_100ns: sample.utc_100ns,
            frames_since_resample: 0,
            stats,
        }
    }

    /// Production constructor over [`SystemClock`].
    pub fn system() -> Self {
        Self::new(Box::new(SystemClock))
    }

    /// Current wall time: `anchor_utc + elapsed_monotonic`, in 100-ns units
    /// since the Unix epoch.
    ///
    /// Hot read path: it reads the MONOTONIC clock only (`now_monotonic`) —
    /// never `Utc::now()` (#146 follow-up). The realtime clock is sampled only
    /// at anchor time (construction and every resample in [`tick`](Self::tick)).
    pub fn now_100ns(&self) -> i64 {
        self.source
            .read_100ns(self.anchor_instant, self.anchor_utc_100ns)
    }

    /// Advance the frame counter and re-anchor the monotonic-to-UTC mapping
    /// every `OFFSET_RESAMPLE_INTERVAL_FRAMES` frames.
    pub fn tick(&mut self) {
        self.frames_since_resample = self.frames_since_resample.saturating_add(1);
        if should_resample_mono_to_real_offset(self.frames_since_resample) {
            self.reanchor();
            self.frames_since_resample = 0;
        }
    }

    /// Re-anchor from a fresh bracketed sample, moving the wall by at most
    /// [`ANCHOR_MAX_STEP_100NS`] (#147). `delta` is what an unbounded re-anchor
    /// would step the wall by at the sample instant. Within ±1 ms it applies
    /// as-is (normal dantesync slewing). Beyond that ±1 ms applies now and the
    /// rest slews in over the next resamples, each of which re-measures the
    /// remaining offset. A backward correction is a hold, never a step back.
    fn reanchor(&mut self) {
        let sample = anchor_sample(&*self.source);
        self.stats.record_sample(&sample);
        let wall = wall_at(self.anchor_instant, self.anchor_utc_100ns, sample.instant);
        let delta = sample.utc_100ns.saturating_sub(wall);
        let step = bounded_anchor_update(delta);
        self.stats.record_step(delta, &step);
        if step.is_clamped() {
            tracing::warn!(
                delta_us = to_us(delta),
                bracket_us = sample.bracket.as_micros() as u64,
                applied_us = to_us(step.applied_100ns),
                carry_us = to_us(step.carry_100ns),
                "wallclock: re-anchor delta over 1 ms — slewing it in, not stepping the wall (#147)"
            );
        }
        let (instant, utc) = apply_anchor_step(sample.instant, wall, step.applied_100ns);
        self.anchor_instant = instant;
        self.anchor_utc_100ns = utc;
    }

    /// Anchor telemetry (#147): the largest measured re-anchor delta, the
    /// wide-bracket count and the slewed total. Surfaced on `PacingStats`.
    pub fn anchor_stats(&self) -> WallAnchorStats {
        self.stats
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
    /// lateness includes decode time (#147). `tick`'s re-anchor is a no-op here
    /// (`read_100ns` ignores the anchor).
    pub fn settable(initial_100ns: i64) -> (Self, SettableClock) {
        let handle = SettableClock(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            initial_100ns,
        )));
        let source = SettableClock(handle.0.clone());
        (WallClock::new(Box::new(source)), handle)
    }
}

/// Test-only handle over a settable wall clock (see [`WallClock::settable`]).
/// Cloneable so a `pull` closure and the test body can both drive it.
#[cfg(test)]
#[derive(Clone)]
pub struct SettableClock(std::sync::Arc<std::sync::atomic::AtomicI64>);

#[cfg(test)]
impl SettableClock {
    /// Set the wall clock's current 100-ns reading.
    pub fn set(&self, v_100ns: i64) {
        self.0.store(v_100ns, std::sync::atomic::Ordering::SeqCst);
    }

    /// Advance the wall clock by `delta_100ns` (may be negative to step back).
    pub fn advance(&self, delta_100ns: i64) {
        self.0
            .fetch_add(delta_100ns, std::sync::atomic::Ordering::SeqCst);
    }

    /// Current 100-ns reading.
    pub fn get(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl ClockSource for SettableClock {
    fn sample(&self) -> (Instant, i64) {
        (Instant::now(), self.get())
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
