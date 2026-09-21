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
// default_affinity_mask — the upper half of the logical cores.
// ---------------------------------------------------------------------------

#[test]
fn default_mask_is_upper_half_even_cores() {
    // 8 cores → upper half = cores 4..7 = 0b1111_0000.
    assert_eq!(default_affinity_mask(8), 0xF0);
    // 24 cores (the win-resolume box) → cores 12..23 = 0xFFF000.
    assert_eq!(default_affinity_mask(24), 0xFFF000);
    // 12 cores → cores 6..11 = 0xFC0.
    assert_eq!(default_affinity_mask(12), 0xFC0);
}

#[test]
fn default_mask_odd_cores_gives_upper_the_extra_core() {
    // 9 cores → lower half = 4 (cores 0..3), upper = 5 (cores 4..8) = 0x1F0.
    assert_eq!(default_affinity_mask(9), 0x1F0);
}

#[test]
fn default_mask_tiny_and_zero_core_counts() {
    // 1 core → the single core (bit 0). 0 (unreported) clamps up to 1 → same.
    assert_eq!(default_affinity_mask(1), 0b1);
    assert_eq!(default_affinity_mask(0), 0b1);
    // 2 cores → upper core only (bit 1).
    assert_eq!(default_affinity_mask(2), 0b10);
}

#[test]
fn default_mask_clamps_above_64_cores() {
    // > 64 clamps to 64 (one processor group): upper 32 = 0xFFFFFFFF00000000.
    assert_eq!(default_affinity_mask(64), 0xFFFFFFFF_00000000);
    assert_eq!(default_affinity_mask(65), 0xFFFFFFFF_00000000);
    assert_eq!(default_affinity_mask(1000), 0xFFFFFFFF_00000000);
}

// ---------------------------------------------------------------------------
// containment_from_settings — cap clamp + mask parse + memory-priority flag.
// (clamp_cap_pct / parse_affinity_mask are exercised through this seam and
//  directly below.)
// ---------------------------------------------------------------------------

#[test]
fn defaults_when_both_settings_absent() {
    let c = containment_from_settings(None, None, 24);
    assert_eq!(c.cpu_cap_pct, 25); // RED sentinel 99 fails here → GREEN 25
    assert_eq!(c.affinity_mask, 0xFFF000);
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
    // 8 cores → default upper half = 0xF0.
    assert_eq!(parse_affinity_mask(None, 8), 0xF0);
    // A zero mask is invalid (no cores) → default (kills the `m != 0` mutant).
    assert_eq!(parse_affinity_mask(Some("0"), 8), 0xF0);
    assert_eq!(parse_affinity_mask(Some("zzz"), 8), 0xF0);
    assert_eq!(parse_affinity_mask(Some(""), 8), 0xF0);
}

#[test]
fn containment_honours_explicit_overrides() {
    let c = containment_from_settings(Some("50"), Some("f000"), 24);
    assert_eq!(c.cpu_cap_pct, 50);
    assert_eq!(c.affinity_mask, 0xF000);
    assert!(c.memory_priority_low);
    // Clamp + zero-mask fallback compose through the seam.
    let c2 = containment_from_settings(Some("3"), Some("0"), 8);
    assert_eq!(c2.cpu_cap_pct, 5);
    assert_eq!(c2.affinity_mask, 0xF0);
}

#[test]
fn affinity_mask_hex_is_lowercase_no_prefix() {
    assert_eq!(affinity_mask_hex(0xFFF000), "fff000");
    assert_eq!(affinity_mask_hex(0xF0), "f0");
}
