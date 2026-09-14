//! Audio clock-discipline math tests (#148 rework) — pure, WASM-safe, no clock
//! calls. Covers `samples_per_boundary`, the level→ppm conversions
//! (`residual_ppm`, `rate_residual_ppm`), the same-phase [`LevelAverager`], and
//! the slow-trim [`AudioPll`] (60 s cadence, seed-on-first-call, dead-band 50,
//! gain 0.5, slew ±5/update, clamp ±500, and the ±20 ppm position trim).
//! `super::*` resolves to the `genlock::audio` module under test.

use super::*;

const SEC_100NS: i64 = 10_000_000; // one second in 100-ns units
const MIN_100NS: i64 = 60 * SEC_100NS; // one minute (the PLL update cadence)

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
// residual_ppm — buffer-level drift → file-clock error in ppm (pure math)
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
// rate_residual_ppm — two same-phase 60 s means → TRUE ppm rate residual
// ---------------------------------------------------------------------------

#[test]
fn rate_residual_ppm_means_drift_of_9_6_samples_over_60s_is_3_33_ppm() {
    // means 3200 → 3209.6 over 60 s at 48 kHz = +3.33 ppm (#148 item 1 vector).
    let r = rate_residual_ppm(3209.6, 3200.0, 48_000, 60.0);
    assert!((r - 3.3333).abs() < 0.01, "expected ~+3.33 ppm, got {r}");
}

#[test]
fn rate_residual_ppm_guards_zero_window_and_rate() {
    assert_eq!(rate_residual_ppm(3200.0, 3100.0, 48_000, 0.0), 0.0);
    assert_eq!(rate_residual_ppm(3200.0, 3100.0, 0, 60.0), 0.0);
}

#[test]
fn rate_residual_ppm_averages_out_the_push_take_sawtooth_under_1_ppm() {
    // A steady 0-ppm clock still makes the instantaneous level saw up and down as
    // 2002-sample pushes meet 1600-sample takes. A period-1.25-boundary sawtooth
    // of amplitude 2002, sampled at integer boundaries, has period 5 boundaries
    // (frac(0.8·k)); over a 1800-boundary window (= 360 periods) both same-phase
    // means are identical, so the true rate residual is ~0 — well under 1 ppm.
    let mut avg = LevelAverager::new(1800);
    for k in 0..3600i64 {
        let phase = (0.8 * k as f64).fract();
        let level = 3200 + (2002.0 * phase).round() as i64;
        avg.record(level);
    }
    assert!(avg.windows_full());
    let r = rate_residual_ppm(avg.mean_now(), avg.mean_prev(), 48_000, 60.0);
    assert!(r.abs() < 1.0, "sawtooth must average to < 1 ppm, got {r}");
}

// ---------------------------------------------------------------------------
// LevelAverager — same-phase 60 s means of the post-take level
// ---------------------------------------------------------------------------

#[test]
fn level_averager_means_track_the_two_windows() {
    let mut avg = LevelAverager::new(3);
    assert!(!avg.windows_full());
    // prev window = [100,100,100] (mean 100), now window = [200,200,200] (mean 200)
    for _ in 0..3 {
        avg.record(100);
    }
    for _ in 0..3 {
        avg.record(200);
    }
    assert!(avg.windows_full());
    assert!((avg.mean_prev() - 100.0).abs() < 1e-9);
    assert!((avg.mean_now() - 200.0).abs() < 1e-9);
    // A steady +100/window drift → rate residual positive.
    assert!(rate_residual_ppm(avg.mean_now(), avg.mean_prev(), 48_000, 60.0) > 0.0);
}

#[test]
fn level_averager_evicts_oldest_past_two_windows() {
    let mut avg = LevelAverager::new(2);
    for v in [1, 2, 3, 4, 5, 6] {
        avg.record(v);
    }
    // Only the last 4 survive: prev = [3,4] (3.5), now = [5,6] (5.5).
    assert!((avg.mean_prev() - 3.5).abs() < 1e-9);
    assert!((avg.mean_now() - 5.5).abs() < 1e-9);
    avg.clear();
    assert!(!avg.windows_full());
    assert_eq!(avg.mean_now(), 0.0);
}

// ---------------------------------------------------------------------------
// AudioPll — rate term: seed, 60 s cadence, gain 0.5, slew 5, clamp 500, band 50
// ---------------------------------------------------------------------------

#[test]
fn audio_pll_seeds_on_first_call_and_does_not_act() {
    let mut pll = AudioPll::new();
    // A large residual and a large first dt would be a giant step if not seeded.
    assert_eq!(pll.update(200.0, 999 * MIN_100NS), 0.0);
    assert_eq!(pll.applied_ppm(), 0.0, "first call only seeds");
}

#[test]
fn audio_pll_holds_between_60s_ticks() {
    let mut pll = AudioPll::new();
    pll.update(200.0, 0); // seed
    // A call < 60 s after the seed does nothing.
    assert_eq!(pll.update(200.0, MIN_100NS - 1), 0.0);
    // At exactly 60 s it acts.
    let a = pll.update(200.0, MIN_100NS);
    assert!(a < 0.0, "acts on the 60 s tick, got {a}");
}

#[test]
fn audio_pll_rate_steps_at_most_5_ppm_per_update_toward_minus_residual() {
    let mut pll = AudioPll::new();
    pll.update(200.0, 0); // seed
    let mut prev = 0.0;
    for k in 1..=10i64 {
        let a = pll.update(200.0, k * MIN_100NS);
        // +200 residual → applied only DECREASES (toward −residual).
        assert!(a <= prev + 1e-9, "monotone non-increasing: {prev} -> {a}");
        // Each update moves at most 5 ppm (slew), and exactly 5 here
        // (0.5·200 = 100, clamped to 5).
        assert!(
            (a - prev).abs() <= 5.0 + 1e-9,
            "step ≤ 5 ppm: {prev} -> {a}"
        );
        prev = a;
    }
    // 10 updates × −5 ppm = −50 ppm.
    assert!(
        (prev - (-50.0)).abs() < 1e-6,
        "10 updates → −50 ppm, got {prev}"
    );
}

#[test]
fn audio_pll_dead_band_ignores_small_residual() {
    for &res in &[30.0f64, -30.0, 50.0, -50.0] {
        let mut pll = AudioPll::new();
        pll.update(res, 0); // seed
        for k in 1..=40i64 {
            let a = pll.update(res, k * MIN_100NS);
            assert_eq!(a, 0.0, "|{res}| ≤ 50 ppm in-band must not move applied");
        }
    }
}

#[test]
fn audio_pll_clamps_total_at_plus_minus_500() {
    let mut pll = AudioPll::new();
    pll.update(900.0, 0); // seed
    for k in 1..=200i64 {
        pll.update(900.0, k * MIN_100NS);
    }
    assert!(
        (pll.applied_ppm() - (-500.0)).abs() < 1e-6,
        "sustained +900 residual clamps applied at −500, got {}",
        pll.applied_ppm()
    );
}

#[test]
fn audio_pll_reset_returns_to_zero() {
    let mut pll = AudioPll::new();
    pll.update(200.0, 0);
    for k in 1..=20i64 {
        pll.update(200.0, k * MIN_100NS);
    }
    assert!(pll.applied_ppm() < 0.0, "should have corrected");
    pll.reset();
    assert_eq!(pll.applied_ppm(), 0.0, "reset clears the correction");
}

// ---------------------------------------------------------------------------
// AudioPll — position trim: engages after 60 s beyond ±2 boundaries, decays
// ---------------------------------------------------------------------------

#[test]
fn audio_pll_position_trim_engages_after_60s_and_ramps_to_20_then_decays() {
    let target = 3200i64; // 2 · samples_per_boundary
    let high = target + 2 * target; // |level−target| = 2·target > 2 boundaries
    let mut pll = AudioPll::new();

    // Drive one update per minute with the level far above target.
    // k=0 seeds; far_since=0. k=1 (60 s): not yet > 60 s. k=2 (120 s): engaged.
    let mut applied = 0.0;
    for k in 0..=6i64 {
        applied = pll.update_level(high, target, k * MIN_100NS);
    }
    // Ramped to +20 ppm (drain a too-high buffer): 5,10,15,20 over k=2..=5.
    assert!(
        (applied - 20.0).abs() < 1e-6,
        "far-high level must bias to +20 ppm, got {applied}"
    );

    // Level back at target (within 1 boundary): bias decays by 5 ppm/update.
    let a1 = pll.update_level(target, target, 7 * MIN_100NS);
    assert!((a1 - 15.0).abs() < 1e-6, "decay step 1 → 15, got {a1}");
    let a2 = pll.update_level(target, target, 8 * MIN_100NS);
    assert!((a2 - 10.0).abs() < 1e-6, "decay step 2 → 10, got {a2}");
    for k in 9..=12i64 {
        pll.update_level(target, target, k * MIN_100NS);
    }
    assert!(
        pll.applied_ppm().abs() < 1e-6,
        "bias fully decays to 0, got {}",
        pll.applied_ppm()
    );
}

#[test]
fn audio_pll_position_trim_within_2_boundaries_never_engages() {
    let target = 3200i64;
    let within = target + target; // |level−target| = target, NOT > target
    let mut pll = AudioPll::new();
    for k in 0..=10i64 {
        pll.update_level(within, target, k * MIN_100NS);
    }
    assert_eq!(
        pll.applied_ppm(),
        0.0,
        "a level within ±2 boundaries never engages the position trim"
    );
}
