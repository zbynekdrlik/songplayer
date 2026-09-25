"""Drift / step fit of the post-deploy A/V gate's per-window profile (#147).

``av_sync_check.segment_profile`` measures the A/V offset (``av_ms``) of every
2 s window of the OBS recording. ``drift_and_step`` turns that list into the
two numbers that tell a steady clock DRIFT from a sudden JUMP (a resync):

* ``drift_ms_per_10s``: the least-squares slope of av_ms over time;
* ``max_step_ms`` / ``step_at_s`` / ``step_gap_s``: the largest av_ms change
  between neighbouring good windows, and where it happens.

Diagnostics only: the gate's verdict never reads them. Pure numpy, no I/O.
Covered by ``scripts/tests/test_av_sync_profile.py``.
"""

from __future__ import annotations

import numpy as np

SEGMENT_MIN_CORR = 0.8  # a window below this audio corr is left out of the fit
STEP_MIN_MS = 20.0  # a step term is fitted only for a step at least this big
STEP_OUTLIER_X = 3.0  # ... and at least this many times the median step
STEP_PERSIST = 0.5  # ... whose level holds: side medians differ >= 0.5 x step
MIN_COVERAGE = 0.7  # below this share of good windows, coverage_low is set


def good_window(s: dict, min_corr: float = SEGMENT_MIN_CORR) -> bool:
    """A window the fit trusts: its audio matched the original (corr >=
    ``min_corr``) and its picture was measurable (``video_ok``: motion, and a
    plateau no wider than about one source frame)."""
    return (
        s["av_ms"] is not None
        and s["audio_corr"] is not None
        and s["audio_corr"] >= min_corr
        and s["video_ok"] is True
    )


def drift_and_step(profile: list[dict], min_corr: float = SEGMENT_MIN_CORR) -> dict:
    """Drift slope and the largest step of ``av_ms`` over the good windows.

    Each row needs ``t_s`` / ``t_end_s`` (window start / end), ``av_ms``,
    ``audio_corr`` and ``video_ok``.
    * ``max_step_ms``: the largest |av_ms change| between neighbouring good
      windows. ``step_gap_s`` is ``[end of the earlier, start of the later]``
      window, and ``step_at_s`` its middle. A gap wider than one window means
      windows between them were excluded, so the jump is somewhere inside it.
    * ``drift_ms_per_10s``: a least-squares slope of av_ms over time. The fit
      gets its own step term at the largest step (``step_modeled``) when the
      step is an outlier (>= ``STEP_MIN_MS`` and >= ``STEP_OUTLIER_X`` x the
      median step), each side keeps >= 2 good windows, and the level HOLDS:
      the medians of the two sides differ by >= ``STEP_PERSIST`` x the step.
      A jump then reads as a step with a flat slope. A steady drift (equal
      steps), a one-window spike or a sawtooth (equal side medians) is never
      split. A jump in an end window cannot be modelled: it shows as
      ``max_step_ms`` and bends the slope.
    * ``coverage_low``: fewer than ``MIN_COVERAGE`` of the windows are good.
      A fast drift smears its windows below ``min_corr``, so then read the
      excluded rows' raw ``av_ms`` too.
    Fields are None with fewer than two good windows.
    """
    good = [s for s in profile if good_window(s, min_corr)]
    out = {
        "drift_ms_per_10s": None,
        "max_step_ms": None,
        "step_at_s": None,
        "step_gap_s": None,
        "step_modeled": False,
        "windows_used": len(good),
        "windows_total": len(profile),
        "coverage_low": len(good) < MIN_COVERAGE * len(profile),
        "min_corr": min_corr,
    }
    if len(good) < 2:
        return out
    t = np.array([s["t_s"] for s in good], dtype=np.float64)
    t_end = np.array([s["t_end_s"] for s in good], dtype=np.float64)
    av = np.array([s["av_ms"] for s in good], dtype=np.float64)
    steps = np.abs(np.diff(av))
    k = int(np.argmax(steps))
    before, after = av[: k + 1], av[k + 1 :]
    floor = max(STEP_MIN_MS, STEP_OUTLIER_X * float(np.median(steps)))
    modeled = (
        steps[k] >= floor
        and len(before) >= 2
        and len(after) >= 2
        and abs(float(np.median(after) - np.median(before))) >= STEP_PERSIST * steps[k]
    )
    cols = [np.ones_like(t), t]
    if modeled:
        cols.append((t >= t[k + 1]).astype(np.float64))
    coef = np.linalg.lstsq(np.stack(cols, axis=1), av, rcond=None)[0]
    gap = [round(float(t_end[k]), 3), round(float(t[k + 1]), 3)]
    out.update(
        drift_ms_per_10s=round(float(coef[1]) * 10.0, 1),
        max_step_ms=round(float(steps[k]), 1),
        step_at_s=round((gap[0] + gap[1]) / 2.0, 3),
        step_gap_s=gap,
        step_modeled=bool(modeled),
    )
    return out
