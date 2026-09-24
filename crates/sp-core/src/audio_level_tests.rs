//! Unit tests for [`super`] — the shared RMS level helper of the #184 G4 stage
//! probes. Every expected value is the closed-form level of a known signal.

use super::*;

fn assert_db(actual: f32, expected: f32, tol: f32) {
    assert!(
        (actual - expected).abs() <= tol,
        "expected {expected} dBFS (±{tol}), got {actual}"
    );
}

/// `n` samples of a full-scale-`amp` square wave (+amp, −amp, +amp, …).
fn square(n: usize, amp: f32) -> Vec<f32> {
    (0..n)
        .map(|i| if i % 2 == 0 { amp } else { -amp })
        .collect()
}

/// 100 whole periods of a 1 kHz sine at 48 kHz, amplitude `amp`.
fn sine(amp: f32) -> Vec<f32> {
    (0..4800)
        .map(|k| amp * (2.0 * std::f32::consts::PI * k as f32 / 48.0).sin())
        .collect()
}

#[test]
fn empty_slice_is_the_silence_floor() {
    assert_eq!(rms_dbfs(&[]), SILENCE_FLOOR_DBFS);
    assert_eq!(LevelWindow::default().rms_dbfs(), SILENCE_FLOOR_DBFS);
}

#[test]
fn all_zero_samples_are_the_silence_floor() {
    assert_eq!(rms_dbfs(&[0.0; 960]), SILENCE_FLOOR_DBFS);
}

#[test]
fn full_scale_square_is_zero_dbfs() {
    assert_db(rms_dbfs(&square(960, 1.0)), 0.0, 1e-4);
}

#[test]
fn full_scale_sine_is_minus_3_01_dbfs() {
    assert_db(rms_dbfs(&sine(1.0)), -3.0103, 0.05);
}

#[test]
fn half_amplitude_square_is_minus_6_02_dbfs() {
    assert_db(rms_dbfs(&square(960, 0.5)), -6.0206, 0.01);
}

#[test]
fn a_quiet_signal_above_the_floor_is_reported_exactly() {
    // 1e-6 full scale = −120 dBFS: well above the floor, never clamped.
    assert_db(rms_dbfs(&square(960, 1e-6)), -120.0, 0.01);
}

#[test]
fn a_signal_below_the_floor_clamps_to_it() {
    // 1e-12 full scale = −240 dBFS: below the floor, reported AS the floor.
    assert_eq!(rms_dbfs(&square(960, 1e-12)), SILENCE_FLOOR_DBFS);
}

#[test]
fn mean_square_conversion_is_ten_log10() {
    assert_db(dbfs_from_mean_square(1.0), 0.0, 1e-6);
    assert_db(dbfs_from_mean_square(0.01), -20.0, 1e-4);
    assert_db(dbfs_from_mean_square(0.25), -6.0206, 1e-3);
    assert_eq!(dbfs_from_mean_square(0.0), SILENCE_FLOOR_DBFS);
    assert_eq!(dbfs_from_mean_square(f64::NAN), SILENCE_FLOOR_DBFS);
}

#[test]
fn window_accumulates_across_adds() {
    let mut w = LevelWindow::default();
    w.add(&[1.0, -1.0]);
    w.add(&[0.0, 0.0]);
    // Σx² = 2 over 4 samples → mean square 0.5 → −3.01 dBFS.
    assert_eq!(w.samples(), 4);
    assert_db(w.rms_dbfs(), -3.0103, 1e-3);
    // Reading the level does NOT reset the window.
    assert_eq!(w.samples(), 4);
}

#[test]
fn take_reads_then_resets_the_window() {
    let mut w = LevelWindow::default();
    w.add(&square(8, 1.0));
    let (db, n) = w.take();
    assert_db(db, 0.0, 1e-4);
    assert_eq!(n, 8);

    // The window is empty right after a take.
    assert_eq!(w.samples(), 0);
    assert_eq!(w.take(), (SILENCE_FLOOR_DBFS, 0));

    // The NEXT window measures only its own samples — a leftover sum of squares
    // from the first window would read 0 dBFS here instead of −6.02.
    w.add(&square(4, 0.5));
    let (db, n) = w.take();
    assert_db(db, -6.0206, 1e-3);
    assert_eq!(n, 4);
}
