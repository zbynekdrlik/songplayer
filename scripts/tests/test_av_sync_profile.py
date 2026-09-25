"""Tests for the per-segment A/V profile of scripts/av_sync_check.py (#147).

A single whole-recording A/V number cannot tell a steady clock DRIFT from a
sudden JUMP (a resync). The release run 36068121677 read av_ms 60.5 with
glitches only from 12.5 s. ``segment_profile`` measures the audio and video
offsets per 2 s window, and ``drift_and_step`` turns that into a slope
(ms per 10 s) plus the largest neighbour step. These tests pin that the two
cases read differently.

Synthetic fixtures with numpy only. The ORIGINAL audio (8 kHz) is band-limited
noise with a level envelope, the ORIGINAL picture (25 fps) moves on every frame.
The recording (30 fps picture, sample 0 of the audio at -21 ms) shows the
original picture from ``START``, while its audio is taken from
``START + shift(t)``, so ``shift`` is exactly the A/V offset at recording time
``t``.

Why a bass-band (<= 60 Hz) audio by default: the acceptance drift is 50 ms over
20 s (2500 ppm). That smears the alignment by +-2.5 ms inside ONE 2 s window.
Broadband content decorrelates under such a smear, and its windows drop below
``SEGMENT_MIN_CORR`` (``test_a_broadband_drift_this_fast_is_not_fitted`` pins
that). A clock error of a few hundred ppm smears +-0.1-0.3 ms. Music-band
audio (energy up to ~1 kHz) survives that
(``test_a_realistic_clock_drift_on_music_band_audio``), but bright material
near 4 kHz decorrelates from ~100 ppm. obs-ndi-health.md has the measured table.
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
# conftest.py puts scripts/ on sys.path, exactly as on the box.
import av_sync_drift as drift  # noqa: E402

SR = avs.SR
ORIG_FPS = 25.0
REC_FPS = 30.0
ORIG_S = 60.0
REC_S = 20.0
REC_T0 = -0.021
START = 21.4  # original time shown at recording time 0


def _original(seed: int = 5, band_hz: float | None = 60.0):
    """``band_hz`` None = broadband noise, else noise low-passed to ``band_hz``."""
    rng = np.random.default_rng(seed)
    n = int(ORIG_S * SR)
    audio = rng.standard_normal(n)
    if band_hz is not None:
        spec = np.fft.rfft(audio)
        spec[np.fft.rfftfreq(n, 1.0 / SR) > band_hz] = 0.0
        audio = np.fft.irfft(spec, n)
    block = SR // 10
    env = np.repeat(rng.uniform(0.2, 1.0, n // block + 1), block)[:n]
    audio = audio / audio.std() * 0.1 * env
    pts = np.arange(int(ORIG_S * ORIG_FPS)) / ORIG_FPS
    frames = rng.uniform(0, 255, size=(len(pts), 32, 64))
    return audio, frames, pts


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
    d = drift.drift_and_step(prof)
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=5.0), d
    assert d["max_step_ms"] < 15.0, d
    assert d["step_modeled"] is False
    assert d["windows_used"] == 10


def test_60ms_jump_at_12s_reads_as_a_step_and_no_slope():
    audio, frames, pts = _original()
    rec = _recording(audio, frames, pts, lambda t: np.where(t >= 12.0, 0.06, 0.0))
    prof = _profile(audio, frames, pts, *rec)
    d = drift.drift_and_step(prof)
    assert d["max_step_ms"] == pytest.approx(60.0, abs=10.0), d
    assert d["step_at_s"] == pytest.approx(12.0, abs=0.1), d
    assert d["drift_ms_per_10s"] == pytest.approx(0.0, abs=5.0), d
    assert d["step_modeled"] is True
    before = [s["av_ms"] for s in prof if s["t_s"] < 11.0]
    after = [s["av_ms"] for s in prof if s["t_s"] > 11.0]
    assert np.median(after) - np.median(before) == pytest.approx(60.0, abs=10.0), prof


def test_a_realistic_clock_drift_on_music_band_audio():
    # 100 ppm (1 ms per 10 s) on audio with energy up to 1 kHz, like music
    # at 8 kHz: every window still matches, and the slope is read.
    audio, frames, pts = _original(band_hz=1000.0)
    prof = _profile(
        audio, frames, pts, *_recording(audio, frames, pts, lambda t: 1e-4 * t)
    )
    assert all(s["audio_corr"] >= 0.9 for s in prof), prof
    d = drift.drift_and_step(prof)
    assert d["windows_used"] == 10
    assert d["drift_ms_per_10s"] == pytest.approx(1.0, abs=0.3), d
    assert d["step_modeled"] is False


def test_a_broadband_drift_this_fast_is_not_fitted():
    # The documented limit: 2500 ppm on broadband audio smears every 2 s window
    # below the correlation floor. The profile must then report no drift at all
    # rather than fit a slope through windows that do not match.
    audio, frames, pts = _original(band_hz=None)
    prof = _profile(
        audio, frames, pts, *_recording(audio, frames, pts, lambda t: 0.05 * t / REC_S)
    )
    assert all(s["audio_corr"] < drift.SEGMENT_MIN_CORR for s in prof), prof
    d = drift.drift_and_step(prof)
    assert d["windows_used"] == 0
    assert d["coverage_low"] is True
    assert d["drift_ms_per_10s"] is None


def test_in_sync_take_has_neither_drift_nor_step():
    audio, frames, pts = _original()
    prof = _profile(audio, frames, pts, *_recording(audio, frames, pts, _const(0.0)))
    d = drift.drift_and_step(prof)
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
    assert prof[3]["audio_corr"] < drift.SEGMENT_MIN_CORR, prof[3]
    assert all(s["audio_corr"] >= 0.9 for i, s in enumerate(prof) if i != 3)
    d = drift.drift_and_step(prof)
    assert d["windows_used"] == 9
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=5.0), d
    assert d["max_step_ms"] < 15.0, d


# --- drift_and_step on hand-built profiles --------------------------------------


def _seg(t, av, corr=0.95, video_ok=True):
    return {
        "t_s": t,
        "t_end_s": t + 2.0,
        "audio_offset_s": 0.0,
        "audio_corr": corr,
        "video_offset_s": 0.0,
        "video_match": 0.99,
        "video_ok": video_ok,
        "av_ms": av,
    }


def _profile_of(avs_ms, t0=0.0):
    return [_seg(t0 + 2.0 * i, av) for i, av in enumerate(avs_ms)]


def test_drift_fit_ignores_low_corr_and_unmeasured_windows():
    prof = [_seg(t, 2.5 * t) for t in range(0, 20, 2)]
    prof[4] = _seg(8.0, 400.0, corr=0.5)  # a wrong local match
    prof[6] = _seg(12.0, None)  # the picture was unmeasurable here
    d = drift.drift_and_step(prof)
    assert d["windows_used"] == 8
    assert d["windows_total"] == 10
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=0.01)
    # The neighbours of the excluded windows are 4 s apart: a 10 ms step.
    assert d["max_step_ms"] == pytest.approx(10.0, abs=0.01)
    assert d["step_modeled"] is False


def test_step_term_separates_a_jump_from_an_underlying_drift():
    prof = [_seg(t, 1.0 * t + (60.0 if t >= 12 else 0.0)) for t in range(0, 20, 2)]
    d = drift.drift_and_step(prof)
    assert d["drift_ms_per_10s"] == pytest.approx(10.0, abs=0.01)
    assert d["max_step_ms"] == pytest.approx(62.0, abs=0.01)
    assert d["step_at_s"] == 12
    assert d["step_modeled"] is True


def test_a_fast_steady_drift_is_never_split_into_a_step():
    # 25 ms per 2 s window: every step is >= STEP_MIN_MS, none is an outlier.
    d = drift.drift_and_step(_profile_of([12.5 * i * 2 for i in range(10)]))
    assert d["step_modeled"] is False
    assert d["drift_ms_per_10s"] == pytest.approx(125.0, abs=0.01)
    assert d["max_step_ms"] == pytest.approx(25.0, abs=0.01)


def test_a_step_must_stand_out_from_the_median_step():
    # A jittery 15 ms/window drift with one 22 ms step: >= STEP_MIN_MS but not
    # >= 3x the median step, so it is drift, not a jump.
    av = np.cumsum([0, 15, 14, 16, 15, 22, 15, 14, 16, 15]).tolist()
    d = drift.drift_and_step(_profile_of(av))
    assert d["max_step_ms"] == pytest.approx(22.0, abs=0.01)
    assert d["step_modeled"] is False


def test_small_noise_steps_are_never_modelled():
    # +-1 ms jitter with one 5 ms step: an outlier vs the median, but below
    # STEP_MIN_MS.
    av = [0.0, 1.0, 0.0, -1.0, 0.0, 5.0, 4.0, 5.0, 6.0, 5.0]
    d = drift.drift_and_step(_profile_of(av))
    assert d["step_modeled"] is False
    assert d["max_step_ms"] == pytest.approx(5.0, abs=0.01)


def test_a_one_window_spike_is_not_a_jump():
    # Up then straight back down: two outlier steps, so no step term.
    av = [0.0] * 10
    av[5] = 60.0
    d = drift.drift_and_step(_profile_of(av))
    assert d["step_modeled"] is False
    assert d["max_step_ms"] == pytest.approx(60.0, abs=0.01)


def test_a_jump_needs_two_good_windows_on_each_side():
    av = [0.0] * 9 + [60.0]  # the jump is on the last window
    d = drift.drift_and_step(_profile_of(av))
    assert d["step_modeled"] is False
    av = [0.0] * 8 + [60.0] * 2
    assert drift.drift_and_step(_profile_of(av))["step_modeled"] is True
    av = [0.0] + [60.0] * 9  # the jump is on the first window
    assert drift.drift_and_step(_profile_of(av))["step_modeled"] is False
    av = [0.0] * 2 + [60.0] * 8
    assert drift.drift_and_step(_profile_of(av))["step_modeled"] is True


def test_step_at_is_the_middle_of_the_gap_between_good_windows():
    # The 12-14 s window straddles a 13.0 s jump and is excluded (low corr):
    # the step sits between the end of 10-12 and the start of 14-16.
    prof = _profile_of([0.0] * 6 + [30.0] + [60.0] * 3)
    prof[6]["audio_corr"] = 0.4
    d = drift.drift_and_step(prof)
    assert d["step_at_s"] == pytest.approx(13.0)
    assert d["step_gap_s"] == [12.0, 14.0]
    assert d["max_step_ms"] == pytest.approx(60.0)
    assert d["step_modeled"] is True
    assert d["drift_ms_per_10s"] == pytest.approx(0.0, abs=0.01)


def test_a_window_whose_picture_is_not_ok_is_excluded():
    prof = _profile_of([2.5 * 2 * i for i in range(10)])
    prof[4] = _seg(8.0, 900.0, video_ok=False)
    d = drift.drift_and_step(prof)
    assert d["windows_used"] == 9
    assert d["drift_ms_per_10s"] == pytest.approx(25.0, abs=0.01)
    assert d["max_step_ms"] == pytest.approx(10.0, abs=0.01)


def _with_still(frames, pts, t0, t1):
    """The original picture frozen on one frame over orig times [t0, t1)."""
    frames = frames.copy()
    held = (pts >= t0) & (pts < t1)
    frames[held] = frames[np.flatnonzero(held)[0]]
    return frames


@pytest.mark.parametrize(
    "still",
    [
        (7.5, 10.5),  # longer than window + search: the curve is flat
        (7.9, 10.1),  # barely covers the window: edges drop, plateau is wide
    ],
)
def test_a_still_shot_window_is_not_video_ok(still):
    audio, frames, pts = _original()
    frames = _with_still(frames, pts, START + still[0], START + still[1])
    prof = _profile(audio, frames, pts, *_recording(audio, frames, pts, _const(0.0)))
    # Window 4 (rec 7.979-9.979 s) sees only the still.
    assert prof[4]["video_ok"] is False, prof[4]
    assert all(s["video_ok"] for i, s in enumerate(prof) if i not in (3, 4, 5)), prof
    d = drift.drift_and_step(prof)
    assert d["max_step_ms"] < 10.0, d
    assert d["drift_ms_per_10s"] == pytest.approx(0.0, abs=5.0), d


def test_a_nearly_static_window_is_not_video_ok():
    # Almost no motion (a slow fade): the curve has a unique but tiny peak.
    # The plateau is narrow, yet the contrast is below MIN_VIDEO_CONTRAST.
    audio, frames, pts = _original()
    frames = frames.copy()
    held = (pts >= START + 7.5) & (pts < START + 10.5)
    base = frames[np.flatnonzero(held)[0]]
    rng = np.random.default_rng(7)
    frames[held] = base + rng.normal(0, 1.0, size=frames[held].shape)
    prof = _profile(audio, frames, pts, *_recording(audio, frames, pts, _const(0.0)))
    w = prof[4]
    assert w["video_contrast"] < avs.MIN_VIDEO_CONTRAST, w
    assert w["video_ok"] is False, w


def test_coverage_low_flags_a_mostly_excluded_take():
    def used(n):
        prof = _profile_of([0.0] * 10)
        for s in prof[n:]:
            s["audio_corr"] = 0.5  # e.g. a drift fast enough to smear them
        return drift.drift_and_step(prof)

    assert used(6)["coverage_low"] is True
    assert used(7)["coverage_low"] is False  # 70 % is enough
    assert used(10)["coverage_low"] is False


def test_a_jump_next_to_one_bad_window_is_still_a_jump():
    # A still/fade window that slipped through reads 25 ms off: two steps
    # >= 20 ms, but only the 60 ms one changes the level for good.
    av = [0.0] * 5 + [60.0] * 5
    av[1] = 25.0
    d = drift.drift_and_step(_profile_of(av))
    assert d["step_modeled"] is True
    assert d["max_step_ms"] == pytest.approx(60.0)
    assert abs(d["drift_ms_per_10s"]) < 10.0, d


def test_a_sawtooth_is_not_modelled_as_one_jump():
    av = [0.0, 10.0, 20.0, 30.0, 0.0, 10.0, 20.0, 30.0, 0.0, 10.0]
    assert drift.drift_and_step(_profile_of(av))["step_modeled"] is False


def test_drift_needs_two_good_windows():
    d = drift.drift_and_step([_seg(0.0, 5.0), _seg(2.0, 9.0, corr=0.1)])
    assert d["windows_used"] == 1
    assert d["drift_ms_per_10s"] is None
    assert d["max_step_ms"] is None
    assert d["step_at_s"] is None


def test_summary_line_carries_the_drift_fields():
    result = {
        "status": "fail",
        "reasons": ["|A/V| 60.5 ms > 40 ms"],
        "av_ms": 60.5,
        "drift": {
            "drift_ms_per_10s": 25.0,
            "max_step_ms": 5.1,
            "step_at_s": 11.979,
            "windows_used": 9,
            "windows_total": 10,
        },
    }
    line = avs.summary_line(result)
    assert "drift_ms_per_10s=25.0 max_step_ms=5.1 step_at_s=11.979 windows=9/10" in line
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
    # Diagnostics only: the verdict is still the whole-take one. A mid-take
    # jump halves the whole-take audio correlation, so it reads cannot_measure
    # (audio), as on release run 36068121677 (corr 0.899). The profile is what
    # shows the jump.
    assert r["audio"]["corr"] < avs.MIN_AUDIO_CORR
    assert r["status"] == "cannot_measure"
    assert r["unmeasurable_sides"] == ["audio"]


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
