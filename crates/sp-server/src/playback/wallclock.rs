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

use std::time::Instant;

use sp_core::genlock::should_resample_mono_to_real_offset;

/// Source of paired `(monotonic instant, utc_100ns)` samples. Production reads
/// the system clocks; tests inject a deterministic fake.
pub trait ClockSource: Send + Sync {
    /// Sample both clocks "at the same instant". Called only at anchor time
    /// (construction + every resample), never on the hot read path.
    fn sample(&self) -> (Instant, i64);

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
        let inst = self.now_monotonic();
        let elapsed = inst.saturating_duration_since(anchor_instant);
        let elapsed_100ns = (elapsed.as_nanos() / 100) as i64;
        anchor_utc_100ns.saturating_add(elapsed_100ns)
    }
}

/// Production clock source: `Instant::now()` paired with the UTC wall clock in
/// 100-ns units since the Unix epoch.
pub struct SystemClock;

impl ClockSource for SystemClock {
    fn sample(&self) -> (Instant, i64) {
        (Instant::now(), utc_now_100ns())
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
}

impl WallClock {
    /// Build a wall clock over `source`, seeding the anchor from one sample.
    pub fn new(source: Box<dyn ClockSource>) -> Self {
        let (anchor_instant, anchor_utc_100ns) = source.sample();
        Self {
            source,
            anchor_instant,
            anchor_utc_100ns,
            frames_since_resample: 0,
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
            let (inst, utc) = self.source.sample();
            self.anchor_instant = inst;
            self.anchor_utc_100ns = utc;
            self.frames_since_resample = 0;
        }
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

#[cfg(test)]
#[path = "wallclock_tests.rs"]
mod wallclock_tests;

#[cfg(test)]
#[path = "wallclock_tests_mutants.rs"]
mod wallclock_tests_mutants;
