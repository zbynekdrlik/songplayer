//! The last stretch of the paced boundary wait (#147 follow-up, design record
//! 5852618200): spin to the boundary for precision, but never busy-spin through
//! a wall that stands still.
//!
//! `pipeline_paced::sleep_to_boundary` coarse-sleeps to ~2 ms before the
//! boundary, then waits here. A normal boundary arrives within those ~2 ms, so
//! it never reaches [`SPIN_BUDGET`] and keeps full spin precision.
//!
//! Worst case: a plan read inside a resample's ≤ 1 ms hold makes the coarse
//! sleep short by the un-slept rest of that hold, so the spin starts < 3 ms
//! before the boundary. The inclusive budget still covers that.
//!
//! A followed backward date step is ONE hold of the pacer's wall, up to ~1.5 s
//! (`genlock.md`). The wall freezes a few µs PAST the emitted boundary, so
//! `0 < until − now ≤ interval` for the whole hold, and an unbounded spin burned
//! one core per paced thread through it. Past the budget, measured on the
//! MONOTONIC clock (which keeps running through a hold), each check yields
//! [`SPIN_YIELD`] instead. A frozen wall then costs ~1 wake-up per ms. The wall
//! resumes from its frozen value, so the awaited boundary is still ~one slot
//! ahead, and the wait keeps yielding through that slot too. That boundary goes
//! out at most one yield after the wall reaches it (up to ~1 ms plus the sleep's
//! wake-up overshoot): the one boundary per hold without spin precision.
//!
//! Cross-platform and Linux-tested (`pacer_spin_tests.rs`): `pipeline_paced`
//! itself is `#[cfg(windows)]` and excluded from the mutation gate.

use std::time::{Duration, Instant};

use crate::playback::pacer::Pacer;

/// How long a boundary wait may busy-spin before it yields. It sits above the
/// 2 ms spin margin plus the < 1 ms rest of a resample hold, so a normal
/// boundary never reaches it.
pub const SPIN_BUDGET: Duration = Duration::from_millis(3);

/// One check's wait once the spin is past [`SPIN_BUDGET`]: a 1 ms sleep (the
/// paced threads run on the 1 ms `timeBeginPeriod` timer).
pub const SPIN_YIELD: Duration = Duration::from_millis(1);

/// What one check of the boundary wait does ([`spin_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpinStep {
    /// The wall reached the boundary: stop and emit.
    Done,
    /// The boundary is more than one interval ahead, so the wall stepped back:
    /// stop so the loop re-latches (the old spin's bail, unchanged).
    Bail,
    /// Short of the boundary within the budget: `std::hint::spin_loop()`.
    Spin,
    /// Short of the boundary past the budget. Only a wall that stood still (a
    /// followed hold) gets here, and the wait then stays here through the hold
    /// and the slot after it: sleep [`SPIN_YIELD`].
    Yield,
}

/// The pure decision for one check. `elapsed` is the monotonic time since the
/// wait started spinning, `delta_100ns` is `until − now` on the wall and
/// `interval_100ns` is one grid slot (0 = genlock off, which never bails).
pub fn spin_step(elapsed: Duration, delta_100ns: i64, interval_100ns: i64) -> SpinStep {
    if delta_100ns <= 0 {
        return SpinStep::Done;
    }
    if interval_100ns > 0 && delta_100ns > interval_100ns {
        return SpinStep::Bail;
    }
    if elapsed <= SPIN_BUDGET {
        SpinStep::Spin
    } else {
        SpinStep::Yield
    }
}

/// What one [`spin_to_boundary`] did: the checks that spun, the checks that
/// yielded, and why it stopped ([`SpinStep::Done`] or [`SpinStep::Bail`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpinTally {
    pub spins: u64,
    pub yields: u64,
    pub exit: SpinStep,
}

/// Wait on the pacer's wall until `until_100ns`, one [`spin_step`] per check:
/// spin while within [`SPIN_BUDGET`], then yield [`SPIN_YIELD`] per check.
/// `observe(step, elapsed)` sees every check before it acts on it (the tests'
/// hook; production passes a no-op).
pub fn spin_to_boundary(
    pacer: &Pacer,
    until_100ns: i64,
    mut observe: impl FnMut(SpinStep, Duration),
) -> SpinTally {
    let interval = pacer.interval_100ns();
    let start = Instant::now();
    let mut spins = 0;
    let mut yields = 0;
    loop {
        let elapsed = start.elapsed();
        let step = spin_step(elapsed, until_100ns - pacer.now_100ns(), interval);
        observe(step, elapsed);
        match step {
            SpinStep::Spin => {
                spins += 1;
                std::hint::spin_loop();
            }
            SpinStep::Yield => {
                yields += 1;
                std::thread::sleep(SPIN_YIELD);
            }
            SpinStep::Done | SpinStep::Bail => {
                return SpinTally {
                    spins,
                    yields,
                    exit: step,
                };
            }
        }
    }
}

#[cfg(test)]
#[path = "pacer_spin_tests.rs"]
mod tests;
