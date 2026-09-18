//! Tests for the #176 whole-box GLOBAL genlock summary (`global_lock_summary`),
//! the always-visible dashboard indicator incl. the explicit OFF state.
//!
//! TIER-0 RED (0.56.0-dev.2): the impl shipped with a wrong `OFF_REASON`
//! (`"pacing off"`), so `off_when_no_output_has_pacing_enabled` failed on the
//! reason assert while the whole function still compiled and used every field;
//! GREEN sets it to `"pacing vypnuté"`. Wired from `genlock.rs`; references the
//! public `lock_state` API.

use crate::genlock::lock_state::{GlobalLock, GlobalLockInput, LockState, global_lock_summary};

fn inp(
    pacing_enabled: bool,
    live: bool,
    state: LockState,
    clock_ok: bool,
    reason: &str,
) -> GlobalLockInput {
    GlobalLockInput {
        pacing_enabled,
        live,
        state,
        clock_ok,
        reason: reason.to_string(),
    }
}

#[test]
fn off_when_no_output_has_pacing_enabled() {
    // Production default: genlock_pacing=false everywhere → OFF, never hidden.
    let out = [
        inp(false, true, LockState::Unlocked, false, "pacing disabled"),
        inp(false, false, LockState::Unlocked, false, "pacing disabled"),
    ];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Off);
    assert_eq!(s.reason, "pacing vypnuté");
}

#[test]
fn off_when_empty() {
    let s = global_lock_summary(&[]);
    assert_eq!(s.state, GlobalLock::Off);
    assert_eq!(s.reason, "pacing vypnuté");
}

#[test]
fn off_even_if_a_stray_output_reports_locked_but_pacing_disabled() {
    // Defensive: pacing disabled dominates — a LOCKED lock_state with pacing off
    // is still OFF globally (the box free-runs).
    let out = [inp(false, true, LockState::Locked, true, "locked")];
    assert_eq!(global_lock_summary(&out).state, GlobalLock::Off);
}

#[test]
fn locked_when_all_live_pacing_enabled_are_locked() {
    let out = [
        inp(true, true, LockState::Locked, true, "locked"),
        inp(true, true, LockState::Locked, true, "locked"),
    ];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Locked);
    assert_eq!(s.reason, "locked");
}

#[test]
fn degraded_is_the_worst_of_the_live_outputs() {
    let out = [
        inp(true, true, LockState::Locked, true, "locked"),
        inp(true, true, LockState::Degraded, true, "no receiver"),
    ];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Degraded);
    assert_eq!(s.reason, "no receiver");
}

#[test]
fn unlocked_beats_degraded() {
    // worst-first ordering guards the max_by_key tie/flatten trap.
    let out = [
        inp(true, true, LockState::Unlocked, true, "clock not ok"),
        inp(true, true, LockState::Degraded, true, "no receiver"),
    ];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Unlocked);
    assert_eq!(s.reason, "clock not ok");
}

#[test]
fn locked_but_clock_not_ok_demotes_to_unlocked() {
    // A LOCKED output whose clock is not ok must never read LOCKED globally.
    let out = [inp(true, true, LockState::Locked, false, "locked")];
    assert_eq!(global_lock_summary(&out).state, GlobalLock::Unlocked);
}

#[test]
fn non_live_pacing_enabled_output_does_not_drag_a_live_locked_box_down() {
    // A non-live UNLOCKED (pacing enabled) output is ignored while a live LOCKED
    // output exists — only LIVE outputs decide the worst-of.
    let out = [
        inp(true, true, LockState::Locked, true, "locked"),
        inp(true, false, LockState::Unlocked, false, "pacing disabled"),
    ];
    assert_eq!(global_lock_summary(&out).state, GlobalLock::Locked);
}

#[test]
fn pacing_enabled_nothing_live_clock_ok_is_locked() {
    let out = [inp(true, false, LockState::Unlocked, true, "x")];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Locked);
    assert_eq!(s.reason, "no live output");
}

#[test]
fn pacing_enabled_nothing_live_clock_not_ok_is_unlocked() {
    let out = [inp(true, false, LockState::Unlocked, false, "x")];
    let s = global_lock_summary(&out);
    assert_eq!(s.state, GlobalLock::Unlocked);
    assert_eq!(s.reason, "clock not ok");
}
