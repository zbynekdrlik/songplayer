"""Tests for the A/V gate's settle handling in scripts/av_sync_check.py (#268).

The post-deploy gate points cg OBS's probe input at SP-program right before a
take. The probe then cold-starts: camera-box's shallow lock latches its depth,
and the audio hold follows the realized delay, slewed through a resampler at
1000 ppm (camera-box 1367): one frame of depth = 33 ms = 33 s of slewing. Both
failing takes on #268 recorded ONLY that slew (-999.67 ppm and +999.59 ppm,
fit residual < 0.1 ms over 79 of 79 windows). The gate therefore records past
the settle, and the analysis starts after it (``--analysis-start-s``).

The original is an analytic tone sum, so a slewing recording is exact at
every sample and independent of the code under test (as in
``test_av_sync_warp.py``). Cases:

* the analysed part drops the head and moves its sample-0 time; 0 keeps the
  whole recording; too short an analysed part is a named error;
* the picture's frames are cut at the same time;
* a take whose first 30 s slew at -1000 ppm PASSES from 30 s on and reads
  cannot_measure over the whole recording;
* the ``settle`` diagnostic shows the head slewing and the tail settled;
* a smooth slew past the warp's bound in the analysed part is NAMED; a step,
  a warpable rate and no fit are not;
* ``main`` passes ``--analysis-start-s`` through and the summary carries it.
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
ORIG_S = 60.0
START_S = 5.0  # the recording starts 5 s into the original
PPM = -1000.0  # camera-box's audio-hold slew rate
SLEW_S = 30.0  # the head that slews; settled after it
REC_S = 50.0


class _Tones:
    """Random tones (60-3400 Hz) under a slow per-tone envelope, evaluable at
    any time (seconds)."""

    def __init__(self, seed: int = 7, n: int = 60):
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


@functools.lru_cache(maxsize=None)
def _slewing(ppm: float = PPM, slew_s: float = SLEW_S, rec_s: float = REC_S):
    """The original heard from ``START_S`` on whose audio moves ``ppm`` for
    the first ``slew_s`` seconds, then holds still (the settled hold)."""
    t = np.arange(int(rec_s * SR)) / SR
    orig_t = START_S + t + ppm * 1e-6 * np.minimum(t, slew_s)
    rng = np.random.default_rng(13)
    return 0.7 * _signal()(orig_t) + rng.standard_normal(len(t)) * 0.002


SETTLED_OFFSET_S = START_S + PPM * 1e-6 * SLEW_S  # 4.970 s


# --- the analysed part ----------------------------------------------------------


def test_the_analysed_part_drops_the_head_and_moves_sample_0():
    audio = np.arange(int(50 * SR), dtype=np.float64)
    part, t0, cut = avs.analysed_part(audio, -0.021, 30.0)
    assert cut == int(round(30.021 * SR))
    assert t0 == pytest.approx(30.0, abs=1e-9)
    assert part[0] == audio[cut] and len(part) == len(audio) - cut


def test_analysis_start_0_keeps_the_whole_recording():
    audio = np.arange(int(20 * SR), dtype=np.float64)
    part, t0, cut = avs.analysed_part(audio, -0.021, 0.0)
    assert cut == 0 and t0 == -0.021
    np.testing.assert_array_equal(part, audio)


def test_too_short_an_analysed_part_is_a_named_error():
    audio = np.zeros(int(30 * SR))
    with pytest.raises(ValueError, match=r"--analysis-start-s 25 leaves 5\.00 s"):
        avs.analysed_part(audio, 0.0, 25.0)


def test_the_picture_is_cut_at_the_same_time():
    pts = np.arange(0, 50, 1 / 30)
    frames = np.arange(len(pts))[:, None, None] * np.ones((1, 2, 3))
    f, p = avs.analysed_frames(frames, pts, 30.0)
    assert p[0] == pytest.approx(30.0) and p.min() >= 30.0 - 1e-9
    assert len(p) == int((pts >= 30.0 - 1e-9).sum())
    np.testing.assert_array_equal(f, frames[pts >= 30.0 - 1e-9])
    # An ffmpeg input seek also returns the frame ON SCREEN at the start
    # (pts 13.367 for a 13.37 s seek): it is dropped.
    f, p = avs.analysed_frames(frames, pts, 13.37)
    assert p[0] >= 13.37
    same_f, same_p = avs.analysed_frames(frames, pts, 0.0)
    assert same_f is frames and same_p is pts


def test_no_frame_after_the_start_is_a_named_error():
    pts = np.arange(0, 10, 1 / 30)
    with pytest.raises(ValueError, match="no recording frame"):
        avs.analysed_frames(np.zeros((len(pts), 2, 2)), pts, 11.0)


# --- measure(): the settled part passes, the whole recording does not -----------


def _patch(monkeypatch, rec, seen):
    def fake_decode_audio(_ffmpeg, path):
        return (rec, 0.0) if path == "rec.mkv" else (_original(), 0.0)

    def video(_ffmpeg, _rec, _orig, center_s, analysis_start_s=0.0):
        seen.append(analysis_start_s)
        # The picture shows the settled position: in sync with the settled audio.
        return {
            "frames": 600,
            "_offset_exact": SETTLED_OFFSET_S,
            "_match_exact": 0.99,
            "_contrast_exact": 0.01,
        }

    monkeypatch.setattr(avs, "decode_audio", fake_decode_audio)
    monkeypatch.setattr(avs, "_measure_video", video)


def test_a_take_past_the_settle_passes_from_the_analysis_start(monkeypatch):
    seen: list[float] = []
    _patch(monkeypatch, _slewing(), seen)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4", analysis_start_s=SLEW_S)
    json.dumps(r, allow_nan=False)
    assert r["status"] == "pass", r["reasons"]
    assert seen == [SLEW_S], "the picture is cut at the same start"
    assert r["analysis_start_s"] == SLEW_S
    assert r["recording_s"] == pytest.approx(REC_S, abs=0.01)
    assert r["analysed_s"] == pytest.approx(REC_S - SLEW_S, abs=0.01)
    assert r["audio"]["corr"] >= 0.98
    assert abs(r["audio"]["rate_ppm"]) < 5.0
    assert r["audio"]["offset_s"] == pytest.approx(SETTLED_OFFSET_S, abs=0.0005)
    assert r["dropouts"]["dropout_count"] == 0
    assert abs(r["av_ms"]) < 1.0
    # The per-window profile covers the analysed part only.
    assert len(r["segments"]) == 10
    assert all(s["t_s"] >= SLEW_S - 1e-6 for s in r["segments"])
    assert "analysed_from_s=30.0" in avs.summary_line(r)


def test_the_same_take_analysed_whole_cannot_be_measured(monkeypatch):
    # The bug #268 fixes: the slewing head inside the analysed part.
    seen: list[float] = []
    _patch(monkeypatch, _slewing(), seen)
    r = avs.measure("rec.mkv", "a.flac", "v.mp4")
    assert seen == [0.0]
    assert r["analysis_start_s"] == 0.0
    assert r["status"] == "cannot_measure"
    assert "audio" in r["unmeasurable_sides"]


def test_settle_shows_the_head_slewing_and_the_tail_settled(monkeypatch):
    _patch(monkeypatch, _slewing(), [])
    r = avs.measure("rec.mkv", "a.flac", "v.mp4", analysis_start_s=SLEW_S)
    settle = r["settle"]
    assert settle["window_s"] == avs.SETTLE_WINDOW_S
    rows = settle["rows"]
    assert len(rows) == int(REC_S / avs.SETTLE_WINDOW_S)
    head = [row for row in rows if row["t_end_s"] <= SLEW_S + 1e-6]
    tail = [row for row in rows if row["t_s"] >= SLEW_S - 1e-6]
    assert len(head) == 15 and len(tail) == 10
    for row in head:
        assert row["windows"] == 8, row
        assert row["rate_ppm"] == pytest.approx(PPM, abs=60.0), row
    for row in tail:
        assert row["rate_ppm"] == pytest.approx(0.0, abs=60.0), row
        assert row["dev_ms"] == pytest.approx(0.0, abs=0.5), row
    # Against the settled alignment the head starts 30 ms early and closes in.
    assert rows[0]["dev_ms"] == pytest.approx(29.0, abs=1.0)
    assert rows[14]["dev_ms"] == pytest.approx(1.0, abs=1.0)


def test_settle_rows_read_a_rate_and_a_level_per_window():
    win = int(0.25 * SR)
    t = np.arange(win / 2, 20 * SR, win)  # 0.25 s window centres, 20 s
    dev = np.where(t < 8 * SR, 500e-6 * t, 500e-6 * 8 * SR)  # +500 ppm, then still
    keep = np.ones(len(t), dtype=bool)
    keep[(t >= 12 * SR) & (t < 14 * SR)] = False  # a window nothing matched
    keep[(t >= 16 * SR) & (t < 18 * SR) & (np.arange(len(t)) % 8 > 1)] = False
    rows = avs.settle_rows(t[keep], dev[keep], SR, -0.021, int(20 * SR))
    assert [row["t_s"] for row in rows[:2]] == [-0.021, 1.979]
    assert len(rows) == 10
    assert rows[0]["rate_ppm"] == pytest.approx(500.0, abs=1.0)
    assert rows[5]["rate_ppm"] == pytest.approx(0.0, abs=1.0)
    assert rows[5]["dev_ms"] == pytest.approx(500e-6 * 8 * 1000, abs=0.01)
    assert rows[6] == {
        "t_s": 11.979,
        "t_end_s": 13.979,
        "windows": 0,
        "dev_ms": None,
        "rate_ppm": None,
    }
    # Two windows: a level, but too few for a rate.
    assert rows[8]["windows"] == 2
    assert rows[8]["dev_ms"] is not None and rows[8]["rate_ppm"] is None


# --- a smooth slew past the warp's bound is named ----------------------------------


def test_a_smooth_1000ppm_slew_is_named():
    # Seeded with the true start lag: at 1000 ppm the one-lag global search
    # is smeared (#268 run 38412602549 landed on a noise peak).
    rec = _slewing(slew_s=20.0, rec_s=20.0)
    comp = avw.compensate(rec, _original(), int(START_S * SR), SR)
    assert comp["warped"] is False
    assert comp["rate_ppm"] == pytest.approx(PPM, abs=5.0)
    assert comp["fit_residual_median_ms"] < 0.05
    note = avs.audio_slew_note(
        comp["rate_ppm"], comp["fit_residual_median_ms"], comp["fit_windows"]
    )
    assert note is not None
    assert "slews smoothly at -1000 ppm" in note
    assert "the recorded audio had not settled" in note


def test_a_step_a_warpable_rate_and_no_fit_are_not_named():
    # A 12 ms jump: the line fits half the windows badly.
    t = np.arange(int(20 * SR)) / SR
    orig_t = START_S + t + np.where(t >= 10.0, 0.012, 0.0)
    rec = 0.7 * _signal()(orig_t)
    comp = avw.compensate(rec, _original(), int(START_S * SR), SR)
    assert comp["fit_residual_median_ms"] > avw.MAX_FIT_RESIDUAL_MS
    assert (
        avs.audio_slew_note(
            comp["rate_ppm"], comp["fit_residual_median_ms"], comp["fit_windows"]
        )
        is None
    )
    assert avs.audio_slew_note(150.0, 0.01, 79) is None, "the warp takes it"
    assert avs.audio_slew_note(None, None, 3) is None, "no fit"
    assert avs.audio_slew_note(-999.67, 0.01, 79) is not None


def test_the_verdict_puts_the_note_right_after_the_audio_reason():
    note = "the audio slews smoothly at -1000 ppm ..."
    status, reasons, sides = avs.verdict(0.29, 0.99, 0.2, 5.0, 2, audio_note=note)
    assert status == "cannot_measure" and sides == ["audio"]
    assert reasons == ["audio correlation 0.290 < 0.9", note, "2 audio dropout(s)"]
    # A measurable audio side never carries it.
    status, reasons, _ = avs.verdict(0.99, 0.99, 0.2, 5.0, 0, audio_note=note)
    assert (status, reasons) == ("pass", [])


# --- the command line -------------------------------------------------------------


def test_main_passes_the_analysis_start_through(monkeypatch, capsys):
    calls = []

    def fake_measure(*args, **kwargs):
        calls.append((args, kwargs))
        return {"status": "pass", "reasons": [], "analysis_start_s": 45.2}

    monkeypatch.setattr(avs, "measure", fake_measure)
    rc = avs.main(
        ["--recording", "r.mkv", "--orig-audio", "a.flac", "--orig-video", "v.mp4",
         "--analysis-start-s", "45.2"]
    )  # fmt: skip
    assert rc == 0
    assert calls[0][1]["analysis_start_s"] == pytest.approx(45.2)
    assert "analysed_from_s=45.2" in capsys.readouterr().err


def test_a_negative_analysis_start_is_refused(capsys):
    with pytest.raises(SystemExit):
        avs.main(
            ["--recording", "r.mkv", "--orig-audio", "a.flac", "--orig-video", "v.mp4",
             "--analysis-start-s", "-1"]
        )  # fmt: skip
    assert "--analysis-start-s must be >= 0, got -1" in capsys.readouterr().err
