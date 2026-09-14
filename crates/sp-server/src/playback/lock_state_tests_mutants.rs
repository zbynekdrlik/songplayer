//! Mutation-killing unit tests for `EventWindow` (#149) — `len`, `push` (reset
//! detection) and `evict` (time-eviction + hard cap). Each test pins an EXACT
//! retained-sample count at a precise input so a single operator / whole-body
//! mutation flips it. `super::*` resolves to the `lock_state` module under test.
//!
//! (The pinned-skip mutant `evict` 96:31 `> -> <` is NOT targeted — it makes the
//! hard-cap loop spin forever and is pinned by the supervisor.)

use super::*;

/// 100-ns units per second — the sample-timestamp unit (mirrors sp-core).
const U: i64 = sp_core::genlock::UNITS_PER_SECOND;

/// `len()` returns the retained count. Three in-window, monotonically-increasing
/// samples neither reset nor evict, so the ring holds exactly 3.
///
/// Kills 52:9 `len -> usize with 0`.
#[test]
fn len_reports_the_retained_sample_count() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(U, 1, 1, 1);
    w.push(2 * U, 2, 2, 2);
    assert_eq!(w.len(), 3);
}

/// A lone `late` decrease (repeats/resyncs unchanged) is a re-anchor reset: the
/// ring is cleared and restarted from the new sample, so `len == 1`.
///
/// Kills 70:33 `|| -> &&`: `(late<back.late && repeats<back.repeats) || ...`
/// evaluates to `(true && false) || false` = false → no reset → `len == 2`.
#[test]
fn a_lone_late_decrease_triggers_a_reset() {
    let mut w = EventWindow::new();
    w.push(0, 5, 5, 5);
    w.push(U, 3, 5, 5); // late 5 -> 3, others unchanged
    assert_eq!(
        w.len(),
        1,
        "reset clears the ring, keeping only the new sample"
    );
}

/// A lone `repeats` decrease is a re-anchor reset, so `len == 1`.
///
/// Kills 70:59 `|| -> &&`: `late<back.late || (repeats<back.repeats &&
/// resyncs<back.resyncs)` = `false || (true && false)` = false → no reset →
/// `len == 2`.
#[test]
fn a_lone_repeats_decrease_triggers_a_reset() {
    let mut w = EventWindow::new();
    w.push(0, 5, 5, 5);
    w.push(U, 5, 3, 5); // repeats 5 -> 3, others unchanged
    assert_eq!(
        w.len(),
        1,
        "reset clears the ring, keeping only the new sample"
    );
}

/// The eviction cutoff is `now - 60 s`. With samples at 0, 10 s, 75 s the t=10 s
/// sample ages out at now=75 s, leaving the left edge + newest = 2.
///
/// Kills 88:32 `- -> /`: `75s / 60s` (integer, in 100-ns units) collapses the
/// cutoff to ~0, so nothing is evicted and `len == 3`.
#[test]
fn evict_cutoff_uses_subtraction_not_division() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(10 * U, 0, 0, 0);
    w.push(75 * U, 0, 0, 0);
    assert_eq!(w.len(), 2, "one sample aged out of the 60 s window");
}

/// The time-eviction loop runs while `ring.len() > 1`. Same aging scenario.
///
/// Kills 92:31 `> -> <`: `ring.len() < 1` is always false right after a push
/// (len >= 1), so the loop never runs, nothing is evicted, and `len == 3`.
#[test]
fn evict_time_loop_runs_while_ring_has_more_than_one() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(10 * U, 0, 0, 0);
    w.push(75 * U, 0, 0, 0);
    assert_eq!(w.len(), 2, "the strictly-older sample is evicted");
}

/// A sample exactly at the window start (t=15 s == cutoff 75-60) is retained by
/// the strict `<` (inclusive left edge), so `len == 3`.
///
/// Kills 92:60 `< -> <=`: `15s <= 15s` would evict the boundary sample → `len == 2`.
#[test]
fn evict_keeps_a_sample_exactly_at_the_window_start() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(15 * U, 0, 0, 0);
    w.push(75 * U, 0, 0, 0);
    assert_eq!(
        w.len(),
        3,
        "a sample exactly at the window start is retained"
    );
}

/// A strictly-older sample (t=10 s < cutoff 80-60 = 20 s) IS evicted, so
/// `len == 2`.
///
/// Kills 92:60 `< -> ==`: `10s == 20s` is false → the strictly-older sample is
/// kept → `len == 3`.
#[test]
fn evict_drops_a_strictly_older_sample() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(10 * U, 0, 0, 0);
    w.push(80 * U, 0, 0, 0);
    assert_eq!(w.len(), 2, "a strictly-older sample is evicted");
}

/// 20 samples 1 s apart all fall inside the 60 s window (nothing ages out), so
/// only the hard cap bounds the ring — to EXACTLY `EVENT_WINDOW_CAP` (13).
///
/// Kills 96:31 `> -> ==` and `> -> >=`: both pop one sample too many whenever
/// the length reaches the cap, stabilising the ring at 12 instead of 13.
#[test]
fn hard_cap_trims_to_exactly_the_window_cap() {
    let mut w = EventWindow::new();
    for i in 0..20i64 {
        w.push(i * U, i as u64, i as u64, i as u64);
    }
    // EVENT_WINDOW_CAP is 13; a `> -> ==` / `> -> >=` mutant stabilises at 12.
    assert_eq!(
        w.len(),
        EVENT_WINDOW_CAP,
        "the ring is bounded to exactly the cap (13)"
    );
}
