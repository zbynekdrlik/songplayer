//! Truth-table + precedence tests for `genlock::lock_state` (#149, Lane 1,
//! contract §7 A7.3). RED-first: this file is wired from `genlock.rs` and
//! references `crate::genlock::lock_state`, which does not exist until the
//! GREEN commit — a documented compile-failure RED.

use crate::genlock::lock_state::{LockState, OutputLock, derive, summarize};

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

// ---- global summary (#150): summarize() the per-output lock into one badge ----
//
// summarize() is the WASM-safe reduction the dashboard's GlobalLockBadge
// (and, later, the API/log) share: LOCKED iff every LIVE output is LOCKED
// and clock ok; else the worst live state (UNLOCKED > DEGRADED > LOCKED)
// naming the worst output; no live output → clock-only. RED-first: these
// reference `OutputLock` / `summarize`, added in the GREEN commit.

fn ol(name: &str, state: LockState, live: bool, clock_ok: bool) -> OutputLock {
    OutputLock {
        name: name.to_string(),
        state,
        live,
        clock_ok,
    }
}

#[test]
fn summarize_all_live_locked_is_locked_no_worst() {
    let out = [
        ol("SP-a", LockState::Locked, true, true),
        ol("SP-b", LockState::Locked, true, true),
    ];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Locked);
    assert_eq!(s.worst, None);
    assert_eq!(s.live_count, 2);
}

#[test]
fn summarize_one_degraded_live_names_it() {
    let out = [
        ol("SP-a", LockState::Locked, true, true),
        ol("SP-b", LockState::Degraded, true, true),
    ];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Degraded);
    assert_eq!(s.worst.as_deref(), Some("SP-b"));
    assert_eq!(s.live_count, 2);
}

#[test]
fn summarize_unlocked_beats_degraded() {
    // Severity ordering UNLOCKED > DEGRADED: the unlocked output is the worst.
    let out = [
        ol("SP-a", LockState::Degraded, true, true),
        ol("SP-b", LockState::Unlocked, true, true),
    ];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Unlocked);
    assert_eq!(s.worst.as_deref(), Some("SP-b"));
    assert_eq!(s.live_count, 2);
}

#[test]
fn summarize_ignores_non_live_outputs() {
    // A non-live UNLOCKED output must not drag the summary down.
    let out = [
        ol("SP-a", LockState::Locked, true, true),
        ol("SP-b", LockState::Unlocked, false, true),
    ];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Locked);
    assert_eq!(s.worst, None);
    assert_eq!(s.live_count, 1);
}

#[test]
fn summarize_no_live_clock_ok_is_locked_none() {
    // No live output + box clock ok → LOCKED, no worst (rendered "no live output").
    let out = [ol("SP-a", LockState::Unlocked, false, true)];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Locked);
    assert_eq!(s.worst, None);
    assert_eq!(s.live_count, 0);
}

#[test]
fn summarize_no_live_clock_not_ok_is_unlocked() {
    // No live output + box clock not ok → UNLOCKED (clock-only).
    let out = [ol("SP-a", LockState::Unlocked, false, false)];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Unlocked);
    assert_eq!(s.worst, None);
    assert_eq!(s.live_count, 0);
}

#[test]
fn summarize_empty_is_locked_none() {
    // No outputs at all → clock vacuously ok → LOCKED, no worst.
    let s = summarize(&[]);
    assert_eq!(s.state, LockState::Locked);
    assert_eq!(s.worst, None);
    assert_eq!(s.live_count, 0);
}

#[test]
fn summarize_locked_but_clock_not_ok_is_not_locked() {
    // Defensive: a LOCKED output whose clock is not ok must not report LOCKED
    // (the engine never emits that combo, but the summary must be honest).
    let out = [ol("SP-a", LockState::Locked, true, false)];
    let s = summarize(&out);
    assert_eq!(s.state, LockState::Unlocked);
    assert_eq!(s.worst.as_deref(), Some("SP-a"));
    assert_eq!(s.live_count, 1);
}
