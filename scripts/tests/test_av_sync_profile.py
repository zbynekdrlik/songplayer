"""Tests for the per-segment A/V profile of scripts/av_sync_check.py (#147).

A single whole-recording A/V number cannot tell a steady clock DRIFT from a
sudden JUMP (a resync). The release run 36068121677 read av_ms 60.5 with
glitches only from 12.5 s. ``segment_profile`` measures the audio and video
offsets per 2 s window, and ``drift_and_step`` turns that into a slope
(ms per 10 s) plus the largest neighbour step. These tests pin that the two
cases read differently.

Synthetic fixtures with numpy only: an irregular click train over a noise bed
(the ORIGINAL audio, 8 kHz) and a picture that cuts to a new random texture at
every click (the ORIGINAL video, 25 fps). The recording (30 fps picture,
sample 0 of the audio at -21 ms) shows the original picture from ``START``,
while its audio is taken from ``START + shift(t)``, so ``shift`` is exactly the
A/V offset at recording time ``t``.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import numpy as np
import pytest

_SPEC = importlib.util.spec_from_file_location(
    "av_sync_check", Path(__file__).resolve().parents[1] / "av_sync_check.py"
)
avs = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(avs)

SR = avs.SR
ORIG_FPS = 25.0
REC_FPS = 30.0
ORIG_S = 60.0
REC_S = 20.0
REC_T0 = -0.021
START = 21.4  # original time shown at recording time 0


def _original(seed: int = 5):
    rng = np.random.default_rng(seed)
    n = int(ORIG_S * SR)
    block = SR // 10
    env = np.repeat(rng.uniform(0.1, 1.0, n // block + 1), block)[:n]
    audio = rng.standard_normal(n) * env * 0.1
    clicks, t = [], 0.3
    while t < ORIG_S - 0.3:
        clicks.append(t)
        t += rng.uniform(0.25, 0.9)
    for c in clicks:
        i = int(c * SR)
        audio[i : i + 40] += 0.9
    pts = np.arange(int(ORIG_S * ORIG_FPS)) / ORIG_FPS
    scene = np.searchsorted(np.array(clicks), pts, side="right")
    textures = rng.uniform(0, 255, size=(scene.max() + 1, 32, 64))
    return audio, textures[scene], pts


def _recording(audio, frames, pts, shift, seed=11):
    """Recording whose picture shows orig ``START + t`` at rec time ``t`` and
    whose audio plays orig ``START + t + shift(t)``."""
    rng = np.random.default_rng(seed)
    n = int(REC_S * SR)
    t = REC_T0 + np.arange(n) / SR
    src = np.round((t + START + shift(t)) * SR).astype(int)
    rec_audio = 0.7 * audio[src] + rng.standard_normal(n) * 0.002
    rec_pts = np.arange(int(REC_S * REC_FPS)) / REC_FPS
    j = np.searchsorted(pts, rec_pts + START, side="right") - 1
    rec_frames = frames[j] + rng.normal(0, 2.0, size=frames[j].shape)
    return rec_audio, rec_frames, rec_pts


def _profile(audio, frames, pts, rec_audio, rec_frames, rec_pts):
    """The same global steps measure() runs, then the per-segment profile."""
    aud = avs.audio_offset(rec_audio, audio, SR, rec_t0=REC_T0, orig_t0=0.0)
    vid = avs.video_offset(rec_frames, rec_pts, frames, pts, center_s=aud["offset_s"])
    return avs.segment_profile(
        rec_audio,
        audio,
        aud["lag"],
        sr=SR,
        rec_t0=REC_T0,
        orig_t0=0.0,
        video=(rec_frames, rec_pts, frames, pts),
        video_offset_s=vid["offset_s"],
    )


def _const(value):
    return lambda t: np.full_like(t, value)


# --- the acceptance cases from the design record ------------------------------


def test_steady_drift_reads_as_a_slope_and_no_step():
    audio, frames, pts = _original()
    # The audio drifts 50 ms ahead of the picture over the 20 s take.
    prof = _profile(
        audio, frames, pts, *_recording(audio, frames, pts, lambda t: 0.05 * t / REC_S)
    )
    assert len(prof) == 10
    assert [s["t_s"] for s in prof] == pytest.approx(
        REC_T0 + 2.0 * np.arange(10), abs=1e-3
    )
    assert all(s["audio_corr"] >= 0.9 for s in prof), prof
    assert all(s["video_match"] >= avs.MIN_VIDEO_MATCH for s in prof), prof
    # Each window reads the drift at its own centre: +2.5 ms per second.
    for s in prof:
        assert s["av_ms"] == pytest.approx(2.5 * (s["t_s"] + 1.0), abs=6.0), s
    d = avs.drift_and_step(prof)
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=5.0), d
    assert d["max_step_ms"] < 15.0, d
    assert d["step_modeled"] is False
    assert d["windows_used"] == 10


def test_60ms_jump_at_12s_reads_as_a_step_and_no_slope():
    audio, frames, pts = _original()
    rec = _recording(audio, frames, pts, lambda t: np.where(t >= 12.0, 0.06, 0.0))
    prof = _profile(audio, frames, pts, *rec)
    d = avs.drift_and_step(prof)
    assert d["max_step_ms"] == pytest.approx(60.0, abs=10.0), d
    assert d["step_at_s"] == pytest.approx(12.0, abs=0.1), d
    assert d["drift_ms_per_10s"] == pytest.approx(0.0, abs=5.0), d
    assert d["step_modeled"] is True
    before = [s["av_ms"] for s in prof if s["t_s"] < 11.0]
    after = [s["av_ms"] for s in prof if s["t_s"] > 11.0]
    assert np.median(after) - np.median(before) == pytest.approx(60.0, abs=10.0), prof


def test_in_sync_take_has_neither_drift_nor_step():
    audio, frames, pts = _original()
    prof = _profile(audio, frames, pts, *_recording(audio, frames, pts, _const(0.0)))
    d = avs.drift_and_step(prof)
    assert d["drift_ms_per_10s"] == pytest.approx(0.0, abs=5.0), d
    assert d["max_step_ms"] < 10.0, d
    assert all(abs(s["av_ms"]) <= 10.0 for s in prof), prof


def test_a_window_with_low_correlation_is_excluded_from_the_fit():
    audio, frames, pts = _original()
    rec_audio, rec_frames, rec_pts = _recording(
        audio, frames, pts, lambda t: 0.05 * t / REC_S
    )
    # Window 3 (6-8 s) carries unrelated audio: its local offset is noise.
    i0, i1 = 3 * 2 * SR, 4 * 2 * SR
    rng = np.random.default_rng(99)
    rec_audio = rec_audio.copy()
    rec_audio[i0:i1] = rng.standard_normal(i1 - i0) * np.std(rec_audio[i0:i1])
    prof = _profile(audio, frames, pts, rec_audio, rec_frames, rec_pts)
    assert prof[3]["audio_corr"] < avs.SEGMENT_MIN_CORR, prof[3]
    assert all(s["audio_corr"] >= 0.9 for i, s in enumerate(prof) if i != 3)
    d = avs.drift_and_step(prof)
    assert d["windows_used"] == 9
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=5.0), d
    assert d["max_step_ms"] < 15.0, d


# --- drift_and_step on hand-built profiles --------------------------------------


def _seg(t, av, corr=0.95):
    return {
        "t_s": t,
        "audio_offset_s": 0.0,
        "audio_corr": corr,
        "video_offset_s": 0.0,
        "video_match": 0.99,
        "av_ms": av,
    }


def test_drift_fit_ignores_low_corr_and_unmeasured_windows():
    prof = [_seg(t, 2.5 * t) for t in range(0, 20, 2)]
    prof[4] = _seg(8.0, 400.0, corr=0.5)  # a wrong local match
    prof[6] = _seg(12.0, None)  # the picture was unmeasurable here
    d = avs.drift_and_step(prof)
    assert d["windows_used"] == 8
    assert d["windows_total"] == 10
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=0.01)
    # The neighbours of the excluded windows are 4 s apart: a 10 ms step.
    assert d["max_step_ms"] == pytest.approx(10.0, abs=0.01)
    assert d["step_modeled"] is False


def test_step_term_separates_a_jump_from_an_underlying_drift():
    prof = [_seg(t, 1.0 * t + (60.0 if t >= 12 else 0.0)) for t in range(0, 20, 2)]
    d = avs.drift_and_step(prof)
    assert d["drift_ms_per_10s"] == pytest.approx(10.0, abs=0.01)
    assert d["max_step_ms"] == pytest.approx(62.0, abs=0.01)
    assert d["step_at_s"] == 12
    assert d["step_modeled"] is True


def test_drift_needs_two_good_windows():
    d = avs.drift_and_step([_seg(0.0, 5.0), _seg(2.0, 9.0, corr=0.1)])
    assert d["windows_used"] == 1
    assert d["drift_ms_per_10s"] is None
    assert d["max_step_ms"] is None
    assert d["step_at_s"] is None


def test_summary_line_carries_the_drift_fields():
    result = {
        "status": "fail",
        "reasons": ["|A/V| 60.5 ms > 40 ms"],
        "av_ms": 60.5,
        "drift": {"drift_ms_per_10s": 25.0, "max_step_ms": 5.1, "step_at_s": 11.979},
    }
    line = avs.summary_line(result)
    assert "drift_ms_per_10s=25.0 max_step_ms=5.1 step_at_s=11.979" in line
    # The analysis-error result has no profile: the fields read None.
    err = avs.summary_line({"status": "cannot_measure", "reasons": ["x"]})
    assert "drift_ms_per_10s=None max_step_ms=None step_at_s=None" in err


# --- measure() carries the profile into the JSON --------------------------------


def _patch(monkeypatch, rec_audio, audio, video):
    def fake_decode_audio(_ffmpeg, path):
        return (rec_audio, REC_T0) if path == "rec.mkv" else (audio, 0.0)

    monkeypatch.setattr(avs, "decode_audio", fake_decode_audio)
    monkeypatch.setattr(avs, "_measure_video", video)


def test_measure_reports_segments_and_drift(monkeypatch):
    audio, frames, pts = _original()
    rec_audio, rec_frames, rec_pts = _recording(
        audio, frames, pts, lambda t: np.where(t >= 12.0, 0.06, 0.0)
    )

    def video(_ffmpeg, _rec, _orig, center_s):
        vid = avs.video_offset(rec_frames, rec_pts, frames, pts, center_s=center_s)
        return {
            "frames": len(rec_pts),
            "_offset_exact": vid["offset_s"],
            "_match_exact": vid["match"],
            "_contrast_exact": vid["contrast"],
            "_frames": (rec_frames, rec_pts, frames, pts),
        }

    _patch(monkeypatch, rec_audio, audio, video)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4")
    json.dumps(r, allow_nan=False)
    assert r["video"] == {
        "frames": len(rec_pts)
    }  # the frame arrays are not in the JSON
    assert len(r["segments"]) == 10
    assert r["drift"]["max_step_ms"] == pytest.approx(60.0, abs=10.0)
    assert r["drift"]["step_at_s"] == pytest.approx(12.0, abs=0.1)
    # Diagnostics only: the verdict is still the whole-take one.
    assert r["status"] == "fail"
    assert any("A/V" in reason for reason in r["reasons"])


def test_measure_profiles_the_audio_even_without_a_picture(monkeypatch):
    audio, frames, pts = _original()
    rec_audio, _, _ = _recording(audio, frames, pts, _const(0.0))

    def broken(*_args):
        raise RuntimeError("decoded 0 frames")

    _patch(monkeypatch, rec_audio, audio, broken)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4")
    json.dumps(r, allow_nan=False)
    assert len(r["segments"]) == 10
    assert all(s["audio_corr"] >= 0.9 for s in r["segments"])
    assert all(
        s["video_offset_s"] is None and s["av_ms"] is None for s in r["segments"]
    )
    assert r["drift"]["windows_used"] == 0
    assert r["unmeasurable_sides"] == ["video_error"]


def test_a_profile_error_never_changes_the_verdict(monkeypatch):
    audio, frames, pts = _original()
    rec_audio, _, _ = _recording(audio, frames, pts, _const(0.0))

    def video(_ffmpeg, _rec, _orig, center_s):
        return {
            "frames": 600,
            "_offset_exact": center_s,
            "_match_exact": 0.99,
            "_contrast_exact": 0.01,
        }

    def boom(*_args, **_kwargs):
        raise RuntimeError("profile bug")

    _patch(monkeypatch, rec_audio, audio, video)
    monkeypatch.setattr(avs, "segment_profile", boom)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4")
    json.dumps(r, allow_nan=False)
    assert r["status"] == "pass"
    assert r["segments"] == []
    assert r["drift"] == {"error": "profile error: RuntimeError: profile bug"}
