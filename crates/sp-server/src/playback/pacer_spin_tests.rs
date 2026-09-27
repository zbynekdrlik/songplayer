//! #147 follow-up: the boundary wait never busy-spins through a wall that stands
//! still (design record 5852618200, Approach 1).
//!
//! - [`spin_step`] at the exact edge of each of its four rules.
//! - [`spin_to_boundary`] on its own thread over a real 30-fps [`Pacer`] whose
//!   wall is a [`SettableClock`] the test drives. A followed backward date step
//!   freezes the pacer's wall for up to ~1.5 s, a few µs past the boundary it
//!   just emitted.
//!
//! Every wait on the spinner is bounded ([`BOUND`], `recv_timeout`), so a mutant
//! that never stops fails instead of timing out. A [`Release`] guard moves the
//! wall far past the boundary on every exit path, so a spinner a failed
//! assertion left behind ends too. The assertions never time a check: under the
//! Coverage job's ptrace a thread can stall for long, so they compare what the
//! observer saw with the tally, and every yield with the real time it took.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sp_core::genlock::{GENLOCK_GRID_FPS, interval_100ns};

use super::{SPIN_BUDGET, SPIN_YIELD, SpinStep, SpinTally, spin_step, spin_to_boundary};
use crate::playback::pacer::Pacer;
use crate::playback::wallclock::{SettableClock, WallClock};

/// Every wait on the spinner thread is bounded by this.
const BOUND: Duration = Duration::from_secs(20);

/// The boundary the tests wait for (the wall is synthetic; any value works).
const UNTIL: i64 = 17_900_000_000_000_000;

/// One nanosecond, the smallest step past [`SPIN_BUDGET`].
const NS: Duration = Duration::from_nanos(1);

/// One 30-fps grid slot (333 333 × 100 ns): the paced outputs' interval.
fn slot() -> i64 {
    interval_100ns(GENLOCK_GRID_FPS)
}

#[test]
fn the_budget_is_three_ms_and_a_yield_sleeps_one_ms() {
    assert_eq!(SPIN_BUDGET, Duration::from_millis(3));
    assert_eq!(SPIN_YIELD, Duration::from_millis(1));
}

#[test]
fn short_of_the_boundary_it_spins_up_to_the_budget_inclusive() {
    let i = slot();
    assert_eq!(spin_step(Duration::ZERO, 1, i), SpinStep::Spin);
    assert_eq!(spin_step(SPIN_BUDGET, 1, i), SpinStep::Spin);
    assert_eq!(spin_step(SPIN_BUDGET, i, i), SpinStep::Spin);
}

#[test]
fn short_of_the_boundary_past_the_budget_it_yields() {
    let i = slot();
    assert_eq!(spin_step(SPIN_BUDGET + NS, 1, i), SpinStep::Yield);
    assert_eq!(spin_step(SPIN_BUDGET + NS, i, i), SpinStep::Yield);
    // A wall frozen 5 µs past the emitted boundary: 4 ms in, and a whole
    // followed ~1.5 s hold in.
    assert_eq!(
        spin_step(Duration::from_millis(4), i - 50, i),
        SpinStep::Yield
    );
    assert_eq!(
        spin_step(Duration::from_millis(1_500), i - 50, i),
        SpinStep::Yield
    );
}

#[test]
fn the_wall_at_or_past_the_boundary_is_done_even_past_the_budget() {
    let i = slot();
    assert_eq!(spin_step(Duration::ZERO, 0, i), SpinStep::Done);
    assert_eq!(spin_step(Duration::ZERO, -1, i), SpinStep::Done);
    assert_eq!(spin_step(SPIN_BUDGET + NS, 0, i), SpinStep::Done);
    assert_eq!(spin_step(Duration::ZERO, 0, 0), SpinStep::Done);
}

#[test]
fn it_bails_only_when_the_boundary_is_more_than_one_interval_ahead() {
    let i = slot();
    assert_eq!(spin_step(Duration::ZERO, i + 1, i), SpinStep::Bail);
    assert_eq!(spin_step(SPIN_BUDGET + NS, i + 1, i), SpinStep::Bail);
    // Exactly one interval ahead is a normal wait, not a backward step.
    assert_eq!(spin_step(Duration::ZERO, i, i), SpinStep::Spin);
}

#[test]
fn genlock_off_interval_zero_never_bails() {
    assert_eq!(spin_step(Duration::ZERO, 5, 0), SpinStep::Spin);
    assert_eq!(spin_step(Duration::ZERO, i64::MAX, 0), SpinStep::Spin);
    assert_eq!(spin_step(SPIN_BUDGET + NS, 5, 0), SpinStep::Yield);
}

/// On drop, moves the wall far past every boundary, so a spinner that a failed
/// assertion left on a frozen wall ends instead of spinning on.
struct Release(SettableClock);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.set(i64::MAX / 2);
    }
}

/// A settable wall at `at_100ns`, plus the guard that frees the spinner.
fn wall_at(at_100ns: i64) -> (SettableClock, Release) {
    let (_, wall) = WallClock::settable(at_100ns);
    let release = Release(wall.clone());
    (wall, release)
}

/// Run `spin_to_boundary(until)` on its own thread over a real 30-fps `Pacer`
/// whose wall is `wall`. The tally arrives on the returned channel.
fn spawn_spin(
    wall: &SettableClock,
    until: i64,
    observe: impl FnMut(SpinStep, Duration) + Send + 'static,
) -> Receiver<SpinTally> {
    let (tx, rx) = mpsc::channel();
    let clock = wall.clone();
    std::thread::spawn(move || {
        let wall = WallClock::new(Box::new(clock));
        let pacer = Pacer::with_wallclock(GENLOCK_GRID_FPS, true, wall);
        let tally = spin_to_boundary(&pacer, until, observe);
        // A failed test may have dropped the receiver already.
        let _ = tx.send(tally);
    });
    rx
}

#[test]
fn a_wall_at_the_boundary_ends_the_wait_at_once() {
    let (wall, _release) = wall_at(UNTIL);
    let tally = spawn_spin(&wall, UNTIL, |_, _| {})
        .recv_timeout(BOUND)
        .expect("the wait ends at once");
    let expected = SpinTally {
        spins: 0,
        yields: 0,
        exit: SpinStep::Done,
    };
    assert_eq!(tally, expected);
}

#[test]
fn a_wall_more_than_one_interval_short_bails_at_once() {
    let (wall, _release) = wall_at(UNTIL - slot() - 1);
    let tally = spawn_spin(&wall, UNTIL, |_, _| {})
        .recv_timeout(BOUND)
        .expect("the wait bails at once");
    let expected = SpinTally {
        spins: 0,
        yields: 0,
        exit: SpinStep::Bail,
    };
    assert_eq!(tally, expected);
}

/// A normal boundary: the wall moves on while the loop checks, and the wait
/// ends on the very check that sees it reach the boundary.
#[test]
fn the_wait_ends_on_the_check_that_sees_the_wall_reach_the_boundary() {
    const STEP: i64 = 1_000; // 100 µs of wall per check
    let (wall, _release) = wall_at(UNTIL - 20 * STEP);
    let clock = wall.clone();
    let tally = spawn_spin(&wall, UNTIL, move |_, _| clock.advance(STEP))
        .recv_timeout(BOUND)
        .expect("the wait ends once the wall reaches the boundary");
    assert_eq!(tally.exit, SpinStep::Done);
    assert_eq!(tally.spins + tally.yields, 20, "{tally:?}");
}

/// What the observer saw on the spinner thread.
struct Probe {
    spins: AtomicU64,
    yields: AtomicU64,
    /// The latest `elapsed` (ns) at which a check still spun.
    last_spin_ns: AtomicU64,
    /// The earliest `elapsed` (ns) at which a check yielded (`u64::MAX` = none).
    first_yield_ns: AtomicU64,
    /// Yields checked while the wall already stood at or past the boundary.
    yields_after_pass: AtomicU64,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            spins: AtomicU64::new(0),
            yields: AtomicU64::new(0),
            last_spin_ns: AtomicU64::new(0),
            first_yield_ns: AtomicU64::new(u64::MAX),
            yields_after_pass: AtomicU64::new(0),
        })
    }
}

/// The observer: count each check, note when it spun or yielded, and signal
/// every yield on `yielded`.
fn observer(
    probe: &Arc<Probe>,
    wall: &SettableClock,
    yielded: mpsc::Sender<()>,
) -> impl FnMut(SpinStep, Duration) + Send + 'static {
    let probe = Arc::clone(probe);
    let wall = wall.clone();
    move |step: SpinStep, elapsed: Duration| {
        let at_ns = elapsed.as_nanos() as u64;
        match step {
            SpinStep::Spin => {
                probe.spins.fetch_add(1, SeqCst);
                probe.last_spin_ns.fetch_max(at_ns, SeqCst);
            }
            SpinStep::Yield => {
                probe.yields.fetch_add(1, SeqCst);
                probe.first_yield_ns.fetch_min(at_ns, SeqCst);
                if wall.get() >= UNTIL {
                    probe.yields_after_pass.fetch_add(1, SeqCst);
                }
                // The test may have failed and dropped the receiver.
                let _ = yielded.send(());
            }
            SpinStep::Done | SpinStep::Bail => {}
        }
    }
}

/// Wait for `n` more yield signals, each within [`BOUND`].
fn await_yields(yielded: &Receiver<()>, n: usize) {
    for k in 0..n {
        if let Err(e) = yielded.recv_timeout(BOUND) {
            panic!("yield {k} never came ({e:?}): the wait spun through the frozen wall");
        }
    }
}

/// A followed backward date step holds the pacer's wall still for up to ~1.5 s,
/// 5 µs past the boundary it just emitted. The wait spins for at most
/// `SPIN_BUDGET`, then yields 1 ms per check while the wall stands still, never
/// returns while it does, and ends on the first check after the wall passes
/// the boundary.
#[test]
fn a_frozen_wall_is_waited_out_by_yielding_never_by_spinning_through_it() {
    let (wall, _release) = wall_at(UNTIL - slot() + 50);
    let probe = Probe::new();
    let (yield_tx, yielded) = mpsc::channel();
    let started = Instant::now();
    let done = spawn_spin(&wall, UNTIL, observer(&probe, &wall, yield_tx));

    // The wall stands still: the wait must start yielding.
    await_yields(&yielded, 5);
    let spins = probe.spins.load(SeqCst);
    let budget_ns = SPIN_BUDGET.as_nanos() as u64;
    assert!(
        probe.last_spin_ns.load(SeqCst) <= budget_ns,
        "a check spun past the budget"
    );
    assert!(
        probe.first_yield_ns.load(SeqCst) > budget_ns,
        "a check yielded within the budget"
    );

    // Still frozen: it neither returns nor spins again.
    await_yields(&yielded, 5);
    assert_eq!(
        done.try_recv(),
        Err(TryRecvError::Empty),
        "the wait returned while the wall stood still"
    );
    assert_eq!(
        probe.spins.load(SeqCst),
        spins,
        "the wait spun again after it started yielding"
    );

    // The wall resumes and passes the boundary.
    wall.set(UNTIL + 1);
    let tally = done
        .recv_timeout(BOUND)
        .expect("the wait ends once the wall passes the boundary");
    let took = started.elapsed();
    assert_eq!(tally.exit, SpinStep::Done);
    assert_eq!(tally.spins, spins, "{tally:?}");
    assert_eq!(tally.yields, probe.yields.load(SeqCst), "{tally:?}");
    assert!(tally.yields >= 10, "{tally:?}");
    // At most the one check that decided just before the wall moved.
    assert!(
        probe.yields_after_pass.load(SeqCst) <= 1,
        "it kept yielding after the wall passed the boundary"
    );
    // Every yield really sleeps SPIN_YIELD: never a busy loop in disguise.
    assert!(
        u128::from(tally.yields) * SPIN_YIELD.as_nanos() <= took.as_nanos(),
        "{tally:?} in {took:?}"
    );
}
