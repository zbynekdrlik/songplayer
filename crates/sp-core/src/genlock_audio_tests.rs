//! Audio clock-discipline math tests (#148) — pure, WASM-safe, no clock calls.
//!
//! Mirrors camera-box#1294 §6: `samples_per_boundary = rate / fps`, the
//! buffer-level → file-clock residual conversion, and the slow-resample PLL
//! (dead-band / hold / one-pole toward `−residual` / slew clamp / ±max clamp).
//! `super::*` resolves to the `genlock::audio` module under test.

use super::*;

const SEC_100NS: i64 = 10_000_000; // one second in 100-ns units

// ---------------------------------------------------------------------------
// samples_per_boundary
// ---------------------------------------------------------------------------

#[test]
fn samples_per_boundary_exact_grid_rates() {
    assert_eq!(samples_per_boundary(48_000, 30), 1600);
    assert_eq!(samples_per_boundary(48_000, 60), 800);
}

#[test]
fn samples_per_boundary_zero_fps_is_zero_no_panic() {
    assert_eq!(samples_per_boundary(48_000, 0), 0);
    assert_eq!(samples_per_boundary(0, 30), 0);
}

// ---------------------------------------------------------------------------
// residual_ppm — buffer-level drift → file-clock error in ppm
// ---------------------------------------------------------------------------

#[test]
fn residual_ppm_growing_level_is_positive_two_hundred() {
    // level grows by 96 samples over 10 s at 48 kHz → +200 ppm.
    let r = residual_ppm(3200 + 96, 3200, 10 * SEC_100NS, 48_000);
    assert!((r - 200.0).abs() < 5.0, "expected ~+200 ppm, got {r}");
}

#[test]
fn residual_ppm_shrinking_level_is_negative_two_hundred() {
    let r = residual_ppm(3200 - 96, 3200, 10 * SEC_100NS, 48_000);
    assert!((r + 200.0).abs() < 5.0, "expected ~-200 ppm, got {r}");
}

#[test]
fn residual_ppm_guards_zero_elapsed_and_rate() {
    assert_eq!(residual_ppm(100, 0, 0, 48_000), 0.0);
    assert_eq!(residual_ppm(100, 0, SEC_100NS, 0), 0.0);
}

// ---------------------------------------------------------------------------
// AudioPll — dead-band, hold, one-pole toward −residual, slew + clamp
// ---------------------------------------------------------------------------

/// Drive the PLL with a constant residual on a 1-s update cadence and collect
/// `applied_ppm` after each update.
fn run_pll(residual: f64, updates: usize) -> Vec<f64> {
    let mut pll = AudioPll::new();
    let mut hist = Vec::with_capacity(updates + 1);
    for k in 0..=updates {
        hist.push(pll.update(residual, k as i64 * SEC_100NS));
    }
    hist
}

#[test]
fn audio_pll_holds_zero_for_ten_seconds_then_converges_to_minus_residual() {
    let hist = run_pll(200.0, 60);

    // Stays 0 for the 10 s hold (updates k=0..=9), first non-zero at k=10.
    for (k, a) in hist.iter().take(10).enumerate() {
        assert_eq!(*a, 0.0, "must hold 0 during the 10 s window: k={k} a={a}");
    }
    assert!(
        hist[10] != 0.0,
        "must start correcting once the hold elapses: {}",
        hist[10]
    );

    // Converges to −200 within 60 s.
    assert!(
        (hist[60] - (-200.0)).abs() < 10.0,
        "must converge to ~-200 within 60 s, got {}",
        hist[60]
    );
}

#[test]
fn audio_pll_is_monotone_never_overshoots_and_steps_at_most_ten_ppm() {
    let hist = run_pll(200.0, 60);
    for w in hist.windows(2) {
        // Monotone non-increasing (residual +200 → applied only decreases).
        assert!(w[1] <= w[0] + 1e-9, "monotone: {} -> {}", w[0], w[1]);
        // Each 1-s update moves applied_ppm by at most 10 ppm.
        assert!(
            (w[1] - w[0]).abs() <= 10.0 + 1e-9,
            "step must be <= 10 ppm/update: {} -> {}",
            w[0],
            w[1]
        );
    }
    // Never overshoots past the target.
    for &a in &hist {
        assert!(a >= -200.0 - 1e-9, "must not overshoot below -200: {a}");
    }
}

#[test]
fn audio_pll_dead_band_ignores_small_residual() {
    // ±30 ppm is inside the ±50 ppm band → applied never moves.
    for &res in &[30.0f64, -30.0] {
        let hist = run_pll(res, 40);
        for a in hist {
            assert_eq!(a, 0.0, "±30 ppm in-band must not move applied_ppm");
        }
    }
}

#[test]
fn audio_pll_clamps_at_plus_minus_five_hundred() {
    // A huge sustained residual clamps applied_ppm at ∓500 (opposite sign,
    // since applied tracks −residual).
    let hi = run_pll(900.0, 200);
    assert!(
        (hi.last().unwrap() - (-500.0)).abs() < 1.0,
        "+900 residual must clamp applied at -500, got {}",
        hi.last().unwrap()
    );
    let lo = run_pll(-900.0, 200);
    assert!(
        (lo.last().unwrap() - 500.0).abs() < 1.0,
        "-900 residual must clamp applied at +500, got {}",
        lo.last().unwrap()
    );
}

#[test]
fn audio_pll_reset_returns_to_zero() {
    let mut pll = AudioPll::new();
    for k in 0..=60 {
        pll.update(200.0, k * SEC_100NS);
    }
    assert!(pll.applied_ppm() < -50.0, "should have corrected");
    pll.reset();
    assert_eq!(pll.applied_ppm(), 0.0, "reset clears the correction");
}
