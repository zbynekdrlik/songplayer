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
STEP_UNDO = 0.5  # a neighbour step undoes one when their sum is < 0.5 x it
MIN_COVERAGE = 0.7  # below this share of good windows, coverage_low is set


def good_window(s: dict, min_corr: float = SEGMENT_MIN_CORR) -> bool:
    """A window the fit trusts: its audio matched the original (corr >=
    ``min_corr``) and its picture was measurable (``video_ok``: motion, and a
    plateau no wider than ~1.5 frames of the coarser frame clock)."""
    return (
        s["av_ms"] is not None
        and s["audio_corr"] is not None
        and s["audio_corr"] >= min_corr
        and s["video_ok"] is True
    )


def _lasting_outliers(av: np.ndarray) -> list[int]:
    """Indices ``j`` of the neighbour steps ``av[j+1] - av[j]`` that are
    outliers (>= ``STEP_MIN_MS`` and >= ``STEP_OUTLIER_X`` x the median step)
    and are NOT undone by an adjacent step: the two edges of a one-window
    spike cancel (their sum is < ``STEP_UNDO`` x the step)."""
    sd = np.diff(av)
    floor = max(STEP_MIN_MS, STEP_OUTLIER_X * float(np.median(np.abs(sd))))
    out = []
    for j, d in enumerate(sd):
        if abs(d) < floor:
            continue
        near = [n for n in (j - 1, j + 1) if 0 <= n < len(sd)]
        if not any(abs(d + sd[n]) < STEP_UNDO * abs(d) for n in near):
            out.append(j)
    return out


def _step_holds(
    t: np.ndarray, av: np.ndarray, k: int, lasting: list[int]
) -> tuple[bool, np.ndarray | None]:
    """Whether the step between good windows ``k`` and ``k + 1`` is ONE
    lasting jump, and the ``[level, slope, step]`` fit
    ``av = level + slope * t + step * [t >= t_k+1]`` if so.

    It must be the ONLY lasting outlier step (``_lasting_outliers``), with
    >= 2 good windows on each side.
    * A steady drift has equal steps, so it has no outlier.
    * A one-window spike undoes itself, so it has no lasting outlier.
    * Two resyncs (opposite, same direction or periodic) have two. A
      one-step model would misread them as a steep drift.
    The slope is fitted jointly with the step, so a drift followed by one
    resync against it still reads as its own drift.
    """
    n_before, n_after = k + 1, len(av) - (k + 1)
    if lasting != [k] or n_before < 2 or n_after < 2:
        return False, None
    after = (t >= t[k + 1]).astype(np.float64)
    cols = np.stack([np.ones_like(t), t, after], axis=1)
    return True, np.linalg.lstsq(cols, av, rcond=None)[0]


def drift_and_step(profile: list[dict], min_corr: float = SEGMENT_MIN_CORR) -> dict:
    """Drift slope and the largest step of ``av_ms`` over the good windows.

    Each row needs ``t_s`` / ``t_end_s`` (window start / end), ``av_ms``,
    ``audio_corr`` and ``video_ok``.
    * ``max_step_ms``: the largest |av_ms change| between neighbouring good
      windows. ``step_gap_s`` is ``[end of the earlier, start of the later]``
      window, and ``step_at_s`` its middle. A gap wider than one window means
      windows between them were excluded, so the jump is somewhere inside it.
    * ``drift_ms_per_10s``: a least-squares slope of av_ms over time. The fit
      gets its own step term at the largest step (``step_modeled``, see
      ``_step_holds``) when it is the only lasting outlier step and its
      level HOLDS once the drift is removed. A jump then reads as a step
      with a flat slope, and a drift followed by ONE resync against it (a
      one-cycle sawtooth) as the drift's own slope plus the step. A steady
      drift (equal steps) or a one-window spike is never split. A jump in an
      end window cannot be modelled: it shows as ``max_step_ms`` and bends
      the slope.
    * ``outlier_steps``: how many lasting outlier steps there are. With two
      or more (several resyncs), no step is modelled and the slope is NOT a
      clock drift: read the per-window ``av_ms``.
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
        "outlier_steps": 0,
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
    lasting = _lasting_outliers(av)
    modeled, coef = _step_holds(t, av, k, lasting)
    if not modeled:
        cols = np.stack([np.ones_like(t), t], axis=1)
        coef = np.linalg.lstsq(cols, av, rcond=None)[0]
    gap = [round(float(t_end[k]), 3), round(float(t[k + 1]), 3)]
    out.update(
        drift_ms_per_10s=round(float(coef[1]) * 10.0, 1),
        max_step_ms=round(float(steps[k]), 1),
        step_at_s=round((gap[0] + gap[1]) / 2.0, 3),
        step_gap_s=gap,
        step_modeled=bool(modeled),
        outlier_steps=len(lasting),
    )
    return out
