//! Mutation-kill tests for `sp_core::genlock::audio` (PR #153 / 0.47.0).
//!
//! Each test pins an EXACT `f64` / `bool` at the precise input where a
//! surviving mutant diverges: the `windows_full` `2*w` boundary at `w > 2`,
//! the 60 s update-cadence gate with a NON-zero seed time (so `-` vs `+` on
//! `now - last` diverges), the `far_since` sustain equality point, the
//! near/far hysteresis edges, and the position-bias sign. Wired from
//! `genlock_audio.rs`, so `super::*` is the `genlock::audio` module.

use super::*;

const SEC: i64 = 10_000_000; // one second in 100-ns units
const MIN: i64 = 60 * SEC; // AUDIO_PLL_UPDATE_100NS (the 60 s cadence)
const EPS: f64 = 1e-9;

// ---------------------------------------------------------------------------
// LevelAverager::windows_full 136:30 `*` -> `+`
//
// `len >= 2 * window` vs `len >= 2 + window`. With window == 3 they differ:
// 2*3 == 6, 2+3 == 5. At len == 5 the original is NOT full; the mutant is.
// ---------------------------------------------------------------------------

#[test]
fn windows_full_uses_two_times_window_not_two_plus_window() {
    let mut la = LevelAverager::new(3);
    for _ in 0..5 {
        la.record(100);
    }
    assert!(
        !la.windows_full(),
        "5 < 2*3 == 6: not full (mutant `2 + window == 5` would report full)"
    );
    la.record(100); // 6th
    assert!(la.windows_full(), "6 == 2*3: full");
}

// ---------------------------------------------------------------------------
// AudioPll::update 248:22 `-` -> `+` on the `now - last < UPDATE` cadence gate.
//
// Seed at a NON-zero time (MIN), then a call at 1.5*MIN: `now - last` == 0.5*MIN
// (< UPDATE -> no action, returns 0). The mutant `now + last` == 2.5*MIN
// (>= UPDATE -> acts -> applied becomes -5).
// ---------------------------------------------------------------------------

#[test]
fn update_cadence_gate_uses_now_minus_last() {
    let mut pll = AudioPll::new();
    assert_eq!(pll.update(200.0, MIN), 0.0); // seed last_rate = MIN
    let r = pll.update(200.0, MIN + MIN / 2); // dt == 0.5*MIN < UPDATE -> no action
    assert!(
        r.abs() < EPS,
        "0.5*MIN elapsed since seed -> no rate action (mutant `now + last` acts -> -5), got {r}"
    );
}

// ---------------------------------------------------------------------------
// AudioPll::update_level position trim. target == 3200 (2 boundaries),
// BIAS == 20, SLEW == 5. All sequences drive the pll then assert the EXACT
// returned applied_ppm.
// ---------------------------------------------------------------------------

const TARGET: i64 = 3200;
const HIGH: i64 = 9600; // dev == 6400 > TARGET -> far

/// 287:29 `<`->`>` and 287:22 `-`->`+` on the bias-tick cadence gate
/// (`now - last < UPDATE`). After engaging a +5 bias, a call with dt < UPDATE
/// must NOT act (returns the held 5.0); either mutant wrongly acts and decays
/// the bias to 0.
#[test]
fn update_level_bias_tick_gate_holds_within_60s() {
    let mut pll = AudioPll::new();
    pll.update_level(HIGH, TARGET, MIN); // seed: far_since = MIN, last_bias = MIN
    let engaged = pll.update_level(HIGH, TARGET, 3 * MIN); // dt 2*MIN, engaged -> +5
    assert!(
        (engaged - 5.0).abs() < EPS,
        "engaged tick -> +5, got {engaged}"
    );
    // dt == 0.5*MIN since last_bias (3*MIN) -> below cadence -> hold 5.0.
    let held = pll.update_level(TARGET, TARGET, 3 * MIN + MIN / 2);
    assert!(
        (held - 5.0).abs() < EPS,
        "within 60 s of the last bias tick the bias is HELD at 5 (mutant acts -> 0), got {held}"
    );
}

/// 294:44 `>`->`>=` and 294:40 `-`->`+` on the `now - far_since > UPDATE`
/// engage check. far_since is set at MIN; the tick lands at 2*MIN, so the
/// sustain is EXACTLY UPDATE — `>` does NOT engage (returns 0.0), while `>=`
/// engages and `now + far_since` (3*MIN > UPDATE) also engages, both -> +5.
#[test]
fn update_level_engage_needs_strictly_more_than_60s_sustain() {
    let mut pll = AudioPll::new();
    pll.update_level(HIGH, TARGET, MIN); // seed: far_since = MIN, last_bias = MIN
    let r = pll.update_level(HIGH, TARGET, 2 * MIN); // sustain == MIN exactly
    assert!(
        r.abs() < EPS,
        "sustain of exactly 60 s must NOT engage (mutant `>=`/`+` engages -> +5), got {r}"
    );
}

/// 296:51 `-`->`+` and `-`->`/` on `signum(post_take_level - target)`.
/// A level BELOW target while still far (level -100, dev 3300 > 3200) makes the
/// deviation negative: bias must ramp to -5. `+` makes the sign positive (+5);
/// `/` truncates -100/3200 to 0 so signum is 0 (0.0).
#[test]
fn update_level_bias_sign_from_deviation_below_target() {
    let mut pll = AudioPll::new();
    pll.update_level(-100, TARGET, MIN); // seed far (dev 3300), far_since = MIN
    let r = pll.update_level(-100, TARGET, 3 * MIN); // engaged tick
    assert!(
        (r + 5.0).abs() < EPS,
        "a level below target biases NEGATIVE (-5); mutant `+`->+5 / `/`->0, got {r}"
    );
}

/// 271:24 `<=`->`>` and 271:41 `/`->`%` on `near = dev <= target / 2.0`.
/// After a +5 engaged bias, a level just above target (dev 100, within one
/// boundary) is `near`, so the bias DECAYS to 0. The mutants make `near` false
/// (`100 > 1600`, or `dev <= target % 2 == 0`), keeping far_since set so the
/// bias instead RAMPS to 10.
#[test]
fn update_level_near_engages_decay_at_small_deviation() {
    let mut pll = AudioPll::new();
    pll.update_level(HIGH, TARGET, MIN); // seed far, far_since = MIN
    pll.update_level(HIGH, TARGET, 3 * MIN); // engaged -> +5
    let r = pll.update_level(3300, TARGET, 5 * MIN); // dev 100 -> near -> decay
    assert!(
        r.abs() < EPS,
        "dev 100 is `near` (100 <= 1600) -> decay to 0; mutants keep ramping -> 10, got {r}"
    );
}

/// 271:41 `/`->`*` on `dev <= target / 2.0`. A deviation of 3000 is beyond one
/// boundary (`3000 > 1600`, NOT near) but below `target * 2 == 6400`. Original:
/// not near -> far_since kept -> bias RAMPS to 10. Mutant `*`: `3000 <= 6400`
/// -> near -> far_since cleared -> bias DECAYS to 0.
#[test]
fn update_level_near_uses_half_target_not_double() {
    let mut pll = AudioPll::new();
    pll.update_level(HIGH, TARGET, MIN); // seed far, far_since = MIN
    pll.update_level(HIGH, TARGET, 3 * MIN); // engaged -> +5
    let r = pll.update_level(6200, TARGET, 5 * MIN); // dev 3000: not near
    assert!(
        (r - 10.0).abs() < EPS,
        "dev 3000 is NOT near (> target/2 == 1600) -> ramp to 10; mutant `*` decays -> 0, got {r}"
    );
}
