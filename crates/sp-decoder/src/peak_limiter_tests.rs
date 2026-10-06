//! Unit tests for [`PeakLimiter`] (#184). Pure f32 arithmetic, so the exact
//! values below come from a scratch f32 model of `process` and hold on every
//! platform (the release factor is an IEEE division, no `exp`).

use super::*;

/// The release fixture in mono: three frames at 0.5, ONE over at 1.96 (it
/// needs the gain 0.98/1.96 = 0.5), then 1000 frames at 0.5.
fn release_fixture() -> Vec<f32> {
    let mut s = vec![0.5_f32; 3];
    s.push(1.96);
    s.extend(std::iter::repeat_n(0.5_f32, 1000));
    s
}

#[test]
fn the_release_factor_is_one_minus_one_over_the_release_frames() {
    // 50 ms at 1000 Hz = 50 frames → 1 − 1/50 = 0.98 exactly.
    assert_eq!(PeakLimiter::new(1_000).release, 0.98_f32);
    // 50 ms at 48 kHz = 2400 frames → 1 − 1/2400.
    let r48 = PeakLimiter::new(48_000).release;
    assert!(
        (f64::from(r48) - (1.0 - 1.0 / 2400.0)).abs() < 1e-7,
        "{r48}"
    );
    // A rate under 20 Hz releases at once instead of growing the reduction.
    assert_eq!(PeakLimiter::new(10).release, 0.0);
}

#[test]
fn a_new_limiter_is_at_rest() {
    let l = PeakLimiter::new(48_000);
    assert_eq!(l.reduction, 0.0);
    assert_eq!(l.limited_frames(), 0);
}

#[test]
fn frames_at_or_under_the_ceiling_pass_bit_for_bit() {
    let mut l = PeakLimiter::new(48_000);
    let input = [0.0_f32, -0.0, 0.5, -0.25, 0.98, -0.98, 0.979_999, 1e-30];
    let mut block = input;
    l.process(&mut block, 2);
    for (o, i) in block.iter().zip(&input) {
        assert_eq!(o.to_bits(), i.to_bits(), "{o} vs {i}");
    }
    assert_eq!(l.reduction, 0.0);
    assert_eq!(l.limited_frames(), 0);
}

#[test]
fn an_over_frame_comes_out_at_the_ceiling_with_one_gain_for_both_channels() {
    let mut l = PeakLimiter::new(48_000);
    // Peak on the RIGHT channel: the frame's gain comes from its highest
    // |sample|, whichever channel holds it.
    let mut block = [0.3_f32, -2.45];
    l.process(&mut block, 2);
    let gain = 0.98 / 2.45;
    assert!((block[0] - 0.3 * gain).abs() < 1e-6, "{block:?}");
    assert!((block[1] + 0.98).abs() < 1e-6, "{block:?}");
    assert!((l.reduction - (1.0 - gain)).abs() < 1e-6, "{}", l.reduction);
    assert_eq!(l.limited_frames(), 1);
}

/// Release 0.71.0 review: `ceiling / peak` and `1 − reduction` each round in
/// f32, so `peak × gain` can land a code or two ABOVE 0.98 (a peak of 1.071
/// came out at 0.98000008; about one over in four of this sweep did). The
/// program's own limiter then scaled a limited stem mix again, so a playlist
/// on program did not pass it bit for bit. No sample may leave above the
/// ceiling, on either side.
#[test]
fn no_limited_sample_leaves_above_the_ceiling() {
    let mut l = PeakLimiter::new(48_000);
    let mut over_ceiling = Vec::new();
    for k in 981..4_000_u16 {
        let peak = f32::from(k) / 1000.0;
        for frame in [[peak, -0.3_f32], [-peak, 0.3]] {
            l.reset();
            let mut out = frame;
            l.process(&mut out, 2);
            if out.iter().any(|s| s.abs() > LIMIT_CEILING) {
                over_ceiling.push((frame, out));
            }
        }
    }
    assert!(
        over_ceiling.is_empty(),
        "{} frames left above {LIMIT_CEILING}, e.g. {:?}",
        over_ceiling.len(),
        &over_ceiling[..over_ceiling.len().min(3)]
    );
}

#[test]
fn the_reduction_decays_by_the_release_factor_each_quiet_frame() {
    let mut l = PeakLimiter::new(1_000);
    let input = release_fixture();
    let mut block = input.clone();
    l.process(&mut block, 1);
    // Frame 3 + k carries the gain 1 − 0.5 × 0.98^k (k = 0 is the over).
    for k in 0..=5_usize {
        let gain = f64::from(block[3 + k]) / f64::from(input[3 + k]);
        let want = 1.0 - 0.5 * 0.98_f64.powi(k as i32);
        assert!(
            (gain - want).abs() < 1e-6,
            "frame {}: want {want}, got {gain}",
            3 + k
        );
    }
}

#[test]
fn a_new_peak_above_the_released_gain_attacks_at_once() {
    let mut l = PeakLimiter::new(1_000);
    // 1.96 needs a reduction of 0.5; one frame later it has decayed to 0.49,
    // and 3.92 needs 0.75: the bigger need wins in that same frame.
    let mut block = [1.96_f32, 3.92];
    l.process(&mut block, 1);
    assert!((block[0] - 0.98).abs() < 1e-6, "{block:?}");
    assert!((block[1] - 0.98).abs() < 1e-6, "{block:?}");
    assert!((l.reduction - 0.75).abs() < 1e-6, "{}", l.reduction);
    // A smaller over inside the released gain needs no new attack: 1.0 at
    // the gain 1 − 0.75 × 0.98 = 0.265 stays under the ceiling.
    let mut quieter = [1.0_f32];
    l.process(&mut quieter, 1);
    assert!((quieter[0] - 0.265).abs() < 1e-6, "{quieter:?}");
    assert!((l.reduction - 0.735).abs() < 1e-6, "{}", l.reduction);
}

#[test]
fn the_limiter_returns_to_rest_and_counts_only_the_scaled_frames() {
    let mut l = PeakLimiter::new(1_000);
    let input = release_fixture();
    let mut block = input.clone();
    l.process(&mut block, 1);
    // The over (frame 3) plus 823 release frames are scaled; from frame 827 the
    // gain rounds to exactly 1.0 and the state is dropped.
    assert_eq!(l.limited_frames(), 824);
    assert_eq!(l.reduction, 0.0, "at rest once the gain is unity");
    assert!(block[826] < input[826]);
    assert_eq!(block[827].to_bits(), input[827].to_bits());
}

#[test]
fn reset_drops_the_release_tail() {
    let mut l = PeakLimiter::new(1_000);
    let mut over = [1.96_f32];
    l.process(&mut over, 1);
    assert!(l.reduction > 0.0);
    l.reset();
    assert_eq!(l.reduction, 0.0);
    let mut quiet = [0.5_f32];
    l.process(&mut quiet, 1);
    assert_eq!(quiet[0].to_bits(), 0.5_f32.to_bits());
    assert_eq!(l.limited_frames(), 1, "reset keeps the count");
}
