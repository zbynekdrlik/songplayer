//! #203 — unit tests for the pure containment decision core. Exact boundaries,
//! mutation-clean (no loops; every constant/operator has a distinguishing case).

use super::*;

// ---------------------------------------------------------------------------
// cpu_rate_from_pct — the Job Object CpuRate unit (pct * 100).
// ---------------------------------------------------------------------------

#[test]
fn cpu_rate_is_pct_times_100() {
    // Distinct multiplicands so a `* -> +`, `100 -> 1`, `100 -> 0` or
    // `100 -> 101` mutant all diverge from the expected product.
    assert_eq!(cpu_rate_from_pct(25), 2500);
    assert_eq!(cpu_rate_from_pct(5), 500);
    assert_eq!(cpu_rate_from_pct(100), 10000);
    assert_eq!(cpu_rate_from_pct(1), 100);
}

// ---------------------------------------------------------------------------
// default_affinity_mask — the TOP 3 logical cores (#168 round 8; was the top 4
// in round 5). Round 7's paced measurement (comment 5786765465) put a 3-core /
// 3-thread block (≈ 1.0–1.1 core) at receiver `dropped_due` 0.27–0.5/min with
// the sender ≤ 20 ms in 16/16 minutes vs 0.9–1.35/min for the 4-core / 4-thread
// block (≈ 1.5–2.0 cores).
// ---------------------------------------------------------------------------

#[test]
fn default_mask_is_the_top_3_logical_cores() {
    // 24 cores (the win-resolume box) → cores 21..23 = 0xE00000 (the measured
    // best resident-child block; was 0xF00000 before round 8).
    assert_eq!(default_affinity_mask(24), 0xE00000);
    // 8 cores → cores 5..7 = 0xE0 (was 0xF0 before round 8).
    assert_eq!(default_affinity_mask(8), 0xE0);
    // 6 cores → cores 3..5 = 0x38 (was 0x3C).
    assert_eq!(default_affinity_mask(6), 0x38);
    // 12 cores → cores 9..11 = 0xE00 (was 0xF00).
    assert_eq!(default_affinity_mask(12), 0xE00);
    // 4 cores → the top 3 of 4 = 0xE (a 4-core box no longer gets all four).
    assert_eq!(default_affinity_mask(4), 0xE);
}

#[test]
fn default_mask_boxes_with_3_or_fewer_cores_get_all_of_them() {
    // 3 cores → all three (top 3 of 3) = 0x7.
    assert_eq!(default_affinity_mask(3), 0x7);
    // 2 cores → both cores (< 3, so all) = 0x3.
    assert_eq!(default_affinity_mask(2), 0x3);
    // 1 core → the single core. 0 (unreported) clamps up to 1 → same.
    assert_eq!(default_affinity_mask(1), 0x1);
    assert_eq!(default_affinity_mask(0), 0x1);
}

#[test]
fn default_mask_clamps_above_64_cores_to_the_top_3() {
    // > 64 clamps to 64 (one processor group): top 3 = cores 61..63 =
    // 0xE000_0000_0000_0000.
    assert_eq!(default_affinity_mask(64), 0xE000_0000_0000_0000);
    assert_eq!(default_affinity_mask(65), 0xE000_0000_0000_0000);
    assert_eq!(default_affinity_mask(100), 0xE000_0000_0000_0000);
    assert_eq!(default_affinity_mask(1000), 0xE000_0000_0000_0000);
}

// ---------------------------------------------------------------------------
// containment_from_settings — cap clamp + mask parse + memory-priority flag.
// (clamp_cap_pct / parse_affinity_mask are exercised through this seam and
//  directly below.)
// ---------------------------------------------------------------------------

#[test]
fn defaults_when_both_settings_absent() {
    let c = containment_from_settings(None, None, None, 24);
    assert_eq!(c.cpu_cap_pct, 25); // RED sentinel 99 fails here → GREEN 25
    assert_eq!(c.affinity_mask, 0xE00000); // #168 round 8: top 3 logical cores
    assert!(c.memory_priority_low);
}

#[test]
fn cap_absent_or_garbage_is_default_25() {
    assert_eq!(clamp_cap_pct(None), 25);
    assert_eq!(clamp_cap_pct(Some("abc")), 25);
    assert_eq!(clamp_cap_pct(Some("")), 25);
}

#[test]
fn cap_clamps_to_inclusive_5_and_100() {
    // In-range values pass through.
    assert_eq!(clamp_cap_pct(Some("5")), 5);
    assert_eq!(clamp_cap_pct(Some("100")), 100);
    assert_eq!(clamp_cap_pct(Some("50")), 50);
    // Below the floor clamps up to 5 (kills a `min` boundary mutant).
    assert_eq!(clamp_cap_pct(Some("4")), 5);
    assert_eq!(clamp_cap_pct(Some("0")), 5);
    // Above the ceiling clamps down to 100 (kills a `max` boundary mutant).
    assert_eq!(clamp_cap_pct(Some("101")), 100);
    assert_eq!(clamp_cap_pct(Some("200")), 100);
}

#[test]
fn cap_trims_whitespace() {
    // A `delete .trim()` mutant leaves the leading space → parse fails → 25.
    assert_eq!(clamp_cap_pct(Some(" 30 ")), 30);
}

#[test]
fn mask_parses_hex_with_and_without_prefix() {
    assert_eq!(parse_affinity_mask(Some("f0"), 8), 0xF0);
    assert_eq!(parse_affinity_mask(Some("0xF0"), 8), 0xF0);
    assert_eq!(parse_affinity_mask(Some("0X0f"), 8), 0x0F);
    assert_eq!(parse_affinity_mask(Some("3"), 8), 0x3);
    assert_eq!(parse_affinity_mask(Some(" f0 "), 8), 0xF0);
}

#[test]
fn mask_absent_zero_or_garbage_falls_back_to_default() {
    // 8 cores → default (top 3 of 8) = 0xE0.
    assert_eq!(parse_affinity_mask(None, 8), 0xE0);
    // A zero mask is invalid (no cores) → default (kills the `m != 0` mutant).
    assert_eq!(parse_affinity_mask(Some("0"), 8), 0xE0);
    assert_eq!(parse_affinity_mask(Some("zzz"), 8), 0xE0);
    assert_eq!(parse_affinity_mask(Some(""), 8), 0xE0);
}

#[test]
fn containment_honours_explicit_overrides() {
    let c = containment_from_settings(Some("50"), Some("f000"), None, 24);
    assert_eq!(c.cpu_cap_pct, 50);
    assert_eq!(c.affinity_mask, 0xF000);
    assert!(c.memory_priority_low);
    // Clamp + zero-mask fallback compose through the seam (default = top 3 of 8).
    let c2 = containment_from_settings(Some("3"), Some("0"), None, 8);
    assert_eq!(c2.cpu_cap_pct, 5);
    assert_eq!(c2.affinity_mask, 0xE0);
}

#[test]
fn affinity_mask_hex_is_lowercase_no_prefix() {
    assert_eq!(affinity_mask_hex(0xE00000), "e00000");
    assert_eq!(affinity_mask_hex(0xF0), "f0");
}

/// 0.63.0 integration review: an operator override with bits beyond the box's
/// logical cores would fail the single extended-limit SetInformationJobObject
/// call on Windows (and drop the memory ceiling with it) — the override is
/// clamped to the cores that exist; an override with NO valid bit falls back
/// to the default mask.
#[test]
fn affinity_override_is_clamped_to_the_existing_cores() {
    // 8 cores: bits 0..=7 exist; 0xF0F0 keeps only its valid low bits 0xF0
    // (an explicit override keeps its valid bits verbatim — NOT the default).
    let c = containment_from_settings(None, Some("0xF0F0"), None, 8);
    assert_eq!(c.affinity_mask, 0xF0);
    // A partly-valid override keeps its valid bits only.
    let c = containment_from_settings(None, Some("0x10C"), None, 8);
    assert_eq!(c.affinity_mask, 0x0C);
    // Entirely beyond the core count → the default (top 3 of 8 = 0xE0).
    let c = containment_from_settings(None, Some("0xF00"), None, 8);
    assert_eq!(c.affinity_mask, 0xE0);
    // 64+ cores: every bit is valid, the override is honoured verbatim.
    let c = containment_from_settings(None, Some("0xFFFFFFFFFFFFFFFF"), None, 64);
    assert_eq!(c.affinity_mask, u64::MAX);
}

// ---------------------------------------------------------------------------
// #207 — parse_purge_delay_ms (heavy_purge_delay_ms setting) + the Containment
// purge field. Valid = -1 (never decommit) or 0..=600_000 ms; else fall back.
// ---------------------------------------------------------------------------

#[test]
fn purge_delay_absent_or_garbage_is_never_decommit() {
    // Missing / unparseable → the never-decommit default (-1).
    assert_eq!(parse_purge_delay_ms(None), -1);
    assert_eq!(parse_purge_delay_ms(Some("abc")), -1);
    assert_eq!(parse_purge_delay_ms(Some("")), -1);
}

#[test]
fn purge_delay_valid_values_pass_through() {
    assert_eq!(parse_purge_delay_ms(Some("-1")), -1);
    assert_eq!(parse_purge_delay_ms(Some("0")), 0);
    assert_eq!(parse_purge_delay_ms(Some("1000")), 1000);
    assert_eq!(parse_purge_delay_ms(Some("600000")), 600_000);
    // A `delete .trim()` mutant leaves the spaces → parse fails → default.
    assert_eq!(parse_purge_delay_ms(Some(" 250 ")), 250);
}

#[test]
fn purge_delay_out_of_range_falls_back_to_never() {
    // Above the 600 000 ms cap or below -1 → default (-1).
    assert_eq!(parse_purge_delay_ms(Some("600001")), -1);
    assert_eq!(parse_purge_delay_ms(Some("-2")), -1);
}

#[test]
fn containment_carries_the_parsed_purge_delay() {
    // Absent → never-decommit default; explicit in-range → verbatim.
    assert_eq!(
        containment_from_settings(None, None, None, 24).purge_delay_ms,
        -1
    );
    assert_eq!(
        containment_from_settings(None, None, Some("1000"), 24).purge_delay_ms,
        1000
    );
}
