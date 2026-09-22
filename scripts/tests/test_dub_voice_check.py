"""Pure-helper tests for scripts/dub_voice_check.py (#184 round E).

The voice-consistency check is a dev1/box tool, but its pure helpers (the f0
estimate, the per-window median, and the high-band FRACTION gate) run in CI's
`eval-checks` job. It uses numpy + soundfile only — the autocorrelation fallback
is forced (`use_librosa=False`) so the test RUNS without librosa (which CI does
not install), never skips.

Round E replaces the round-C interquartile-spread rule with a FRACTION rule:
the check flags a file when more than `MAX_HIGH_BAND_FRACTION` of its voiced
5-s windows sit farther than 6 st ABOVE the file median — the rotating-voice
symptom (recurring female stretches above a base male voice). A balanced 50/50
octave alternation is NOT flagged by design (by definition at most ~50 % of
windows can exceed the median, and an arithmetic median of an equal split sits
too close to the high voice), so the "rotation" fixtures below are base-dominant
with recurring high stretches, exactly the real symptom.
"""

import importlib.util
import os

import numpy as np
import soundfile as sf

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_dub_voice_check():
    path = os.path.join(_SCRIPTS_DIR, "dub_voice_check.py")
    spec = importlib.util.spec_from_file_location("dub_voice_check", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dvc = _load_dub_voice_check()


def _tone(freq: float, sr: int, dur_s: float) -> np.ndarray:
    t = np.arange(int(sr * dur_s)) / sr
    return (0.5 * np.sin(2 * np.pi * freq * t)).astype("float32")


def test_round_e_constants():
    # The gate is a 5 % fraction of 5-s windows more than 6 st above the median.
    assert dvc.WINDOW_S == 5.0
    assert dvc.MAX_HIGH_BAND_FRACTION == 0.05


def test_high_band_fraction_one_voice_is_zero():
    # A single steady voice — nothing above the median.
    assert dvc.high_band_fraction([110.0] * 8) == 0.0
    # Natural intonation within +-4 st never exceeds the 6 st band.
    meds = [110.0 * 2 ** (x / 12) for x in [-4, -2, 0, 2, 4, -3, 1, 3]]
    assert dvc.high_band_fraction(meds) == 0.0
    # Fewer than two voiced windows -> no fraction; unvoiced ignored.
    assert dvc.high_band_fraction([110.0]) == 0.0
    assert dvc.high_band_fraction([]) == 0.0
    assert dvc.high_band_fraction([0.0, 110.0]) == 0.0


def test_high_band_fraction_flags_a_high_stretch():
    # A 3-of-30-window 220 Hz stretch in a 110 Hz voice: median stays at 110, the
    # three octave-up windows are ~12 st above -> 10 % of voiced windows.
    frac = dvc.high_band_fraction([110.0] * 27 + [220.0] * 3)
    assert abs(frac - 0.1) < 1e-9
    assert frac > dvc.MAX_HIGH_BAND_FRACTION
    # A base-dominant rotation (recurring female stretches) -> 40 %.
    assert abs(dvc.high_band_fraction([110.0] * 6 + [220.0] * 4) - 0.4) < 1e-9


def test_check_flags_a_rotating_file(tmp_path):
    # Ten 0.5 s windows: six 110 Hz then four 220 Hz (a base voice with a long
    # high stretch). 40 % > 5 % -> exceeded, via the 5-tuple check().
    sr = 24000
    sig = np.concatenate([_tone(110.0, sr, 0.5)] * 6 + [_tone(220.0, sr, 0.5)] * 4)
    path = os.path.join(tmp_path, "rotating.wav")
    sf.write(path, sig, sr)

    medians, spread, iqr, high_fraction, true_drift, exceeded = dvc.check(
        path, win_s=0.5, use_librosa=False
    )
    assert len(medians) == 10
    assert abs(medians[0] - 110.0) < 5.0
    assert abs(medians[-1] - 220.0) < 5.0
    # spread + IQR stay available as information only.
    assert spread > 11.0
    assert iqr > 0.0
    assert abs(high_fraction - 0.4) < 1e-9
    # No --source -> round-E behaviour: true_drift is None, gate on high-band.
    assert true_drift is None
    assert exceeded is True


def test_check_passes_one_voice(tmp_path):
    # A single 110 Hz voice across both windows -> 0 % high band -> OK.
    sr = 24000
    sig = _tone(110.0, sr, 1.0)
    path = os.path.join(tmp_path, "one_voice.wav")
    sf.write(path, sig, sr)

    medians, spread, iqr, high_fraction, true_drift, exceeded = dvc.check(
        path, win_s=0.5, use_librosa=False
    )
    assert len(medians) == 2
    assert high_fraction == 0.0
    assert true_drift is None
    assert exceeded is False


def test_f0_autocorr_recovers_a_pure_tone():
    sr = 24000
    f = dvc.f0_autocorr(_tone(110.0, sr, 0.2), sr)
    assert f is not None
    assert abs(f - 110.0) < 5.0
    # Silence is unvoiced -> None.
    assert dvc.f0_autocorr(np.zeros(2048, dtype="float32"), sr) is None


def test_spread_and_iqr_remain_for_information():
    # An octave (110 -> 220 Hz) is exactly 12 semitones (max spread, info only).
    assert abs(dvc.spread_semitones([110.0, 220.0]) - 12.0) < 0.05
    # IQR still available; a single value has no spread.
    assert dvc.iqr_semitones([110.0]) == 0.0


# ── #184 round E2: the shared source-discounted drift definition ─────────────────


def test_drift_windows_against_baseline_counts_high_output():
    # Every window an octave above the baseline, source steady below its own
    # baseline -> all counted as drift.
    assert dvc.drift_windows([220.0] * 4, [110.0] * 4, 108.0, 131.0) == (4, 4)


def test_drift_windows_no_baseline_yet_counts_nothing():
    # The seed chunk has no running baseline yet -> nothing can drift (0, voiced).
    assert dvc.drift_windows([220.0] * 4, [110.0] * 4, None, 131.0) == (0, 4)
    assert dvc.drift_windows([220.0] * 4, [110.0] * 4, 0.0, 131.0) == (0, 4)


def test_drift_windows_source_following_is_discounted():
    # The source is ALSO high at the same index (above the source baseline) -> the
    # dub is correctly following it, not drifting.
    assert dvc.drift_windows([220.0] * 4, [220.0] * 4, 108.0, 108.0) == (0, 4)


def test_drift_windows_aligns_mismatched_counts():
    # Fewer source windows (an atempo compressed them): a high output stretch over
    # a steady source still counts after fraction-alignment.
    assert dvc.drift_windows([110.0] * 7 + [220.0] * 3, [110.0] * 5, 108.0, 110.0) == (
        3,
        10,
    )


def test_true_drift_fraction_synthetic_medians():
    # File median stays at 110 (base-dominant); the two 220 windows drift over a
    # steady source -> 2/10.
    frac = dvc.true_drift_fraction([110.0] * 8 + [220.0] * 2, [110.0] * 10)
    assert abs(frac - 0.2) < 1e-9
    # Source ALSO high at the same indices -> source-following -> 0.
    both = [110.0] * 8 + [220.0] * 2
    assert dvc.true_drift_fraction(both, both) == 0.0
    # Fewer than two voiced output windows -> 0.0.
    assert dvc.true_drift_fraction([110.0], [110.0]) == 0.0


def test_check_source_gate_discounts_following(tmp_path):
    # #184 round E2: with --source the gated number is the true-drift fraction. A
    # rotating dub over a STEADY source flags; the SAME dub over a source that
    # rises at the same spot does NOT (source-following discounted).
    sr = 24000
    dub = np.concatenate([_tone(110.0, sr, 0.5)] * 6 + [_tone(220.0, sr, 0.5)] * 4)
    dub_path = os.path.join(tmp_path, "dub.wav")
    sf.write(dub_path, dub, sr)

    steady = np.concatenate([_tone(110.0, sr, 0.5)] * 10)
    steady_path = os.path.join(tmp_path, "steady_src.wav")
    sf.write(steady_path, steady, sr)

    _, _, _, high_fraction, true_drift, exceeded = dvc.check(
        dub_path, win_s=0.5, use_librosa=False, source=steady_path
    )
    assert abs(high_fraction - 0.4) < 1e-9
    assert true_drift is not None and abs(true_drift - 0.4) < 1e-9
    assert exceeded is True

    following = np.concatenate(
        [_tone(110.0, sr, 0.5)] * 6 + [_tone(220.0, sr, 0.5)] * 4
    )
    following_path = os.path.join(tmp_path, "following_src.wav")
    sf.write(following_path, following, sr)

    _, _, _, _, true_drift2, exceeded2 = dvc.check(
        dub_path, win_s=0.5, use_librosa=False, source=following_path
    )
    assert true_drift2 == 0.0
    assert exceeded2 is False
