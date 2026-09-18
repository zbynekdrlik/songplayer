"""Pure unit tests for eval/dubbing/intensity.py — the feature→style mapping and
threshold derivation (no librosa, no audio)."""

from __future__ import annotations

from eval.dubbing import intensity
from eval.dubbing.intensity import Intensity


def test_speech_rate():
    assert intensity.speech_rate_wps(10, 5.0) == 2.0
    assert intensity.speech_rate_wps(3, 0.0) == 0.0


def test_classify_intense_on_loud_or_wide_pitch():
    loud = Intensity(rms_db=-10, f0_semitones=2, words_per_s=3)
    wide = Intensity(rms_db=-30, f0_semitones=12, words_per_s=3)
    assert intensity.classify(loud, rms_hi=-15, rms_lo=-35, f0_hi=8) == "intense"
    assert intensity.classify(wide, rms_hi=-15, rms_lo=-35, f0_hi=8) == "intense"


def test_classify_calm_on_quiet_and_narrow():
    quiet = Intensity(rms_db=-40, f0_semitones=3, words_per_s=2)
    assert intensity.classify(quiet, rms_hi=-15, rms_lo=-35, f0_hi=8) == "calm"


def test_classify_neutral_in_between():
    mid = Intensity(rms_db=-25, f0_semitones=5, words_per_s=3)
    assert intensity.classify(mid, rms_hi=-15, rms_lo=-35, f0_hi=8) == "neutral"


def test_style_and_tag_mappings():
    assert "naliehavo" in intensity.gemini_style("intense")
    assert "pokojne" in intensity.gemini_style("calm")
    assert intensity.gemini_style("neutral") == intensity.GEMINI_STYLE["neutral"]
    # unknown label falls back to neutral
    assert intensity.gemini_style("???") == intensity.GEMINI_STYLE["neutral"]
    assert intensity.soniox_tag("intense") == "[emphatic]"
    assert intensity.soniox_tag("neutral") == ""


def test_thresholds_ordering_and_empty():
    vals = [
        Intensity(-40, 2, 0),
        Intensity(-30, 6, 0),
        Intensity(-20, 10, 0),
        Intensity(-10, 14, 0),
    ]
    rms_hi, rms_lo, f0_hi = intensity.thresholds(vals)
    assert rms_lo < rms_hi
    assert f0_hi > 0
    assert intensity.thresholds([]) == (0.0, 0.0, 0.0)
