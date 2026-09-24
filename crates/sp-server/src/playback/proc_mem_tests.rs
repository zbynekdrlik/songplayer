//! #147 round 9 — pure tests for SongPlayer's page-fault / working-set gauge
//! (`proc_mem.rs`). Exact values on both sides of every boundary so the
//! diff-scoped mutation gate kills each comparison and arithmetic mutant.

use super::*;

const MIB: u64 = 1_048_576;

// (The wrap-safe `fault_delta` lives in `process_residency` and is tested in
// `process_residency_tests.rs`.)

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

/// A gap of exactly MAX_SAMPLE_GAP_MS still yields a rate; one ms more
/// re-baselines (gauge back to None), and the NEXT full minute measures from
/// the new baseline.
#[test]
fn a_gap_beyond_the_max_rebaselines_instead_of_computing_a_rate() {
    assert_eq!(MAX_SAMPLE_GAP_MS, 300_000);
    let mut w = FaultWindow::default();
    w.observe(0, 0, 0);
    let at_max = w.observe(50_000, 0, 300_000).unwrap();
    assert_eq!(at_max.page_faults_per_min, 10_000, "50 000 over 5 min");

    let mut w = FaultWindow::default();
    w.observe(0, 0, 0);
    assert_eq!(w.observe(50_000, 0, 300_001), None, "one ms past the max");
    assert_eq!(w.latest(), None);
    // The long-gap reading became the baseline: +60 s → a fresh rate from it.
    let g = w.observe(56_000, 64 * MIB, 360_001).unwrap();
    assert_eq!(g.page_faults_per_min, 6_000);
    assert_eq!(g.working_set_mb, 64);
}

// ---- to_slots / from_slots (the lock-free publish encoding) ----------------

#[test]
fn slots_roundtrip_a_gauge() {
    let g = ProcMemGauge {
        page_faults_per_min: 48_213,
        working_set_mb: 2_300,
    };
    assert_eq!(to_slots(Some(g)), (48_213, 2_300));
    assert_eq!(from_slots(48_213, 2_300), Some(g));
}

#[test]
fn slots_encode_none_as_the_sentinel_and_back() {
    assert_eq!(NO_READING, u64::MAX);
    assert_eq!(to_slots(None), (u64::MAX, u64::MAX));
    assert_eq!(from_slots(u64::MAX, u64::MAX), None);
    // Either slot at the sentinel is "no reading".
    assert_eq!(from_slots(u64::MAX, 5), None);
    assert_eq!(from_slots(5, u64::MAX), None);
}

#[test]
fn a_real_value_is_capped_below_the_sentinel() {
    let huge = ProcMemGauge {
        page_faults_per_min: u64::MAX,
        working_set_mb: u64::MAX,
    };
    assert_eq!(to_slots(Some(huge)), (u64::MAX - 1, u64::MAX - 1));
    assert_eq!(
        from_slots(u64::MAX - 1, u64::MAX - 1),
        Some(ProcMemGauge {
            page_faults_per_min: u64::MAX - 1,
            working_set_mb: u64::MAX - 1,
        })
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
