//! Tests for [`WallClock`] using an injectable, deterministic clock source —
//! never a real `sleep`. Wired via
//! `#[cfg(test)] #[path = "wallclock_tests.rs"] mod wallclock_tests;`.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A scriptable [`ClockSource`]: the test drives the monotonic offset (ns)
/// and can observe how many times the clock was actually sampled. `sample`
/// returns `(base + offset, base_utc + offset/100)`.
struct ScriptClock {
    base: Instant,
    base_utc_100ns: i64,
    offset_ns: AtomicU64,
    samples: AtomicU64,
}

impl ScriptClock {
    fn new(base_utc_100ns: i64) -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            base_utc_100ns,
            offset_ns: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        })
    }
    fn advance_ns(&self, ns: u64) {
        self.offset_ns.fetch_add(ns, Ordering::SeqCst);
    }
    fn sample_count(&self) -> u64 {
        self.samples.load(Ordering::SeqCst)
    }
}

impl ClockSource for Arc<ScriptClock> {
    fn sample(&self) -> (Instant, i64) {
        self.samples.fetch_add(1, Ordering::SeqCst);
        let off = self.offset_ns.load(Ordering::SeqCst);
        (
            self.base + Duration::from_nanos(off),
            self.base_utc_100ns + (off / 100) as i64,
        )
    }
}

#[test]
fn now_100ns_advances_with_elapsed_time() {
    let clock = ScriptClock::new(1_000_000_000);
    let wall = WallClock::new(Box::new(clock.clone()));

    let t0 = wall.now_100ns();
    assert_eq!(t0, 1_000_000_000, "at zero elapsed, now == anchor utc");

    clock.advance_ns(500_000); // 500 us == 5_000 * 100 ns
    let t1 = wall.now_100ns();
    assert_eq!(
        t1, 1_000_005_000,
        "now must advance by elapsed monotonic time / 100"
    );
    assert!(t1 > t0);
}

#[test]
fn anchor_resamples_on_100th_tick_not_before() {
    let clock = ScriptClock::new(2_000_000_000);
    let mut wall = WallClock::new(Box::new(clock.clone()));
    // Construction sampled the clock exactly once to seed the anchor.
    assert_eq!(clock.sample_count(), 1);

    for _ in 0..99 {
        wall.tick();
    }
    assert_eq!(
        wall.frames_since_resample(),
        99,
        "no resample within the first 99 ticks"
    );
    assert_eq!(
        clock.sample_count(),
        1,
        "tick must not sample the clock before the 100th"
    );

    wall.tick(); // the 100th tick
    assert_eq!(
        wall.frames_since_resample(),
        0,
        "the 100th tick resets the frame counter"
    );
    assert_eq!(
        clock.sample_count(),
        2,
        "the 100th tick re-samples the anchor exactly once"
    );
}
