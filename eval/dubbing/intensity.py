#!/usr/bin/env python3
"""intensity.py — measure the ORIGINAL speaker's per-sentence delivery intensity
and map it to per-engine style controls.

Round-2 owner bar (#175): the dub must carry the preacher's INTENSITY, not read
"ako rozprávanie príbehu". This module measures three per-sentence signals from
the original voice stem — loudness (RMS dB), pitch range (f0 spread in
semitones), speech rate (words/second) — and maps each sentence to a delivery
label (`intense` / `neutral` / `calm`) RELATIVE to the segment's own
distribution, plus the matching Gemini style instruction and Soniox audio tag.

The measurement (`measure_window`) needs librosa (runtime, dev1). The mapping
(`classify`, `gemini_style`, `soniox_tag`) is PURE and unit-tested in CI.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Intensity:
    rms_db: float  # loudness of the sentence (dBFS)
    f0_semitones: float  # pitch spread p90-p10 in semitones (expressiveness)
    words_per_s: float  # speech rate


def speech_rate_wps(word_count: int, dur_s: float) -> float:
    """Pure: words per second, 0.0 for a non-positive duration."""
    return round(word_count / dur_s, 3) if dur_s > 0 else 0.0


def classify(
    val: Intensity,
    rms_hi: float,
    rms_lo: float,
    f0_hi: float,
) -> str:
    """Pure: label a sentence relative to the segment's own thresholds.

    `intense` = loud (>= rms_hi) OR wide pitch (>= f0_hi); `calm` = quiet
    (<= rms_lo, and — having passed the intense check — necessarily narrow
    pitch); otherwise `neutral`.
    """
    if val.rms_db >= rms_hi or val.f0_semitones >= f0_hi:
        return "intense"
    if val.rms_db <= rms_lo:
        return "calm"
    return "neutral"


GEMINI_STYLE = {
    "intense": "Povedz to dôrazne a naliehavo, zvýšeným hlasom, ako kazateľ ktorý"
    " intenzívne hovorí k ľuďom:",
    "neutral": "Povedz to prirodzene, ako kazateľ na bohoslužbe:",
    "calm": "Povedz to pokojne a stíšene, dôverne:",
}

SONIOX_TAG = {"intense": "[emphatic]", "neutral": "", "calm": "[calm]"}


def gemini_style(label: str) -> str:
    """Pure: Gemini per-sentence style instruction for a delivery label."""
    return GEMINI_STYLE.get(label, GEMINI_STYLE["neutral"])


def soniox_tag(label: str) -> str:
    """Pure: Soniox audio tag prefix for a delivery label ('' for neutral)."""
    return SONIOX_TAG.get(label, "")


def thresholds(values: list[Intensity]) -> tuple[float, float, float]:
    """Pure: derive (rms_hi, rms_lo, f0_hi) from the segment's own distribution
    (percentile-free, dependency-free): rms_hi/lo = mean ± 0.5*range-ish via the
    max/min midpoints; f0_hi = midpoint of the f0 range. Robust for ~7-34 items."""
    if not values:
        return (0.0, 0.0, 0.0)
    rms = sorted(v.rms_db for v in values)
    f0 = sorted(v.f0_semitones for v in values)
    rms_mid = (rms[0] + rms[-1]) / 2
    rms_hi = (rms_mid + rms[-1]) / 2
    rms_lo = (rms[0] + rms_mid) / 2
    f0_hi = (f0[len(f0) // 2] + f0[-1]) / 2
    return (round(rms_hi, 2), round(rms_lo, 2), round(f0_hi, 2))


def measure_window(samples, sr: int) -> Intensity:
    """Runtime (librosa): measure RMS dB + f0 spread of a mono float array."""
    import logging

    import librosa
    import numpy as np

    a = np.asarray(samples, dtype="float32").reshape(-1)
    if a.size == 0:
        return Intensity(-120.0, 0.0, 0.0)
    rms = float(np.sqrt(np.mean(a**2)) + 1e-9)
    rms_db = round(20.0 * np.log10(rms), 2)
    try:
        f0, _, _ = librosa.pyin(a, sr=sr, fmin=70, fmax=350, frame_length=2048)
        f0 = f0[~np.isnan(f0)]
        if f0.size >= 4:
            lo = np.percentile(f0, 10)
            hi = np.percentile(f0, 90)
            semi = round(float(12.0 * np.log2((hi + 1e-6) / (lo + 1e-6))), 2)
        else:
            semi = 0.0
    except (librosa.util.exceptions.ParameterError, ValueError, FloatingPointError):
        # pyin can fail on a too-short / silent window; treat as no pitch spread.
        logging.getLogger("dubbing_eval.intensity").warning(
            "pyin failed for a %d-sample window; f0 spread set to 0",
            a.size,
            exc_info=True,
        )
        semi = 0.0
    return Intensity(rms_db=rms_db, f0_semitones=semi, words_per_s=0.0)
