//! Test-only virtual-time [`ClockSource`] for the #147 anchor tests: the
//! WallClock re-anchor must not step the genlock wall.
//!
//! The monotonic clock is `base + t` where the test drives `t`. The TRUE UTC
//! is `t · (1 + ppm·10⁻⁶) / 100 + offset` in 100-ns units, so a test can slew
//! it (dantesync ≤ 94 ppm) or step it (`step_utc`). A read can be scripted as
//! PREEMPTED. The thread reads the monotonic clock at `t − delay`, is
//! descheduled, and reads UTC (and the closing monotonic read) only at `t`. That
//! is exactly the unbracketed `(Instant::now(), Utc::now())` failure the box
//! showed. An unscripted read is clean (zero-width bracket).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{BracketedRead, ClockSource};

/// Virtual monotonic time starts at 10 s, so a read delayed by up to 10 s
/// still sits after `base`. The true UTC at `T0` (ppm 0) is exactly the 30-fps
/// grid boundary `1e8` (10 s in 100-ns units).
const VIRTUAL_T0_NS: u64 = 10_000_000_000;

/// Scriptable virtual clock (see the module doc). Share it as `Arc` so the
/// test body drives the time while the `WallClock` owns a clone.
pub struct VirtualClock {
    base: Instant,
    t_ns: AtomicU64,
    utc_offset_100ns: AtomicI64,
    slew_ppm: i64,
    delays_ns: Mutex<VecDeque<u64>>,
    reads: AtomicU64,
}

impl VirtualClock {
    /// A clock at `VIRTUAL_T0_NS` whose true UTC runs `slew_ppm` fast (+) or
    /// slow (−) against the monotonic clock.
    pub fn new(slew_ppm: i64) -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            t_ns: AtomicU64::new(VIRTUAL_T0_NS),
            utc_offset_100ns: AtomicI64::new(0),
            slew_ppm,
            delays_ns: Mutex::new(VecDeque::new()),
            reads: AtomicU64::new(0),
        })
    }

    /// Advance the virtual monotonic clock.
    pub fn advance_ns(&self, ns: u64) {
        self.t_ns.fetch_add(ns, Ordering::SeqCst);
    }

    /// Step the TRUE UTC by `delta_100ns` (a genuine realtime correction).
    pub fn step_utc(&self, delta_100ns: i64) {
        self.utc_offset_100ns
            .fetch_add(delta_100ns, Ordering::SeqCst);
    }

    /// Script the next bracketed reads: each entry preempts one read by that
    /// many ns between its opening monotonic read and its UTC read. 0 = clean.
    pub fn delay_next_reads(&self, delays_ns: &[u64]) {
        self.delays_ns
            .lock()
            .expect("delay script lock")
            .extend(delays_ns.iter().copied());
    }

    /// Bracketed reads taken so far.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// The true UTC now (100 ns).
    pub fn truth_100ns(&self) -> i64 {
        self.truth_at(self.t_ns.load(Ordering::SeqCst))
    }

    fn truth_at(&self, t_ns: u64) -> i64 {
        let scaled = t_ns as i128 * (1_000_000 + self.slew_ppm) as i128 / 1_000_000;
        (scaled / 100) as i64 + self.utc_offset_100ns.load(Ordering::SeqCst)
    }

    fn instant_at(&self, t_ns: u64) -> Instant {
        self.base + Duration::from_nanos(t_ns)
    }
}

impl ClockSource for Arc<VirtualClock> {
    fn sample(&self) -> (Instant, i64) {
        let t = self.t_ns.load(Ordering::SeqCst);
        (self.instant_at(t), self.truth_at(t))
    }

    fn read_bracketed(&self) -> BracketedRead {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let t = self.t_ns.load(Ordering::SeqCst);
        let delay = self
            .delays_ns
            .lock()
            .expect("delay script lock")
            .pop_front()
            .unwrap_or(0);
        BracketedRead {
            m1: self.instant_at(t - delay),
            utc_100ns: self.truth_at(t),
            m2: self.instant_at(t),
        }
    }

    fn now_monotonic(&self) -> Instant {
        self.instant_at(self.t_ns.load(Ordering::SeqCst))
    }
}
