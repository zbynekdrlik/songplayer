//! #147 round 9 — pure tests for SongPlayer's page-fault / working-set gauge
//! (`proc_mem.rs`). Exact values on both sides of every boundary so the
//! diff-scoped mutation gate kills each comparison and arithmetic mutant.

use super::*;

const MIB: u64 = 1_048_576;

// ---- fault_delta -----------------------------------------------------------

#[test]
fn fault_delta_is_the_forward_distance_on_a_monotonic_counter() {
    assert_eq!(fault_delta(1_000, 4_000), 3_000);
    assert_eq!(fault_delta(7, 7), 0, "no faults");
}

/// PageFaultCount is a u32 that WRAPS (≈ every 2.4 h at the box's 500k/s);
/// a decrease is a wrap, never a reset, so the true distance is counted
/// across u32::MAX.
#[test]
fn fault_delta_counts_across_a_u32_wrap() {
    assert_eq!(fault_delta(u32::MAX - 9, 5), 15);
    assert_eq!(fault_delta(u32::MAX, 0), 1);
    assert_eq!(
        fault_delta(1, 0),
        u32::MAX as u64,
        "a decrease = one full wrap"
    );
}

// ---- per_minute ------------------------------------------------------------

#[test]
fn per_minute_normalises_to_sixty_seconds() {
    assert_eq!(per_minute(6_000, 60_000), 6_000, "exactly one minute");
    assert_eq!(per_minute(6_000, 120_000), 3_000, "two minutes → half");
    assert_eq!(per_minute(1_000, 30_000), 2_000, "half a minute → double");
    assert_eq!(per_minute(7, 61_000), 6, "integer division rounds down");
}

#[test]
fn per_minute_zero_window_is_zero_not_a_panic() {
    assert_eq!(per_minute(5_000, 0), 0);
}

#[test]
fn per_minute_saturates_instead_of_overflowing() {
    assert_eq!(per_minute(u64::MAX, 60_000), u64::MAX / 60_000);
}

// ---- FaultWindow -----------------------------------------------------------

#[test]
fn first_reading_only_arms_the_window() {
    let mut w = FaultWindow::default();
    assert!(w.due(0), "nothing read yet → due");
    assert_eq!(
        w.observe(1_000, 512 * MIB, 0),
        None,
        "no delta on the first sample"
    );
    assert_eq!(w.latest(), None);
}

#[test]
fn due_flips_exactly_at_the_sample_period() {
    let mut w = FaultWindow::default();
    w.observe(0, 0, 10_000);
    assert_eq!(SAMPLE_PERIOD_MS, 60_000);
    assert!(!w.due(69_999), "one ms short");
    assert!(w.due(70_000), "exactly one period");
    assert!(w.due(70_001));
    assert!(!w.due(5_000), "a clock that went backwards is not due");
}

#[test]
fn a_full_minute_yields_faults_per_minute_and_working_set_mb() {
    let mut w = FaultWindow::default();
    w.observe(1_000, 100 * MIB, 0);
    let g = w.observe(31_000, 2_300 * MIB, 60_000);
    assert_eq!(
        g,
        Some(ProcMemGauge {
            page_faults_per_min: 30_000,
            working_set_mb: 2_300,
        })
    );
    assert_eq!(w.latest(), g);
}

#[test]
fn a_reading_inside_the_period_keeps_the_previous_gauge_and_baseline() {
    let mut w = FaultWindow::default();
    w.observe(0, 0, 0);
    let g1 = w.observe(60_000, 1_024 * MIB, 60_000);
    // 59.999 s later: ignored — neither the gauge nor the baseline moves.
    assert_eq!(w.observe(999_999, 9 * MIB, 119_999), g1);
    // At a full period after the baseline (60 000 → 120 000) the delta is
    // measured from the BASELINE reading (60 000), not the ignored one.
    let g2 = w.observe(90_000, 2_048 * MIB, 120_000);
    assert_eq!(
        g2,
        Some(ProcMemGauge {
            page_faults_per_min: 30_000,
            working_set_mb: 2_048,
        })
    );
}

#[test]
fn a_late_sample_is_normalised_over_the_actual_window() {
    let mut w = FaultWindow::default();
    w.observe(0, 0, 0);
    // 90 s window (a heartbeat gap): 45 000 faults → 30 000/min.
    let g = w.observe(45_000, 0, 90_000).unwrap();
    assert_eq!(g.page_faults_per_min, 30_000);
    assert_eq!(g.working_set_mb, 0);
}

#[test]
fn the_window_survives_a_counter_wrap() {
    let mut w = FaultWindow::default();
    w.observe(u32::MAX - 999, 0, 0);
    let g = w.observe(1_000, 0, 60_000).unwrap();
    assert_eq!(
        g.page_faults_per_min, 2_000,
        "999 + 1 + 1000 faults across the wrap"
    );
}

// ---- fmt_opt ---------------------------------------------------------------

#[test]
fn fmt_opt_renders_na_before_the_first_minute() {
    assert_eq!(fmt_opt(None), "na");
    assert_eq!(fmt_opt(Some(0)), "0");
    assert_eq!(fmt_opt(Some(48_213)), "48213");
}

// ---- gauge (off Windows) ---------------------------------------------------

#[cfg(not(windows))]
#[test]
fn gauge_is_none_off_windows() {
    assert_eq!(gauge(), None);
}
