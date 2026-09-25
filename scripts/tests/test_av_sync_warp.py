"""Tests for scripts/av_sync_warp.py (#147: the A/V gate compensates a smooth
clock-rate difference before its audio verdict).

Push run 36081072389 recorded the ORIGINAL with a perfectly smooth +16.8 ppm
rate difference between SongPlayer's genlocked output and the OBS audio clock.
The gate correlated the whole take at ONE lag, so the 0.33 ms walk decorrelated
the highs (corr 0.84, 89 "glitches") and read cannot_measure.

The synthetic original here is a sum of random tones with a slow amplitude
envelope, evaluated analytically at ANY time. A drifted recording is therefore
exact at every sample and independent of the warp under test. Cases from the
design record:

* a +16.8 / -50 ppm drift is fitted within 1 ppm and compensated;
* a 12 ms step is NOT absorbed (the fit residual blocks the warp);
* a 20 ms dropout is still a dropout after the warp;
* 500 ppm is outside the bound and is not warped;
* a drift-free take keeps its offset (so av_ms is unchanged).

Plus a measure-level test through the real ``measure()`` with the ffmpeg layer
replaced.
"""

from __future__ import annotations

import functools
import importlib.util
import json
from pathlib import Path

import numpy as np
import pytest

_SCRIPTS = Path(__file__).resolve().parents[1]


def _load(name: str):
    spec = importlib.util.spec_from_file_location(name, _SCRIPTS / f"{name}.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


avw = _load("av_sync_warp")
avs = _load("av_sync_check")

SR = avs.SR
ORIG_S = 30.0
REC_S = 20.0
START_S = 5.0  # the recording starts 5 s into the original


class _Tones:
    """Band-limited "noise-music": random tones (60-3400 Hz) under a slow
    per-tone amplitude envelope, evaluable at any time (seconds)."""

    def __init__(self, seed: int = 5, n: int = 150):
        rng = np.random.default_rng(seed)
        self.f = rng.uniform(60.0, 3400.0, n)
        self.ph = rng.uniform(0.0, 2 * np.pi, n)
        self.amp = rng.uniform(0.2, 1.0, n) / np.sqrt(n)
        self.am_f = rng.uniform(0.2, 3.0, n)
        self.am_ph = rng.uniform(0.0, 2 * np.pi, n)

    def __call__(self, t: np.ndarray) -> np.ndarray:
        out = np.zeros(len(t))
        for f, ph, a, af, aph in zip(self.f, self.ph, self.amp, self.am_f, self.am_ph):
            env = 0.6 + 0.4 * np.sin(2 * np.pi * af * t + aph)
            out += a * env * np.sin(2 * np.pi * f * t + ph)
        return out


@functools.lru_cache(maxsize=None)
def _signal() -> _Tones:
    return _Tones()


@functools.lru_cache(maxsize=None)
def _original() -> np.ndarray:
    return _signal()(np.arange(int(ORIG_S * SR)) / SR)


def _recording(ppm: float = 0.0, step_ms: float = 0.0, gap: tuple | None = None):
    """The original heard from ``START_S`` on, its clock running ``ppm`` fast
    (orig time = START_S + t·(1 + ppm·1e-6)), optionally jumping ``step_ms``
    forward at t = 10 s and zeroed over ``gap = (t0_s, ms)``."""
    t = np.arange(int(REC_S * SR)) / SR
    orig_t = START_S + t * (1.0 + ppm * 1e-6)
    orig_t = orig_t + np.where(t >= 10.0, step_ms / 1000.0, 0.0)
    rng = np.random.default_rng(11)
    rec = 0.7 * _signal()(orig_t) + rng.standard_normal(len(t)) * 0.002
    if gap is not None:
        i0 = int(gap[0] * SR)
        rec[i0 : i0 + int(gap[1] * SR / 1000)] = 0.0
    return rec


def _analyze(rec: np.ndarray, lag: int | None = None):
    """Global alignment (or a given ``lag``), the compensation, the dropouts."""
    orig = _original()
    aud = avs.audio_offset(rec, orig, SR)
    comp = avw.compensate(rec, orig, aud["lag"] if lag is None else lag, SR)
    drops = avs.dropout_blocks(rec, comp["aligned"], SR)
    return aud, comp, drops


@pytest.mark.parametrize("ppm", [16.8, -50.0])
def test_a_smooth_rate_difference_is_fitted_and_compensated(ppm):
    rec = _recording(ppm=ppm)
    aud, comp, drops = _analyze(rec)
    assert aud["corr"] < 0.98, "the fixed-lag correlation is decorrelated by the drift"
    assert comp["warped"] is True
    assert comp["rate_ppm"] == pytest.approx(ppm, abs=1.0)
    assert comp["fit_residual_ms"] <= avw.MAX_FIT_RESIDUAL_MS
    assert comp["corr"] >= 0.98
    assert drops["dropout_count"] == 0
    assert drops["glitch_blocks"] == 0


def test_a_12ms_step_is_not_absorbed():
    rec = _recording(step_ms=12.0)
    aud, comp, drops = _analyze(rec)
    assert comp["warped"] is False
    assert comp["fit_residual_ms"] > avw.MAX_FIT_RESIDUAL_MS
    # Unwarped = today's analysis: the plain slice at the global lag.
    orig = _original()
    np.testing.assert_array_equal(
        comp["aligned"], orig[aud["lag"] : aud["lag"] + len(rec)]
    )
    # ... so the step still reads cannot_measure, exactly as before.
    status, _, sides = avs.verdict(aud["corr"], 0.99, 0.01, 0.0, drops["dropout_count"])
    assert status == "cannot_measure" and "audio" in sides


def test_a_20ms_dropout_is_still_caught_after_the_warp():
    rec = _recording(ppm=16.8, gap=(9.3, 20.0))
    _, comp, drops = _analyze(rec)
    assert comp["warped"] is True
    assert drops["dropout_count"] == 1
    assert drops["dropout_events"][0]["start_s"] == pytest.approx(9.3, abs=0.012)


def test_500_ppm_is_outside_the_bound_and_not_warped():
    # Seeded with the true start lag: a 500 ppm take smears the one-lag global
    # correlation, and this case is about the rate bound, not the coarse search.
    lag = int(START_S * SR)
    rec = _recording(ppm=500.0)
    _, comp, _ = _analyze(rec, lag)
    assert comp["warped"] is False
    assert comp["rate_ppm"] == pytest.approx(500.0, abs=5.0), "still reported"
    orig = _original()
    np.testing.assert_array_equal(comp["aligned"], orig[lag : lag + len(rec)])


def test_a_drift_free_take_keeps_its_offset():
    rec = _recording(ppm=0.0)
    aud, comp, _ = _analyze(rec)
    assert comp["warped"] is True
    assert abs(comp["rate_ppm"]) < 1.0
    # The midpoint offset equals the true one and the global one, to 0.05 ms.
    assert comp["mid_lag"] == pytest.approx(START_S * SR, abs=0.4)
    assert comp["mid_lag"] == pytest.approx(aud["offset_s"] * SR, abs=0.4)


def test_warp_reproduces_a_band_limited_signal_at_fractional_positions():
    # Resample the analytic signal at a fractional start and a 100 ppm rate:
    # the windowed-sinc warp matches the exact values (not linear interp).
    orig = _original()
    n = 8000
    start, rate = 40_000.37, 100e-6
    got = avw.warp(orig, start, rate, n)
    want = _signal()((start + (1.0 + rate) * np.arange(n)) / SR)
    err = np.sqrt(np.mean((got - want) ** 2)) / np.sqrt(np.mean(want**2))
    assert err < 0.01, f"relative RMS error {err:.4f}"


def test_too_few_measurable_windows_are_not_warped():
    # A near-silent take has no window to fit.
    rec = _recording() * 0.0
    rec[: int(0.6 * SR)] = _recording()[: int(0.6 * SR)]
    comp = avw.compensate(rec, _original(), int(START_S * SR), SR)
    assert comp["warped"] is False
    assert comp["rate_ppm"] is None


# --- measure-level: the real measure() with the ffmpeg layer replaced ---------


def test_measure_compensates_the_drift_and_reports_it(monkeypatch):
    rec = _recording(ppm=16.8)
    orig = _original()

    def fake_decode_audio(_ffmpeg, path):
        return (rec, 0.0) if path == "rec.mkv" else (orig, 0.0)

    def video(_ffmpeg, _rec, _orig, center_s):
        # The picture is in sync with the audio at the recording's midpoint.
        return {
            "frames": 600,
            "_offset_exact": START_S + 10.0 * 16.8e-6,
            "_match_exact": 0.99,
            "_contrast_exact": 0.01,
        }

    monkeypatch.setattr(avs, "decode_audio", fake_decode_audio)
    monkeypatch.setattr(avs, "_measure_video", video)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4")
    json.dumps(r, allow_nan=False)
    assert r["status"] == "pass", r["reasons"]
    assert r["audio"]["warped"] is True
    assert r["audio"]["rate_ppm"] == pytest.approx(16.8, abs=1.0)
    assert r["audio"]["fit_residual_ms"] <= avw.MAX_FIT_RESIDUAL_MS
    assert r["audio"]["corr"] >= 0.98
    assert r["audio"]["corr_unwarped"] < r["audio"]["corr"]
    assert r["dropouts"]["glitch_blocks"] == 0
    assert abs(r["av_ms"]) < 0.5
    line = avs.summary_line(r)
    assert "rate_ppm=" in line and "warped=True" in line
    assert "fit_residual_ms=" in line
