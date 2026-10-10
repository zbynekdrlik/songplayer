//! #233: what the wrapper adds — sizes, delay, bounds and buffers. The
//! filter's quality and the frame counts under a servo are sp-server's
//! `asrc_tests.rs`.

use super::*;
use rubato::{SincInterpolationType, WindowFunction};

/// sp-server's filter: 256 taps, oversampled 256×, BlackmanHarris², cubic.
fn params() -> SincInterpolationParameters {
    SincInterpolationParameters::new(256, WindowFunction::BlackmanHarris2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Cubic)
}

/// 48 → 96 kHz, stereo, 1600 frames a call, ±1000 ppm.
fn stage() -> SincStage {
    SincStage::new(2.0, 1.001, &params(), 1600, 2).unwrap()
}

#[test]
fn the_first_call_at_96k_writes_3196_frames_after_a_256_frame_delay() {
    let mut s = stage();
    // rubato's sizing (the scratch model in sp-server's asrc_tests.rs): the
    // sinc's start index is −255, so the first call is 4 frames short.
    let mut out = vec![0.0; s.max_out_frames() * 2];
    assert_eq!(s.process(&[0.25; 3200], &mut out), Ok(3196));
    assert_eq!(s.delay_frames(), 256);
}

#[test]
fn a_call_has_room_for_the_ratio_at_its_bound() {
    // rubato's own sizing: 1600 frames × 2 × 1.001 + 10, truncated.
    assert_eq!(stage().max_out_frames(), 3213);
}

#[test]
fn a_tone_comes_out_at_the_output_rate() {
    let mut s = stage();
    let mut out = vec![0.0; s.max_out_frames() * 2];
    // One continuous tone across the calls (a block of 33⅓ cycles).
    let block = |b: usize| -> Vec<f32> {
        (0..1600)
            .flat_map(|i| {
                let t = (b * 1600 + i) as f32 / 48_000.0;
                let x = (t * 2.0 * std::f32::consts::PI * 1000.0).sin() * 0.5;
                [x, -x]
            })
            .collect()
    };
    s.process(&block(0), &mut out).unwrap();
    let n = s.process(&block(1), &mut out).unwrap();
    // Past the delay, both channels carry the tone, opposite in sign.
    let peak = out[..n * 2]
        .iter()
        .step_by(2)
        .fold(0f32, |m, v| m.max(v.abs()));
    assert!((0.49..0.51).contains(&peak), "{peak}");
    for frame in out[..n * 2].chunks(2) {
        assert!((frame[0] + frame[1]).abs() < 1e-6, "{frame:?}");
    }
}

#[test]
fn a_relative_ratio_past_the_bound_is_refused() {
    let mut s = stage();
    assert_eq!(s.set_relative(1.0005, true), Ok(()));
    assert!(s.set_relative(1.01, true).is_err());
    assert!(s.set_relative(0.99, false).is_err());
}

#[test]
fn a_relative_ratio_moves_the_output() {
    let mut fast = stage();
    let mut nominal = stage();
    fast.set_relative(1.0005, false).unwrap();
    let mut out = vec![0.0; fast.max_out_frames() * 2];
    let (mut a, mut b) = (0, 0);
    for _ in 0..20 {
        a += fast.process(&[0.0; 3200], &mut out).unwrap();
        b += nominal.process(&[0.0; 3200], &mut out).unwrap();
    }
    // 20 calls of 3200 frames at +500 ppm: 32 more frames, give or take
    // the one a fractional index carries.
    assert!((31..=33).contains(&(a - b)), "{a} {b}");
}

#[test]
fn a_short_buffer_is_refused() {
    let mut s = stage();
    let mut short_out = vec![0.0; 10];
    assert!(s.process(&[0.0; 3200], &mut short_out).is_err());
    let mut out = vec![0.0; s.max_out_frames() * 2];
    assert!(s.process(&[0.0; 100], &mut out).is_err());
}

#[test]
fn a_ratio_rubato_refuses_is_an_error() {
    assert!(SincStage::new(0.0, 1.001, &params(), 1600, 2).is_err());
}
