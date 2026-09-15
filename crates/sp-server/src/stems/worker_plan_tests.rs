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

// ---- separation_timeout — the CPU ×4 scaling reaches the spawn seam -------

#[test]
fn separation_timeout_scales_only_the_cpu_plan() {
    // A 10.5-min song → base clamps to 1280 s (isolation_timeout); the exact
    // base is irrelevant here — assert the plan-scaling relative to it.
    let dur = Some(640_000);
    let base = isolation_timeout(dur);
    assert_eq!(
        separation_timeout(&HeavyStepPlan::gpu_below_normal(), dur),
        base,
        "a GPU separation keeps the base ceiling"
    );
    assert_eq!(
        separation_timeout(&HeavyStepPlan::cpu_idle(), dur),
        base * 4,
        "a CPU separation gets ×4 the base so it is not killed mid-run"
    );
}

// ---- stem_defer_fallback — the poisoned-lock path, all four cases ---------

// ---- stem_duration_supported / stem_duration_too_long — the 15-min cap ---

#[test]
fn duration_unknown_is_always_supported() {
    assert!(
        stem_duration_supported(None),
        "an unknown duration must never block separation"
    );
    assert!(!stem_duration_too_long(None));
}

#[test]
fn duration_at_and_under_the_cap_is_supported() {
    assert!(stem_duration_supported(Some(899_999)));
    assert!(stem_duration_supported(Some(STEM_MAX_DURATION_MS)));
    assert!(!stem_duration_too_long(Some(899_999)));
    assert!(!stem_duration_too_long(Some(STEM_MAX_DURATION_MS)));
}

#[test]
fn duration_one_ms_over_the_cap_is_unsupported() {
    assert!(!stem_duration_supported(Some(900_001)));
    assert!(stem_duration_too_long(Some(900_001)));
}

#[test]
fn defer_fallback_defers_only_idle_only_while_in_use() {
    assert!(
        stem_defer_fallback(ProcessingMode::IdleOnly, playing()),
        "idle-only + wall in use defers even without the settle clock"
    );
    assert!(
        !stem_defer_fallback(ProcessingMode::IdleOnly, idle()),
        "idle-only + idle wall proceeds"
    );
    assert!(
        !stem_defer_fallback(ProcessingMode::LowPriority, playing()),
        "low-priority never defers, in use or not"
    );
    assert!(!stem_defer_fallback(ProcessingMode::LowPriority, idle()));
}
