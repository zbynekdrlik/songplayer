//! Truth-table + precedence tests for `genlock::lock_state` (#149, Lane 1,
//! contract §7 A7.3). RED-first: this file is wired from `genlock.rs` and
//! references `crate::genlock::lock_state`, which does not exist until the
//! GREEN commit — a documented compile-failure RED.

use crate::genlock::lock_state::{LockState, derive};

// ---- the five branches, each with its exact reason ----

#[test]
fn unlocked_when_clock_not_ok() {
    // clock_ok=false wins even with pacing enabled + a receiver + no events.
    let (s, r) = derive(false, true, 3, 0, 0, 0);
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "clock not ok");
}

#[test]
fn unlocked_when_pacing_disabled() {
    // clock ok but pacing OFF (today's flag-OFF steady state).
    let (s, r) = derive(true, false, 3, 0, 0, 0);
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "pacing disabled");
}

#[test]
fn degraded_when_no_receiver() {
    let (s, r) = derive(true, true, 0, 0, 0, 0);
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "no receiver");
}

#[test]
fn degraded_when_late_in_window() {
    let (s, r) = derive(true, true, 2, 1, 0, 0);
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late/repeats/resyncs in 60 s");
}

#[test]
fn degraded_when_repeats_in_window() {
    let (s, r) = derive(true, true, 2, 0, 5, 0);
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late/repeats/resyncs in 60 s");
}

#[test]
fn degraded_when_resyncs_in_window() {
    let (s, r) = derive(true, true, 2, 0, 0, 1);
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late/repeats/resyncs in 60 s");
}

#[test]
fn locked_when_all_clear() {
    let (s, r) = derive(true, true, 1, 0, 0, 0);
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

// ---- precedence: earlier conditions beat later ones (declared order) ----

#[test]
fn clock_not_ok_beats_pacing_disabled() {
    // Both !clock_ok and !pacing_enabled hold → the clock reason wins.
    let (s, r) = derive(false, false, 0, 9, 9, 9);
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "clock not ok");
}

#[test]
fn pacing_disabled_beats_no_receiver() {
    // clock ok, pacing OFF, and connections==0 → pacing reason wins.
    let (s, r) = derive(true, false, 0, 9, 9, 9);
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "pacing disabled");
}

#[test]
fn no_receiver_beats_window_events() {
    // clock ok, pacing ON, connections==0 AND window events → no-receiver wins.
    let (s, r) = derive(true, true, 0, 7, 7, 7);
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "no receiver");
}

// ---- vocabulary (camera-box#1298: LOCKED / DEGRADED / UNLOCKED) ----

#[test]
fn as_str_matches_serde_vocabulary() {
    assert_eq!(LockState::Locked.as_str(), "LOCKED");
    assert_eq!(LockState::Degraded.as_str(), "DEGRADED");
    assert_eq!(LockState::Unlocked.as_str(), "UNLOCKED");
}

#[test]
fn serde_renames_to_uppercase_states() {
    assert_eq!(
        serde_json::to_string(&LockState::Locked).unwrap(),
        "\"LOCKED\""
    );
    assert_eq!(
        serde_json::to_string(&LockState::Degraded).unwrap(),
        "\"DEGRADED\""
    );
    assert_eq!(
        serde_json::to_string(&LockState::Unlocked).unwrap(),
        "\"UNLOCKED\""
    );
    let back: LockState = serde_json::from_str("\"DEGRADED\"").unwrap();
    assert_eq!(back, LockState::Degraded);
}
