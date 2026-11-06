//! #162 pure tests for the stem-worker decision seams. Pure — no DB, no
//! worker instance, no async. (The mid-job wall-abort arming is folded into
//! the #184 G0.1 `worker_yield::yield_reason`, unit-tested there.)

use super::*;

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

// ---- stem_duration_supported / stem_duration_too_long — the 120-min cap ---

#[test]
fn cap_is_120_min() {
    // Round G0 (owner ruling 21.9.2026): every video, incl. a long dub video,
    // gets podklad/vokály stems — the cap is now a 120-min sanity ceiling.
    // Asserted against the exact literal (worker.rs keeps the const a literal so
    // the mutation runner can see it).
    assert_eq!(STEM_MAX_DURATION_MS, 7_200_000);
}

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
    // A 36-min dub video (e.g. video 344) is separated now.
    assert!(stem_duration_supported(Some(36 * 60_000)));
    assert!(stem_duration_supported(Some(STEM_MAX_DURATION_MS)));
    assert!(!stem_duration_too_long(Some(36 * 60_000)));
    assert!(!stem_duration_too_long(Some(STEM_MAX_DURATION_MS)));
}

#[test]
fn duration_over_the_cap_is_unsupported() {
    // 121 min is past the 120-min ceiling → still skipped.
    assert!(!stem_duration_supported(Some(121 * 60_000)));
    assert!(stem_duration_too_long(Some(121 * 60_000)));
}
