//! Truth-table + precedence + calibration tests for `genlock::lock_state`
//! (#149, Lane 1, contract §7 A7.3; rate-normalised rule #168 round 6). Wired
//! from `genlock.rs`; references `crate::genlock::lock_state`.

use crate::genlock::lock_state::{
    LockInputs, LockState, OutputLock, derive, expected_repeat_permille, summarize,
};

/// A healthy LOCKED base: clock ok, pacing on, a receiver, no window events, a
/// full minute of 24-fps-on-30-grid slots. Tests override single fields with
/// struct-update syntax so no 9-arg helper is needed (`too_many_arguments`).
fn base() -> LockInputs {
    LockInputs {
        clock_ok: true,
        pacing_enabled: true,
        connections: 2,
        late_w: 0,
        repeats_w: 0,
        resyncs_w: 0,
        slots_w: 1800,
        source_fps: 24.0,
        grid_fps: 30,
    }
}

// ---- the precedence branches, each with its exact reason ----

#[test]
fn unlocked_when_clock_not_ok() {
    // clock_ok=false wins even with pacing enabled + a receiver + no events.
    let (s, r) = derive(&LockInputs {
        clock_ok: false,
        ..base()
    });
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "clock not ok");
}

#[test]
fn unlocked_when_pacing_disabled() {
    // clock ok but pacing OFF (today's flag-OFF steady state).
    let (s, r) = derive(&LockInputs {
        pacing_enabled: false,
        ..base()
    });
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "pacing disabled");
}

#[test]
fn degraded_when_no_receiver() {
    let (s, r) = derive(&LockInputs {
        connections: 0,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "no receiver");
}

#[test]
fn degraded_when_resync_in_window() {
    // A single resync is a hard event (0 everywhere in the calibration).
    let (s, r) = derive(&LockInputs {
        resyncs_w: 1,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "resync in 60 s");
}

#[test]
fn resync_degrades_even_with_zero_late() {
    // Acceptance: resyncs 1 → DEGRADED (resync) regardless of the late count.
    let (s, r) = derive(&LockInputs {
        resyncs_w: 1,
        late_w: 0,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "resync in 60 s");
}

#[test]
fn locked_when_slots_zero() {
    // Nothing emitted (paused / idle) → no grid to break → LOCKED, even with
    // stray non-zero late/repeats: the slots==0 guard short-circuits them.
    let (s, r) = derive(&LockInputs {
        slots_w: 0,
        late_w: 5000,
        repeats_w: 5000,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

#[test]
fn locked_when_all_clear() {
    let (s, r) = derive(&base());
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

// ---- calibration (22.9.2026 data: 1800 slots/min, 24 fps on the 30 fps grid) ----

#[test]
fn late_100_is_locked() {
    // Clean grid: 100 late / 1800 slots ≈ 5.6 % ≤ 25 % → LOCKED.
    let (s, r) = derive(&LockInputs {
        late_w: 100,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

#[test]
fn late_750_is_degraded() {
    // Sender-side stall: 750 late / 1800 slots ≈ 42 % > 25 % → DEGRADED.
    let (s, r) = derive(&LockInputs {
        late_w: 750,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late > 25 % of slots in 60 s");
}

#[test]
fn repeats_360_is_locked_structural() {
    // The by-design 24→30 conversion: 360 / 1800 = 20 % = expected → LOCKED.
    let (s, r) = derive(&LockInputs {
        repeats_w: 360,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

#[test]
fn repeats_600_is_degraded() {
    // Above the conversion + 10 % margin (expected 200 ‰ + 100 ‰ = 300 ‰ →
    // 540 slots): 600 > 540 → DEGRADED (starvation).
    let (s, r) = derive(&LockInputs {
        repeats_w: 600,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "repeats above the fps conversion in 60 s");
}

#[test]
fn source_30_repeats_100_is_locked() {
    // A 30-fps source has 0 structural repeats: threshold is the 10 % margin
    // (180 slots). 100 < 180 → LOCKED.
    let (s, r) = derive(&LockInputs {
        source_fps: 30.0,
        repeats_w: 100,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

#[test]
fn source_30_repeats_200_is_degraded() {
    // 200 > the 180-slot margin → DEGRADED.
    let (s, r) = derive(&LockInputs {
        source_fps: 30.0,
        repeats_w: 200,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "repeats above the fps conversion in 60 s");
}

// ---- boundaries: exactly-at-threshold is LOCKED (strict `>`) ----

#[test]
fn late_exactly_at_threshold_is_locked() {
    // late_w * 1000 == LATE_DEGRADED_PERMILLE * slots_w (450 * 1000 == 250 * 1800).
    let (s, _) = derive(&LockInputs {
        late_w: 450,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    // One more late tips it over.
    let (s2, r2) = derive(&LockInputs {
        late_w: 451,
        ..base()
    });
    assert_eq!(s2, LockState::Degraded);
    assert_eq!(r2, "late > 25 % of slots in 60 s");
}

#[test]
fn repeats_exactly_at_threshold_is_locked() {
    // repeats_w * 1000 == (expected 200 + margin 100) * 1800 (540 * 1000).
    let (s, _) = derive(&LockInputs {
        repeats_w: 540,
        ..base()
    });
    assert_eq!(s, LockState::Locked);
    let (s2, r2) = derive(&LockInputs {
        repeats_w: 541,
        ..base()
    });
    assert_eq!(s2, LockState::Degraded);
    assert_eq!(r2, "repeats above the fps conversion in 60 s");
}

// ---- precedence chain: clock > pacing > receiver > resync > late > repeats ----

#[test]
fn clock_beats_pacing() {
    let (s, r) = derive(&LockInputs {
        clock_ok: false,
        pacing_enabled: false,
        ..base()
    });
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "clock not ok");
}

#[test]
fn pacing_beats_receiver() {
    let (s, r) = derive(&LockInputs {
        pacing_enabled: false,
        connections: 0,
        ..base()
    });
    assert_eq!(s, LockState::Unlocked);
    assert_eq!(r, "pacing disabled");
}

#[test]
fn receiver_beats_resync() {
    let (s, r) = derive(&LockInputs {
        connections: 0,
        resyncs_w: 5,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "no receiver");
}

#[test]
fn resync_beats_late() {
    // resync + a stall-level late → resync wins (it is the harder event).
    let (s, r) = derive(&LockInputs {
        resyncs_w: 1,
        late_w: 900,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "resync in 60 s");
}

#[test]
fn late_beats_repeats() {
    // Both a late-stall AND above-conversion repeats → the late reason wins.
    let (s, r) = derive(&LockInputs {
        late_w: 900,
        repeats_w: 900,
        ..base()
    });
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late > 25 % of slots in 60 s");
}

// ---- expected_repeat_permille (the structural fps-conversion rate) ----

#[test]
fn expected_repeat_permille_values() {
    assert_eq!(expected_repeat_permille(24.0, 30), 200);
    assert_eq!(expected_repeat_permille(25.0, 30), 167);
    assert_eq!(expected_repeat_permille(30.0, 30), 0);
    assert_eq!(expected_repeat_permille(60.0, 30), 0);
}

#[test]
fn expected_repeat_permille_guards_zero_grid() {
    // A zero grid (or non-positive source) is defended, never a divide-by-zero.
    assert_eq!(expected_repeat_permille(24.0, 0), 0);
    assert_eq!(expected_repeat_permille(0.0, 30), 0);
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
// naming the worst output; no live output → clock-only.

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
