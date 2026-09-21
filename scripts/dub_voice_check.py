#!/usr/bin/env python3
"""dub_voice_check.py — objective dub voice-consistency check (#184 round E).

Reads a dub audio file (FLAC/WAV), splits it into fixed 5-s windows, measures the
f0 median (voiced frames) per window, and FLAGS the file when more than
`MAX_HIGH_BAND_FRACTION` of its voiced windows sit farther than 6 semitones ABOVE
the file median — the rotating-voice symptom (recurring female stretches above a
base male voice). The per-window max spread and interquartile spread are still
computed and printed, but only for information: round C's IQR/max-spread rules
DILUTED a 100-s female stretch into a passing number, so the gate is now a
FRACTION of high-band windows, not a spread.

A dev1/box tool — NOT wired into CI. The f0 measurement uses `librosa.pyin` when
available and falls back to a dependency-free autocorrelation estimate otherwise,
so the pure helpers are unit-testable in the CI eval-checks job (which installs
numpy + soundfile but NOT librosa) — the test RUNS the fallback, never skips.
"""

from __future__ import annotations

import argparse
import math
import sys

import numpy as np

# Window length for the per-window f0 median. Round E dropped it from 30 s to 5 s
# so a rotating stretch is measured, not averaged away into the file median.
WINDOW_S = 5.0
# Fail when more than this FRACTION of the voiced 5-s windows sit farther than
# `HIGH_BAND_ST` semitones ABOVE the file median. Measured 2026-09-21 on the
# 36-min re-dubbed sample: a single pinned male voice keeps ~0 % of windows in
# the high band (natural intonation stays under 6 st above its own median), while
# a female↔male rotation puts whole 5-s windows an octave up. A truly balanced
# 50/50 octave alternation is NOT flagged by design — at most ~50 % of windows
# can exceed the median and the arithmetic median of an equal split sits too
# close to the high voice — but that is not the real symptom (a base voice with
# recurring high stretches always keeps its median at the base).
MAX_HIGH_BAND_FRACTION = 0.05
# The semitone band above the file median that counts a window as "high".
HIGH_BAND_ST = 6.0
FMIN = 70.0
FMAX = 350.0
FRAME_LEN = 2048
HOP = 1024


def spread_semitones(medians: list[float]) -> float:
    """Pure: the max f0 spread across windows, in semitones. Windows with no
    voiced pitch (<= 0) are ignored; < 2 usable windows means no spread.
    Information only — no longer the gate."""
    vals = [m for m in medians if m > 0]
    if len(vals) < 2:
        return 0.0
    return 12.0 * math.log2(max(vals) / min(vals))


def iqr_semitones(medians: list[float]) -> float:
    """Pure: the interquartile spread (p75 − p25) of the per-window f0 medians
    expressed in semitones relative to the file median — robust to the odd
    mis-estimated window. Windows with no voiced pitch (<= 0) are ignored; < 2
    usable windows means no spread. Information only — no longer the gate."""
    vals = [m for m in medians if m > 0]
    if len(vals) < 2:
        return 0.0
    arr = np.asarray(vals, dtype=np.float64)
    st = 12.0 * np.log2(arr / np.median(arr))
    return float(np.percentile(st, 75) - np.percentile(st, 25))


def high_band_fraction(medians: list[float], st_above: float = HIGH_BAND_ST) -> float:
    """Pure: the FRACTION of voiced windows whose f0 median is more than
    `st_above` semitones ABOVE the file median (the median of the voiced
    per-window medians). Windows with no voiced pitch (<= 0) are ignored; < 2
    usable windows means no fraction (0.0). This is the round-E gate signal."""
    vals = [m for m in medians if m > 0]
    if len(vals) < 2:
        return 0.0
    arr = np.asarray(vals, dtype=np.float64)
    st = 12.0 * np.log2(arr / np.median(arr))
    return float(np.mean(st > st_above))


def f0_autocorr(frame, sr: int, fmin: float = FMIN, fmax: float = FMAX) -> float | None:
    """Pure: estimate the f0 (Hz) of one mono float frame by autocorrelation, or
    None when the frame is too quiet / has no clear peak in [fmin, fmax] (treated
    as unvoiced). Dependency-free (numpy only), so it runs without librosa."""
    x = np.asarray(frame, dtype=np.float64).reshape(-1)
    if x.size < 2:
        return None
    x = x - x.mean()
    if math.sqrt(float(np.mean(x**2))) < 1e-4:
        return None
    corr = np.correlate(x, x, mode="full")[x.size - 1 :]
    min_lag = max(1, int(sr / fmax))
    max_lag = min(len(corr) - 1, int(sr / fmin))
    if max_lag <= min_lag:
        return None
    seg = corr[min_lag : max_lag + 1]
    peak = int(np.argmax(seg)) + min_lag
    if peak <= 0 or corr[peak] <= 0:
        return None
    return sr / peak


def _librosa_median(samples, sr: int, fmin: float, fmax: float) -> float | None:
    """librosa.pyin f0 median over voiced frames, or None when librosa is absent
    (the CI eval-checks env has numpy + soundfile but no librosa)."""
    try:
        import librosa
    except ImportError:
        return None
    a = np.asarray(samples, dtype="float32").reshape(-1)
    f0, _, _ = librosa.pyin(a, sr=sr, fmin=fmin, fmax=fmax, frame_length=FRAME_LEN)
    f0 = f0[~np.isnan(f0)]
    return float(np.median(f0)) if f0.size else 0.0


def median_f0(
    samples, sr: int, use_librosa: bool = True, fmin: float = FMIN, fmax: float = FMAX
) -> float:
    """The median voiced f0 (Hz) of a mono float chunk. Prefers librosa; falls
    back to frame-wise autocorrelation (no librosa needed). 0.0 when unvoiced."""
    a = np.asarray(samples, dtype=np.float64).reshape(-1)
    if a.size == 0:
        return 0.0
    if use_librosa:
        med = _librosa_median(a, sr, fmin, fmax)
        if med is not None:
            return med
    vals: list[float] = []
    last = max(1, a.size - FRAME_LEN + 1)
    for start in range(0, last, HOP):
        f = f0_autocorr(a[start : start + FRAME_LEN], sr, fmin, fmax)
        if f is not None:
            vals.append(f)
    return float(np.median(vals)) if vals else 0.0


def window_medians(
    samples, sr: int, win_s: float = WINDOW_S, use_librosa: bool = True
) -> list[float]:
    """Per-window median f0 (Hz). A trailing sliver shorter than 0.25 s is
    dropped so a near-empty tail window does not skew the fraction."""
    a = np.asarray(samples, dtype=np.float64).reshape(-1)
    win = max(1, int(win_s * sr))
    out: list[float] = []
    for start in range(0, a.size, win):
        seg = a[start : start + win]
        if seg.size < sr * 0.25:
            continue
        out.append(median_f0(seg, sr, use_librosa))
    return out


def _read_audio(path: str):
    """Read an audio file to (mono float array, sample_rate) via soundfile."""
    import soundfile as sf

    data, sr = sf.read(path, dtype="float32", always_2d=False)
    data = np.asarray(data)
    if data.ndim > 1:
        data = data.mean(axis=1)
    return data, sr


def check(
    path: str, win_s: float = WINDOW_S, use_librosa: bool = True
) -> tuple[list[float], float, float, float, bool]:
    """Read `path`, return (per-window f0 medians, max spread in semitones, IQR
    spread in semitones, high-band fraction, exceeded) where `exceeded` is True
    when the high-band fraction is over [`MAX_HIGH_BAND_FRACTION`] (the spread /
    IQR are reported for information only)."""
    samples, sr = _read_audio(path)
    medians = window_medians(samples, sr, win_s, use_librosa)
    spread = spread_semitones(medians)
    iqr = iqr_semitones(medians)
    high_fraction = high_band_fraction(medians)
    return medians, spread, iqr, high_fraction, high_fraction > MAX_HIGH_BAND_FRACTION


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Objective dub voice-consistency check (#184 round E)"
    )
    parser.add_argument("audio", help="dub audio file (FLAC/WAV)")
    parser.add_argument("--window", type=float, default=WINDOW_S)
    args = parser.parse_args()

    medians, spread, iqr, high_fraction, exceeded = check(args.audio, args.window)
    for i, m in enumerate(medians):
        print(f"window {i}: f0 median {m:.1f} Hz")
    print(f"max spread: {spread:.2f} semitones (information only)")
    print(f"IQR spread: {iqr:.2f} semitones (information only)")
    print(
        f"high-band fraction: {high_fraction:.3f} "
        f"(>{HIGH_BAND_ST:.0f} st above median; limit {MAX_HIGH_BAND_FRACTION})"
    )
    if exceeded:
        print(
            "FAIL: dub voice is not consistent (high-band fraction exceeds the limit)"
        )
        sys.exit(1)
    print("OK: dub voice is consistent")


if __name__ == "__main__":
    main()
