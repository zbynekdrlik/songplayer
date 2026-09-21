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


def test_two_voice_file_flags_over_the_limit(tmp_path):
    # Two 0.5 s windows: 110 Hz then 220 Hz -> ~12 st spread -> exit 1.
    sr = 24000
    sig = np.concatenate([_tone(110.0, sr, 0.5), _tone(220.0, sr, 0.5)])
    path = os.path.join(tmp_path, "two_voice.wav")
    sf.write(path, sig, sr)

    medians, spread, exceeded = dvc.check(path, win_s=0.5, use_librosa=False)
    assert len(medians) == 2
    assert abs(medians[0] - 110.0) < 5.0
    assert abs(medians[1] - 220.0) < 5.0
    assert spread > 3.0
    assert exceeded is True


def test_one_voice_file_passes(tmp_path):
    # A single 110 Hz voice across both windows -> ~0 st spread -> exit 0.
    sr = 24000
    sig = _tone(110.0, sr, 1.0)
    path = os.path.join(tmp_path, "one_voice.wav")
    sf.write(path, sig, sr)

    medians, spread, exceeded = dvc.check(path, win_s=0.5, use_librosa=False)
    assert len(medians) == 2
    assert spread <= 3.0
    assert exceeded is False
