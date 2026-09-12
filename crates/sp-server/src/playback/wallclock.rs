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
        let inst = self.source.now_monotonic();
        let elapsed = inst.saturating_duration_since(self.anchor_instant);
        let elapsed_100ns = (elapsed.as_nanos() / 100) as i64;
        self.anchor_utc_100ns.saturating_add(elapsed_100ns)
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
}

#[cfg(test)]
#[path = "wallclock_tests.rs"]
mod wallclock_tests;
