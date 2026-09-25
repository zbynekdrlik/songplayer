"""Offset + rate fit and band-limited warp for the post-deploy A/V gate (#147).

SongPlayer's genlocked output and the OBS audio clock that records the program
run at slightly different rates. Push run 36081072389 recorded the ORIGINAL with
a perfectly smooth +16.8 ppm difference: the local lag walked -3.35 -> +12.43
samples at 48 kHz over 19.6 s, with no slips or steps, so nothing was audible.
``av_sync_check`` correlates the whole take at ONE lag. A 0.33 ms walk
decorrelates everything above ~1 kHz (corr 0.84 < 0.9), and the fixed-lag
dropout scan reads 89 "glitches". So the gate could not measure a legitimate,
inaudible drift.

Two-stage alignment:

1. ``av_sync_check.audio_offset`` finds the coarse global lag (unchanged).
2. ``local_lags`` measures a sub-sample lag per ``FIT_WINDOW_S`` window of the
   recording. It uses a normalized cross-correlation searched +-``FIT_SEARCH_S``
   around the global lag, with a band-limited (sinc) refinement of the peak.
   ``fit_rate`` least-squares fits ``deviation(i) = a + b·i``:
   recording sample ``i`` is original position ``lag + a + (1 + b)·i``, and
   ``b·1e6`` is the rate difference in ppm.
3. ``compensate`` warps the original onto the recording's time base with a
   Blackman-windowed sinc (``warp``). Linear interpolation (``np.interp``)
   would attenuate the very band the gate checks. The correlation, the
   dropout scan and the glitch scan then run against the WARPED original.

Bounds: only ``|rate| <= MAX_WARP_PPM`` with a max fit residual
``<= MAX_FIT_RESIDUAL_MS`` is warped. A larger rate, a step/slip (the line
cannot fit it) or too few measurable windows leave the take UNWARPED. The
analysis is then exactly the old one-lag analysis, and the fit numbers are
reported as a fault indicator. A real resync still fails exactly as before;
the thresholds of the verdict are unchanged.

Pure numpy (the Eval Checks CI image has numpy + soundfile, no scipy).
Covered by ``scripts/tests/test_av_sync_warp.py``.
"""

from __future__ import annotations

import numpy as np

FIT_WINDOW_S = 0.25  # short enough that 200+ ppm does not smear one window
FIT_SEARCH_S = 0.05  # +- around the global lag (a 12 ms step is inside)
FIT_MIN_CORR = 0.6  # a window below this local corr is left out of the fit
FIT_MIN_WINDOWS = 8
MAX_WARP_PPM = 200.0
MAX_FIT_RESIDUAL_MS = 0.5
REFINE_HALF = 16  # xcorr samples each side of the peak for the sinc refinement
REFINE_STEPS = 64  # refinement grid: 1/64 sample
SINC_HALF_TAPS = 32  # warp kernel half-length (samples)


def _refine_peak(xc: np.ndarray, k: int) -> float:
    """Sub-sample position of the maximum of the band-limited sequence ``xc``
    near its integer argmax ``k``: sinc interpolation on a 1/64-sample grid
    over +-1 sample, then a parabola through the best grid point."""
    lo, hi = max(0, k - REFINE_HALF), min(len(xc), k + REFINE_HALF + 1)
    j = np.arange(lo, hi)
    grid = k + np.linspace(-1.0, 1.0, 2 * REFINE_STEPS + 1)
    vals = (xc[lo:hi][None, :] * np.sinc(grid[:, None] - j[None, :])).sum(axis=1)
    g = int(np.argmax(vals))
    frac = 0.0
    if 0 < g < len(vals) - 1:
        y0, y1, y2 = vals[g - 1], vals[g], vals[g + 1]
        den = y0 - 2.0 * y1 + y2
        if den < 0.0:
            frac = float(np.clip(0.5 * (y0 - y2) / den, -0.5, 0.5))
    return float(grid[g] + frac / REFINE_STEPS)


def local_lags(
    rec: np.ndarray,
    orig: np.ndarray,
    lag: int,
    sr: int,
    window_s: float = FIT_WINDOW_S,
    search_s: float = FIT_SEARCH_S,
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Per-window sub-sample deviation from the global alignment.

    Returns ``(t, dev, corr)``. For every whole ``window_s`` window of ``rec``
    whose local correlation peak lies inside the search and reaches
    ``FIT_MIN_CORR``: ``t`` = the window's centre (recording samples), ``dev``
    = original position of the window's first sample minus ``lag + i0``
    (samples, sub-sample), and ``corr`` = the peak normalized correlation.
    Silent windows and windows whose search leaves the original are skipped.
    """
    rec = np.asarray(rec, dtype=np.float64)
    orig = np.asarray(orig, dtype=np.float64)
    win = int(round(window_s * sr))
    pad = int(round(search_s * sr))
    span = 2 * pad + 1
    size = 1 << int(np.ceil(np.log2(win + span + win)))
    ts, devs, corrs = [], [], []
    for i0 in range(0, len(rec) - win + 1, win):
        r = rec[i0 : i0 + win]
        r_norm = float(np.sqrt(np.dot(r, r)))
        s0 = lag + i0 - pad
        if r_norm == 0.0 or s0 < 0 or s0 + win + span - 1 > len(orig):
            continue
        seg = orig[s0 : s0 + win + span - 1]
        xc = np.fft.irfft(np.fft.rfft(seg, size) * np.conj(np.fft.rfft(r, size)), size)[
            :span
        ]
        csum = np.concatenate(([0.0], np.cumsum(seg * seg)))
        energy = np.maximum(csum[win : win + span] - csum[:span], 1e-12)
        corr = xc / (r_norm * np.sqrt(energy))
        k = int(np.argmax(corr))
        if k == 0 or k == span - 1 or corr[k] < FIT_MIN_CORR:
            continue  # peak at the search edge (not found) or no real match
        ts.append(i0 + win / 2.0)
        devs.append(_refine_peak(xc, k) - pad)
        corrs.append(float(corr[k]))
    return np.array(ts), np.array(devs), np.array(corrs)


def fit_rate(rec: np.ndarray, orig: np.ndarray, lag: int, sr: int) -> dict:
    """Least-squares ``dev(i) = a + b·i`` over the measurable windows.

    Returns ``a`` (samples), ``b`` (dimensionless rate), ``rate_ppm``
    (``b·1e6``), ``fit_residual_ms`` (the max |residual| of a window, in ms)
    and ``windows`` (used). With fewer than ``FIT_MIN_WINDOWS`` windows the
    fit is ``None`` throughout.
    """
    t, dev, _ = local_lags(rec, orig, lag, sr)
    if len(t) < FIT_MIN_WINDOWS:
        return {
            "a": None,
            "b": None,
            "rate_ppm": None,
            "fit_residual_ms": None,
            "windows": len(t),
        }
    design = np.vstack([np.ones_like(t), t]).T
    (a, b), *_ = np.linalg.lstsq(design, dev, rcond=None)
    resid = dev - (a + b * t)
    return {
        "a": float(a),
        "b": float(b),
        "rate_ppm": float(b * 1e6),
        "fit_residual_ms": float(np.max(np.abs(resid)) * 1000.0 / sr),
        "windows": len(t),
    }


def warp(
    orig: np.ndarray, start: float, rate: float, n: int, half_taps: int = SINC_HALF_TAPS
) -> np.ndarray:
    """``orig`` resampled at positions ``start + (1 + rate)·i`` for ``i < n``.

    Band-limited: a Blackman-windowed sinc of ``2·half_taps`` taps. Positions
    outside ``orig`` read as silence.
    """
    orig = np.asarray(orig, dtype=np.float64)
    pos = start + (1.0 + rate) * np.arange(n, dtype=np.float64)
    base = np.floor(pos).astype(np.int64)
    frac = pos - base
    out = np.zeros(n)
    for k in range(-half_taps + 1, half_taps + 1):
        idx = base + k
        ok = (idx >= 0) & (idx < len(orig))
        x = frac - k  # distance from sample idx to the wanted position
        window = (
            0.42
            + 0.5 * np.cos(np.pi * x / half_taps)
            + 0.08 * np.cos(2 * np.pi * x / half_taps)
        )
        out += (
            np.where(ok, orig[np.clip(idx, 0, len(orig) - 1)], 0.0)
            * np.sinc(x)
            * window
        )
    return out


def compensate(rec: np.ndarray, orig: np.ndarray, lag: int, sr: int) -> dict:
    """Fit the offset + rate of ``rec`` against ``orig`` around the global
    ``lag`` and warp the original onto the recording when the fit allows it.

    Always returns ``aligned`` (``len(rec)`` samples of the original heard at
    each recording sample), ``warped``, ``rate_ppm``, ``fit_residual_ms`` and
    ``fit_windows``. When warped it also returns ``corr`` (the normalized
    correlation against the warped original) and ``mid_lag`` (the original
    position of the recording's midpoint minus the midpoint index, in samples).
    Unwarped, ``aligned`` is the plain slice at ``lag``, which is the old
    one-lag analysis.
    """
    rec = np.asarray(rec, dtype=np.float64)
    orig = np.asarray(orig, dtype=np.float64)
    n = len(rec)
    fit = fit_rate(rec, orig, lag, sr)
    out = {
        "warped": False,
        "rate_ppm": fit["rate_ppm"],
        "fit_residual_ms": fit["fit_residual_ms"],
        "fit_windows": fit["windows"],
        "aligned": orig[lag : lag + n],
    }
    if (
        fit["b"] is None
        or not abs(fit["rate_ppm"]) <= MAX_WARP_PPM
        or not fit["fit_residual_ms"] <= MAX_FIT_RESIDUAL_MS
    ):
        return out
    aligned = warp(orig, lag + fit["a"], fit["b"], n)
    denom = float(np.sqrt(np.dot(rec, rec) * np.dot(aligned, aligned)))
    out.update(
        warped=True,
        aligned=aligned,
        corr=float(np.dot(rec, aligned) / denom) if denom > 0.0 else 0.0,
        mid_lag=float(lag + fit["a"] + fit["b"] * n / 2.0),
    )
    return out
