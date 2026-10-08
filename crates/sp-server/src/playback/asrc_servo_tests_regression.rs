//! #233: the drift servo's rate regression — its lock, eviction, re-base,
//! realign and restart edges (moved out of `asrc_servo_tests.rs`, the
//! 1000-line cap).

use super::*;

/// `n` points `step_s` apart from `x0` on y = ppm·1e-6·x + 0.5.
fn line_from(r: &mut RateRegression, x0: f64, n: usize, step_s: f64, ppm: f64) {
    for i in 0..n {
        let x = x0 + i as f64 * step_s;
        assert_eq!(
            r.offer(x, ppm * 1e-6 * x + 0.5),
            Offered::Inserted,
            "point {i}"
        );
    }
}

fn line(r: &mut RateRegression, n: usize, step_s: f64, ppm: f64) {
    line_from(r, 0.0, n, step_s, ppm);
}

#[test]
fn the_rate_locks_at_30_points_spanning_60_s() {
    let mut r = RateRegression::default();
    line(&mut r, 30, 2.0, 20.0); // 30 points over 58 s
    assert!(!r.locked());
    assert_eq!(r.rate_ppm(), 0.0, "no rate before the lock");
    let mut r = RateRegression::default();
    line(&mut r, 29, 3.0, 20.0); // 29 points over 84 s
    assert!(!r.locked());
    let mut r = RateRegression::default();
    line(&mut r, 31, 2.0, 20.0); // 31 points over exactly 60 s
    assert!(r.locked());
    assert!((r.rate_ppm() - 20.0).abs() < 1e-6, "{}", r.rate_ppm());
}

/// The span is the newest point less the oldest, wherever the line starts.
#[test]
fn the_lock_span_is_measured_from_the_oldest_point() {
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 30, 2.0, 20.0); // 50 → 108 s
    assert!(!r.locked(), "58 s");
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 31, 2.0, 20.0); // 50 → 110 s
    assert!(r.locked(), "60 s");
    assert!((r.rate_ppm() - 20.0).abs() < 1e-6, "{}", r.rate_ppm());
}

#[test]
fn exactly_30_points_over_exactly_60_s_lock() {
    let mut r = RateRegression::default();
    for i in 0..29 {
        assert_eq!(r.offer(f64::from(i * 2), 0.0), Offered::Inserted);
    }
    assert_eq!(r.offer(60.0, 0.0), Offered::Inserted);
    assert_eq!(r.point_count(), 30);
    assert!(r.locked());
}

#[test]
fn old_points_leave_by_span_and_by_cap() {
    let mut r = RateRegression::default();
    line(&mut r, 700, 1.0, -30.0);
    assert_eq!(r.point_count(), 601, "600 s of 1 s points");
    let mut r = RateRegression::default();
    line(&mut r, 700, 0.5, -30.0);
    assert_eq!(r.point_count(), REGRESSION_CAP);
    assert!((r.rate_ppm() + 30.0).abs() < 1e-6);
}

#[test]
fn a_step_after_the_lock_rebases_and_keeps_the_rate() {
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    let rate = r.rate_ppm();
    assert_eq!(
        r.offer(100.0, 20e-6 * 100.0 + 0.5 + 0.044),
        Offered::Rebased
    );
    assert_eq!(
        r.offer(101.0, 20e-6 * 101.0 + 0.5 + 0.044),
        Offered::Realigned,
        "the next point realigns onto the moved line"
    );
    assert_eq!(
        r.offer(102.0, 20e-6 * 102.0 + 0.5 + 0.044),
        Offered::Inserted,
        "the new line is the old one"
    );
    assert!(
        (r.rate_ppm() - rate).abs() < 1e-6,
        "the rate is not disturbed"
    );
}

/// A 30 ms step inside a window: its mean is 22 ms off (re-based), the next
/// window's is 30 ms off — 8 ms from the moved line, under the step residual,
/// so without the realign it would stay in the regression as a level shift.
#[test]
fn a_step_straddling_a_window_realigns_the_next_point() {
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    let rate = r.rate_ppm();
    let on = |x: f64| 20e-6 * x + 0.5;
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    assert_eq!(r.offer(101.0, on(101.0) + 0.030), Offered::Realigned);
    assert_eq!(r.offer(102.0, on(102.0) + 0.030), Offered::Inserted);
    assert_eq!(r.offer(103.0, on(103.0) + 0.030), Offered::Inserted);
    assert!(
        (r.rate_ppm() - rate).abs() < 1e-6,
        "{} vs {rate}",
        r.rate_ppm()
    );
    // A 44 ms step: 22 ms in the straddled window's mean, 22 more in the
    // next — the rest of the same step realigns (one step, one rebase).
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    assert_eq!(r.offer(101.0, on(101.0) + 0.044), Offered::Realigned);
    assert_eq!(r.offer(102.0, on(102.0) + 0.044), Offered::Inserted);
    // A flush forgets a pending realign.
    let mut r = RateRegression::default();
    line(&mut r, 100, 1.0, 20.0);
    assert_eq!(r.offer(100.0, on(100.0) + 0.022), Offered::Rebased);
    r.flush();
    line(&mut r, 31, 1.0, 20.0);
    assert_eq!(r.offer(31.0, on(31.0) + 0.005), Offered::Inserted);
}

/// A steep line (1000 ppm) from x = 50 s: every point on it is a point, one
/// 11 ms above it is a step, the next one realigns, then the moved line goes on.
#[test]
fn a_steep_line_is_fitted_through_its_points() {
    let mut r = RateRegression::default();
    line_from(&mut r, 50.0, 70, 1.0, 1000.0);
    assert!(r.locked());
    assert!((r.rate_ppm() - 1000.0).abs() < 1e-6, "{}", r.rate_ppm());
    assert_eq!(r.offer(120.0, 1e-3 * 120.0 + 0.5), Offered::Inserted);
    assert_eq!(r.offer(121.0, 1e-3 * 121.0 + 0.5 + 0.011), Offered::Rebased);
    assert_eq!(
        r.offer(122.0, 1e-3 * 122.0 + 0.5 + 0.011),
        Offered::Realigned
    );
    assert_eq!(
        r.offer(123.0, 1e-3 * 123.0 + 0.5 + 0.011),
        Offered::Inserted
    );
}

/// `n` points one second apart on y = 0 (exact in f64, so a residual of
/// exactly `STEP_RESIDUAL_S` can be pinned; `0.5 + 0.010 − 0.5` is not 0.010).
fn flat(r: &mut RateRegression, n: u32) {
    for i in 0..n {
        assert_eq!(r.offer(f64::from(i), 0.0), Offered::Inserted);
    }
}

#[test]
fn a_residual_of_exactly_10_ms_is_a_point_and_more_is_a_step() {
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.010), Offered::Inserted);
    let mut r = RateRegression::default();
    flat(&mut r, 40);
    assert_eq!(r.offer(40.0, 0.0101), Offered::Rebased);
}

/// The fit takes over from the 31st point (30 behind it), not later.
#[test]
fn the_fit_judges_a_step_once_it_has_30_points() {
    let mut r = RateRegression::default();
    flat(&mut r, 30);
    assert_eq!(r.offer(30.0, 0.0101), Offered::Rebased, "the 31st");
    let mut r = RateRegression::default();
    flat(&mut r, 29);
    assert_eq!(r.offer(29.0, 0.0101), Offered::Restarted, "the 30th");
}

#[test]
fn a_jump_before_the_lock_restarts_the_regression() {
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(r.offer(5.0, 0.020), Offered::Restarted);
    assert_eq!(r.point_count(), 1);
    assert_eq!(
        r.offer(6.0, 0.020),
        Offered::Inserted,
        "the line goes on from the jumped point"
    );
    assert_eq!(r.point_count(), 2);
    let mut r = RateRegression::default();
    flat(&mut r, 5);
    assert_eq!(
        r.offer(5.0, 0.010),
        Offered::Inserted,
        "10 ms is still a point"
    );
    r.flush();
    assert_eq!(r.point_count(), 0);
}

#[test]
fn points_at_one_instant_give_no_slope() {
    let mut r = RateRegression::default();
    for _ in 0..40 {
        r.offer(5.0, 0.5);
    }
    assert_eq!(r.rate_ppm(), 0.0);
    assert_eq!(r.fit(), None);
}
