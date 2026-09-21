"""Pure-helper tests for scripts/dub_voice_check.py (#184 round C).

The voice-consistency check is a dev1/box tool, but its pure helpers (the spread
math, the autocorrelation f0 estimate, the per-window median, and the pass/fail
decision) run in CI's `eval-checks` job. It uses numpy + soundfile only — the
autocorrelation fallback is forced (`use_librosa=False`) so the test RUNS without
librosa (which CI does not install), never skips.
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


def test_spread_semitones_octave_is_twelve():
    # An octave (110 -> 220 Hz) is exactly 12 semitones.
    assert abs(dvc.spread_semitones([110.0, 220.0]) - 12.0) < 0.05
    # Fewer than two voiced windows -> no spread.
    assert dvc.spread_semitones([110.0]) == 0.0
    assert dvc.spread_semitones([]) == 0.0
    # Unvoiced (<= 0) windows are ignored.
    assert dvc.spread_semitones([0.0, 110.0]) == 0.0


def test_f0_autocorr_recovers_a_pure_tone():
    sr = 24000
    f = dvc.f0_autocorr(_tone(110.0, sr, 0.2), sr)
    assert f is not None
    assert abs(f - 110.0) < 5.0
    # Silence is unvoiced -> None.
    assert dvc.f0_autocorr(np.zeros(2048, dtype="float32"), sr) is None


def test_iqr_semitones_alternating_voices_is_an_octave():
    # Windows alternating between two voices an octave apart: the interquartile
    # spread of the per-window medians (vs the file median) is the full octave —
    # this is the "rotating dabéri" signature the check must flag.
    meds = [110.0, 220.0] * 4
    assert abs(dvc.iqr_semitones(meds) - 12.0) < 0.05
    # Fewer than two voiced windows -> no spread; unvoiced windows are ignored.
    assert dvc.iqr_semitones([110.0]) == 0.0
    assert dvc.iqr_semitones([]) == 0.0
    assert dvc.iqr_semitones([0.0, 110.0]) == 0.0


def test_one_voice_with_natural_intonation_is_within_the_limit():
    # ONE male voice over 36 min of speech (the re-dubbed sample measured on
    # 2026-09-21): per-window f0 medians 82–193 Hz, IQR 4.5 st, max spread
    # 14.7 st. A max-spread rule at 3 st flags it as "rotating"; the IQR rule
    # (limit MAX_IQR_ST = 8) passes it and still flags the alternating octave.
    meds = [95.0, 100.0, 110.0, 120.0, 105.0, 98.0, 88.0, 143.0, 104.0, 92.0]
    assert dvc.spread_semitones(meds) > 3.0, "the old max-spread rule fails it"
    assert dvc.iqr_semitones(meds) <= dvc.MAX_IQR_ST
    assert dvc.MAX_IQR_ST == 8.0


def test_two_voice_file_flags_over_the_limit(tmp_path):
    # Eight 0.5 s windows alternating 110 / 220 Hz -> IQR 12 st -> exit 1.
    sr = 24000
    sig = np.concatenate([_tone(110.0, sr, 0.5), _tone(220.0, sr, 0.5)] * 4)
    path = os.path.join(tmp_path, "two_voice.wav")
    sf.write(path, sig, sr)

    medians, spread, iqr, exceeded = dvc.check(path, win_s=0.5, use_librosa=False)
    assert len(medians) == 8
    assert abs(medians[0] - 110.0) < 5.0
    assert abs(medians[1] - 220.0) < 5.0
    assert spread > 11.0
    assert iqr > dvc.MAX_IQR_ST
    assert exceeded is True


def test_one_voice_file_passes(tmp_path):
    # A single 110 Hz voice across both windows -> ~0 st spread -> exit 0.
    sr = 24000
    sig = _tone(110.0, sr, 1.0)
    path = os.path.join(tmp_path, "one_voice.wav")
    sf.write(path, sig, sr)

    medians, spread, iqr, exceeded = dvc.check(path, win_s=0.5, use_librosa=False)
    assert len(medians) == 2
    assert spread <= 1.0
    assert iqr <= dvc.MAX_IQR_ST
    assert exceeded is False
