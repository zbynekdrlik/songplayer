//! #162 pure tests for the stem-worker decision seams.
//!
//! These cover both `stem_defer_decision` mode arms and both
//! `separation_abort_armed` device cases, killing the surviving
//! `process_next` mutants (`==`/`!=` on the mode branch, `!` deletion on the
//! abort-arming branch) by testing the extracted decisions directly. Pure — no
//! DB, no worker instance, no async.

use super::*;
use crate::lyrics::idle_gate::WALL_IDLE_SETTLE;

fn playing() -> WallActivity {
    WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
    }
}

fn idle() -> WallActivity {
    WallActivity::default()
}

// ---- stem_defer_decision — both mode arms --------------------------------

#[test]
fn low_priority_never_defers_even_while_playing() {
    let mut log = GateLog::default();
    assert_eq!(
        stem_defer_decision(
            ProcessingMode::LowPriority,
            true,
            playing(),
            &mut log,
            Instant::now(),
        ),
        None,
        "low-priority runs at reduced priority — it never defers"
    );
}

#[test]
fn idle_only_playing_defers_with_busy_detail() {
    let mut log = GateLog::default();
    assert_eq!(
        stem_defer_decision(
            ProcessingMode::IdleOnly,
            true,
            playing(),
            &mut log,
            Instant::now(),
        ),
        Some("output playing"),
        "idle-only defers on a busy wall with the busy reason as detail"
    );
}

#[test]
fn idle_only_idle_but_unsettled_defers_with_settling_detail() {
    let mut log = GateLog::default();
    // First idle sample starts the settle clock → still deferring.
    assert_eq!(
        stem_defer_decision(
            ProcessingMode::IdleOnly,
            true,
            idle(),
            &mut log,
            Instant::now(),
        ),
        Some("wall just went idle — settling"),
        "a wall that just went idle keeps deferring until the settle window elapses"
    );
}

#[test]
fn idle_only_idle_past_settle_window_proceeds() {
    let mut log = GateLog::default();
    let t0 = Instant::now();
    // Seed the settle clock with the first idle sample (still deferring).
    assert!(
        stem_defer_decision(ProcessingMode::IdleOnly, true, idle(), &mut log, t0).is_some(),
        "first idle sample must still defer"
    );
    // Once the wall has read idle continuously for the full settle window, the
    // worker proceeds.
    let later = t0 + WALL_IDLE_SETTLE + Duration::from_secs(1);
    assert_eq!(
        stem_defer_decision(ProcessingMode::IdleOnly, true, idle(), &mut log, later),
        None,
        "after WALL_IDLE_SETTLE of continuous idle, idle-only stops deferring"
    );
}

// ---- separation_abort_armed — both device cases --------------------------

#[test]
fn abort_armed_only_for_gpu_plan() {
    assert!(
        !separation_abort_armed(&HeavyStepPlan::cpu_idle()),
        "a CPU-idle plan is never aborted — it cannot disturb the wall"
    );
    assert!(
        separation_abort_armed(&HeavyStepPlan::gpu_below_normal()),
        "a GPU plan runs under the mid-job wall-abort watcher"
    );
}
