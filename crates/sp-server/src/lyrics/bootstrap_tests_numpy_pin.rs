//! RED tests (#144) for the lyrics-venv numpy-pin repair.
//!
//! Root cause (win-resolume, 2026-09-11): the cu124 torch
//! `--upgrade --force-reinstall` step re-resolves torch's dependency tree
//! ignoring the constraints of already-installed packages, pulling numpy
//! 2.5.2 next to numba 0.65 (which caps numpy at 2.4). Every
//! `preprocess-vocals` run then died with "Numba needs NumPy 2.4 or less".
//! `IS_READY_PROBE` only imported qwen_asr/torch/audio_separator, so the
//! venv still reported "ready" while every song failed isolation.
//!
//! Sibling file wired from `bootstrap.rs` via
//! `#[path = "bootstrap_tests_numpy_pin.rs"] #[cfg(test)]
//! mod bootstrap_tests_numpy_pin;` to honor the airuleset 1000-line cap.

use super::*;

/// The `is_ready` probe must also import the numeric stack (numba / librosa
/// / soundfile). A broken numba (numpy too new) must make the venv report
/// "not ready" so bootstrap repairs it, instead of shipping a venv that
/// fails on every song.
#[test]
fn is_ready_probe_imports_numeric_stack() {
    for pkg in ["numba", "librosa", "soundfile"] {
        assert!(
            IS_READY_PROBE.contains(pkg),
            "IS_READY_PROBE must import {pkg} so a broken numeric stack is judged not-ready, got: {IS_READY_PROBE:?}"
        );
    }
}

/// The numpy pin must cap below 2.5 — numba 0.65/0.66 require numpy <= 2.4.
#[test]
fn numpy_pin_caps_below_2_5() {
    assert_eq!(
        NUMPY_PIN, "numpy<2.5",
        "NUMPY_PIN must cap numpy below 2.5 (numba 0.65/0.66 need numpy <= 2.4)"
    );
}

/// Structural guard: the numpy pin repair must run AFTER the cu124 torch
/// force-reinstall (which is what breaks the pin), and NUMPY_PIN must be
/// passed to a `pip install`. CRLF-normalised for the Windows CI checkout.
#[test]
fn bootstrap_repairs_numpy_pin_after_torch_reinstall() {
    let src = include_str!("bootstrap.rs").replace("\r\n", "\n");
    let torch_pos = src
        .find("torch==2.6.0+cu124")
        .expect("bootstrap.rs must pin torch==2.6.0+cu124");
    let pin_pos = src
        .find("repairing numpy pin")
        .expect("bootstrap.rs must log the numpy pin repair step");
    assert!(
        torch_pos < pin_pos,
        "the numpy pin repair must run AFTER the cu124 torch force-reinstall that breaks it"
    );
    assert!(
        src.contains("\"install\", NUMPY_PIN"),
        "NUMPY_PIN must be passed as a pip install argument"
    );
}
