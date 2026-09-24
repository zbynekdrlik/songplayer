//! #147 round 9 — pure tests for SongPlayer's hard minimum working set
//! (`process_residency.rs`). Both sides of every clamp boundary are pinned so
//! the diff-scoped mutation gate kills every comparison / arithmetic mutant.

use super::*;

// ---- mb_to_bytes / bytes_to_mb --------------------------------------------

#[test]
fn mb_to_bytes_is_mebibytes() {
    assert_eq!(mb_to_bytes(0), 0);
    assert_eq!(mb_to_bytes(1), 1_048_576);
    assert_eq!(mb_to_bytes(3072), 3_221_225_472);
    // u32::MAX MiB must not overflow on a 64-bit target.
    assert_eq!(mb_to_bytes(u32::MAX), 4_503_599_626_321_920);
}

#[test]
fn bytes_to_mb_rounds_down() {
    assert_eq!(bytes_to_mb(1_048_576), 1);
    assert_eq!(bytes_to_mb(1_048_575), 0, "just under one MiB");
    assert_eq!(bytes_to_mb(3_221_225_472), 3072);
    assert_eq!(bytes_to_mb(0), 0);
}

// ---- parse_min_working_set_mb ---------------------------------------------

#[test]
fn parse_absent_is_the_default() {
    assert_eq!(parse_min_working_set_mb(None), SP_MIN_WS_DEFAULT_MB);
    assert_eq!(SP_MIN_WS_DEFAULT_MB, 3072, "documented default");
}

#[test]
fn parse_zero_disables() {
    assert_eq!(parse_min_working_set_mb(Some("0")), 0);
    assert_eq!(parse_min_working_set_mb(Some(" 0 ")), 0, "trimmed");
}

#[test]
fn parse_garbage_and_negative_fall_back_to_default() {
    assert_eq!(parse_min_working_set_mb(Some("")), 3072);
    assert_eq!(parse_min_working_set_mb(Some("lots")), 3072);
    assert_eq!(parse_min_working_set_mb(Some("2048MB")), 3072);
    assert_eq!(parse_min_working_set_mb(Some("-1")), 3072);
    assert_eq!(parse_min_working_set_mb(Some("-4096")), 3072);
    // Beyond i64 → unparseable → default (not the ceiling).
    assert_eq!(
        parse_min_working_set_mb(Some("99999999999999999999999")),
        3072
    );
}

#[test]
fn parse_clamps_up_to_the_floor_on_both_sides_of_it() {
    assert_eq!(SP_MIN_WS_FLOOR_MB, 256);
    assert_eq!(parse_min_working_set_mb(Some("1")), 256, "1 → floor");
    assert_eq!(parse_min_working_set_mb(Some("255")), 256, "just below");
    assert_eq!(parse_min_working_set_mb(Some("256")), 256, "at the floor");
    assert_eq!(parse_min_working_set_mb(Some("257")), 257, "just above");
}

#[test]
fn parse_clamps_down_to_the_ceiling_on_both_sides_of_it() {
    assert_eq!(SP_MIN_WS_CEIL_MB, 8192);
    assert_eq!(parse_min_working_set_mb(Some("8191")), 8191, "just below");
    assert_eq!(
        parse_min_working_set_mb(Some("8192")),
        8192,
        "at the ceiling"
    );
    assert_eq!(parse_min_working_set_mb(Some("8193")), 8192, "just above");
    assert_eq!(parse_min_working_set_mb(Some("9999999999")), 8192, "huge");
    assert_eq!(parse_min_working_set_mb(Some(" 4096 ")), 4096, "trimmed");
}

// ---- min_working_set_setting_ignored --------------------------------------

#[test]
fn setting_ignored_only_for_a_present_value_not_used_as_written() {
    assert!(
        !min_working_set_setting_ignored(None),
        "absent → default, no warn"
    );
    assert!(
        !min_working_set_setting_ignored(Some("0")),
        "0 disables, valid"
    );
    assert!(
        !min_working_set_setting_ignored(Some("256")),
        "floor is valid"
    );
    assert!(
        !min_working_set_setting_ignored(Some("8192")),
        "ceiling is valid"
    );
    assert!(
        !min_working_set_setting_ignored(Some(" 3072 ")),
        "trimmed valid"
    );
    assert!(min_working_set_setting_ignored(Some("255")), "clamped up");
    assert!(
        min_working_set_setting_ignored(Some("8193")),
        "clamped down"
    );
    assert!(min_working_set_setting_ignored(Some("1")), "clamped up");
    assert!(min_working_set_setting_ignored(Some("-1")), "negative");
    assert!(min_working_set_setting_ignored(Some("abc")), "garbage");
    assert!(min_working_set_setting_ignored(Some("")), "empty");
}

// ---- plan_hard_min + the flags word ---------------------------------------

#[test]
fn hard_min_flags_are_hard_min_soft_max() {
    // QUOTA_LIMITS_HARDWS_MIN_ENABLE (0x1) | QUOTA_LIMITS_HARDWS_MAX_DISABLE (0x8).
    assert_eq!(QUOTA_HARDWS_MIN_ENABLE, 0x1);
    assert_eq!(QUOTA_HARDWS_MAX_DISABLE, 0x8);
    assert_eq!(HARD_MIN_FLAGS, 0x9);
}

#[test]
fn plan_zero_is_disabled() {
    assert_eq!(plan_hard_min(0), None);
}

#[test]
fn plan_sets_hard_min_and_a_soft_max_of_twice_the_min() {
    assert_eq!(
        plan_hard_min(3072),
        Some(WorkingSetPlan {
            min_bytes: 3_221_225_472,
            max_bytes: 6_442_450_944,
            flags: 0x9,
        })
    );
    // The smallest non-zero plan (1 MiB) — `max ≥ min` always holds.
    assert_eq!(
        plan_hard_min(1),
        Some(WorkingSetPlan {
            min_bytes: 1_048_576,
            max_bytes: 2_097_152,
            flags: 0x9,
        })
    );
}

// ---- residency_line --------------------------------------------------------

#[test]
fn residency_line_disabled() {
    assert_eq!(
        residency_line(None, Ok(()), Ok(())),
        "sp working set: hard_min disabled (sp_min_working_set_mb=0)"
    );
}

#[test]
fn residency_line_ok() {
    let p = plan_hard_min(3072).unwrap();
    assert_eq!(
        residency_line(Some(&p), Ok(()), Ok(())),
        "sp working set: hard_min_mb=3072 max_mb=6144 flags=0x9 privilege=ok result=ok"
    );
}

#[test]
fn residency_line_carries_each_last_error_in_its_own_field() {
    let p = plan_hard_min(4096).unwrap();
    // Distinct codes so a privilege/result swap mutant diverges.
    assert_eq!(
        residency_line(Some(&p), Err(1300), Err(1450)),
        "sp working set: hard_min_mb=4096 max_mb=8192 flags=0x9 privilege=failed(err=1300) result=failed(err=1450)"
    );
    assert_eq!(
        residency_line(Some(&p), Err(1300), Ok(())),
        "sp working set: hard_min_mb=4096 max_mb=8192 flags=0x9 privilege=failed(err=1300) result=ok"
    );
}

#[test]
fn setting_key_is_the_documented_name() {
    assert_eq!(SETTING_KEY, "sp_min_working_set_mb");
}
