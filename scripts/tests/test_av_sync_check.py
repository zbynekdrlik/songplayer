"""Tests for scripts/av_sync_check.py (#147 post-deploy A/V + dropout gate).

Synthetic fixtures built with numpy only: an irregular click train over a
noise bed as the ORIGINAL audio, plus a matching "flash" frame sequence (the
picture cuts to a new random texture at every click) as the ORIGINAL video. A
"recording" is cut out of them with known audio and video offsets, a
letterboxed 64x36 canvas, a 30 fps sample-and-hold frame clock against the 25
fps original, and AAC-like priming (sample 0 at -0.021 s).

The ffmpeg I/O layer is not tested here. The Eval Checks CI job has no ffmpeg,
so that layer runs on the box in e2e/post-deploy-av-sync.spec.ts.
"""

from __future__ import annotations

import importlib.util
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
REC_T0 = -0.021  # AAC priming: sample 0 of the recording's audio is at -21 ms
CONTENT = (2, 34, 0, 64)  # 1920x960 source in a 1920x1080 canvas, 64x36 grid


def _click_times(rng: np.random.Generator, duration: float) -> np.ndarray:
    times, t = [], 0.3
    while t < duration - 0.3:
        times.append(t)
        t += rng.uniform(0.25, 0.9)  # irregular, so no periodic correlation peaks
    return np.array(times)


def _original(seed: int = 3, cuts_every_click: bool = True):
    rng = np.random.default_rng(seed)
    n = int(ORIG_S * SR)
    block = SR // 10
    env = np.repeat(rng.uniform(0.1, 1.0, n // block + 1), block)[:n]
    audio = rng.standard_normal(n) * env * 0.1
    clicks = _click_times(rng, ORIG_S)
    for t in clicks:
        i = int(t * SR)
        audio[i : i + 40] += 0.9

    pts = np.arange(int(ORIG_S * ORIG_FPS)) / ORIG_FPS
    cut_times = clicks if cuts_every_click else clicks[::8]
    scene = np.searchsorted(cut_times, pts, side="right")
    textures = rng.uniform(0, 255, size=(scene.max() + 1, 32, 64))
    frames = textures[scene]
    return audio, frames, pts


def _recording(audio, frames, pts, start, av_ms, noise=2.0, seed=11):
    """Recording whose picture shows orig time ``start`` at rec time 0, with the
    audio ``av_ms`` AHEAD of the picture, sample 0 at REC_T0."""
    rng = np.random.default_rng(seed)
    audio_off = start + av_ms / 1000.0
    n = int(REC_S * SR)
    src = np.round((REC_T0 + np.arange(n) / SR + audio_off) * SR).astype(int)
    rec_audio = 0.7 * audio[src] + rng.standard_normal(n) * 0.002

    rec_pts = np.arange(int(REC_S * REC_FPS)) / REC_FPS
    j = np.searchsorted(pts, rec_pts + start, side="right") - 1
    canvas = np.zeros((len(rec_pts), 36, 64))
    y0, y1, x0, x1 = CONTENT
    canvas[:, y0:y1, x0:x1] = frames[j] + rng.normal(0, noise, size=frames[j].shape)
    return rec_audio, canvas, rec_pts


def _measure(audio, frames, pts, rec_audio, canvas, rec_pts, max_av_ms=40.0):
    aud = avs.audio_offset(rec_audio, audio, SR, rec_t0=REC_T0, orig_t0=0.0)
    aligned = audio[aud["lag"] : aud["lag"] + len(rec_audio)]
    drops = avs.dropout_blocks(rec_audio, aligned, SR, t0=REC_T0)
    y0, y1, x0, x1 = avs.content_box(1920, 1080, 1920, 960)
    vid = avs.video_offset(
        canvas[:, y0:y1, x0:x1], rec_pts, frames, pts, center_s=aud["offset_s"]
    )
    av_ms = (aud["offset_s"] - vid["offset_s"]) * 1000.0
    status, reasons, sides = avs.verdict(
        aud["corr"],
        vid["match"],
        vid["contrast"],
        av_ms,
        drops["dropout_count"],
        max_av_ms,
    )
    return {
        "aud": aud,
        "vid": vid,
        "drops": drops,
        "av_ms": av_ms,
        "status": status,
        "reasons": reasons,
        "sides": sides,
    }


# --- the four acceptance cases from the design record --------------------------


def test_audio_120ms_ahead_is_measured_and_fails():
    audio, frames, pts = _original()
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=17.3, av_ms=120.0)
    )
    assert r["av_ms"] == pytest.approx(120.0, abs=10.0), r
    assert r["aud"]["offset_s"] == pytest.approx(17.42, abs=0.001)
    assert r["aud"]["corr"] >= avs.MIN_AUDIO_CORR
    assert r["aud"]["second_corr"] < r["aud"]["corr"] - 0.3  # a unique audio match
    assert r["vid"]["match"] >= avs.MIN_VIDEO_MATCH
    assert r["drops"]["dropout_count"] == 0
    assert r["status"] == "fail"
    assert avs.exit_code(r["status"]) == 1
    assert any("A/V" in reason for reason in r["reasons"])


def test_in_sync_recording_passes():
    audio, frames, pts = _original()
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=31.05, av_ms=0.0)
    )
    assert abs(r["av_ms"]) <= 10.0, r
    assert r["drops"]["dropout_count"] == 0
    assert r["status"] == "pass", r["reasons"]
    assert avs.exit_code(r["status"]) == 0


def test_injected_200ms_zero_gap_is_a_dropout_and_fails():
    audio, frames, pts = _original()
    rec_audio, canvas, rec_pts = _recording(audio, frames, pts, start=12.0, av_ms=0.0)
    gap_start = int((7.0 - REC_T0) * SR)
    rec_audio[gap_start : gap_start + int(0.2 * SR)] = 0.0
    r = _measure(audio, frames, pts, rec_audio, canvas, rec_pts)
    assert abs(r["av_ms"]) <= 10.0  # the gap must not disturb the offsets
    events = r["drops"]["dropout_events"]
    assert events, r["drops"]
    assert all(
        7.0 <= e["start_s"] and e["start_s"] + e["ms"] / 1000 <= 7.2 for e in events
    ), events
    # Every loud 10 ms sub-block inside the gap is caught (the quietest 20 %
    # of the song's sub-blocks are not classified).
    assert sum(e["ms"] for e in events) >= 120, events
    assert r["status"] == "fail"
    assert avs.exit_code(r["status"]) == 1
    assert any("dropout" in reason for reason in r["reasons"])


def test_low_correlation_cannot_measure_exit_2():
    audio, frames, pts = _original()
    _, canvas, rec_pts = _recording(audio, frames, pts, start=20.0, av_ms=0.0)
    unrelated = np.random.default_rng(99).standard_normal(int(REC_S * SR)) * 0.05
    r = _measure(audio, frames, pts, unrelated, canvas, rec_pts)
    assert r["aud"]["corr"] < avs.MIN_AUDIO_CORR
    assert r["status"] == "cannot_measure"
    assert avs.exit_code(r["status"]) == 2
    assert any("audio correlation" in reason for reason in r["reasons"])


# --- robustness the gate depends on --------------------------------------------


def test_audio_behind_picture_is_negative_and_fails():
    audio, frames, pts = _original()
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=25.0, av_ms=-80.0)
    )
    assert r["av_ms"] == pytest.approx(-80.0, abs=10.0), r
    assert r["status"] == "fail"


def test_mostly_static_lyric_video_still_measures_the_true_offset():
    """Few cuts (a lyric video): most frames match every candidate equally. A
    per-frame median would drift to the window centre (the audio offset, A/V ~0)
    and false-PASS a real 120 ms offset; the global alignment must not."""
    audio, frames, pts = _original(cuts_every_click=False)
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=17.3, av_ms=120.0)
    )
    assert r["av_ms"] == pytest.approx(120.0, abs=10.0), r
    assert r["status"] == "fail"


def test_fully_static_video_cannot_measure():
    audio, frames, pts = _original()
    frames = np.repeat(frames[:1], len(frames), axis=0)  # one still image
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=20.0, av_ms=0.0)
    )
    assert r["vid"]["contrast"] < avs.MIN_VIDEO_CONTRAST
    assert r["status"] == "cannot_measure"
    assert any("contrast" in reason for reason in r["reasons"])


def test_wrong_picture_is_low_match_cannot_measure():
    audio, frames, pts = _original()
    rec_audio, canvas, rec_pts = _recording(audio, frames, pts, start=20.0, av_ms=0.0)
    other = np.random.default_rng(5).uniform(0, 255, size=canvas.shape)
    r = _measure(audio, frames, pts, rec_audio, other, rec_pts)
    assert r["vid"]["match"] < avs.MIN_VIDEO_MATCH
    assert r["status"] == "cannot_measure"


def test_video_window_is_clamped_near_the_start_of_the_video():
    """The first manual attempt crashed on an unclamped window: a recording
    that starts 0.2 s into the song asks for original frames before 0."""
    audio, frames, pts = _original()
    rec_audio, canvas, rec_pts = _recording(audio, frames, pts, start=0.2, av_ms=0.0)
    r = _measure(audio, frames, pts, rec_audio, canvas, rec_pts)
    assert r["vid"]["shift_range_s"][0] >= 0.0
    assert abs(r["av_ms"]) <= 10.0, r


def test_video_window_is_clamped_near_the_end_of_the_video():
    audio, frames, pts = _original()
    start = ORIG_S - REC_S - 0.3
    r = _measure(
        audio, frames, pts, *_recording(audio, frames, pts, start=start, av_ms=0.0)
    )
    last_end = pts[-1] + 1.0 / ORIG_FPS
    assert r["vid"]["shift_range_s"][1] + REC_S - 1.0 / REC_FPS <= last_end
    assert abs(r["av_ms"]) <= 10.0, r


def test_video_offset_outside_decoded_span_raises():
    _, frames, pts = _original()
    rec = frames[:30]
    with pytest.raises(ValueError, match="no shift"):
        avs.video_offset(
            rec, np.arange(30) / REC_FPS, frames[:50], pts[:50], center_s=40.0
        )


# --- content box (letterbox crop from the source aspect, never hardcoded) ------


@pytest.mark.parametrize(
    ("canvas", "src", "box"),
    [
        ((1920, 1080), (1920, 960), (2, 34, 0, 64)),  # the 24.9. case: rows 2:34
        ((1920, 1080), (1920, 1080), (0, 36, 0, 64)),  # full frame
        ((1920, 1080), (1440, 1080), (0, 36, 8, 56)),  # 4:3 pillarbox
        # 2.4:1 letterbox = 26.7 content rows spanning 4.67..31.33: the two
        # partial rows (4 and 31) mix bar and picture and are dropped.
        ((1920, 1080), (1920, 800), (5, 31, 0, 64)),
    ],
)
def test_content_box(canvas, src, box):
    assert avs.content_box(*canvas, *src) == box


@pytest.mark.parametrize(
    ("canvas", "src", "crop"),
    [
        ((1920, 1080), (1920, 960), (0.0, 0.0, 1920.0, 960.0)),
        # rows 4.67..31.33 kept as 5..31: the original loses the same 1/3 row
        # (10 px) top and bottom before it is scaled into the 26-row box.
        ((1920, 1080), (1920, 800), (0.0, 10.0, 1920.0, 780.0)),
        ((1920, 1080), (1000, 1080), (20.0, 0.0, 960.0, 1080.0)),
    ],
)
def test_source_crop_matches_the_kept_grid_cells(canvas, src, crop):
    assert avs.source_crop(*canvas, *src) == pytest.approx(crop, abs=1e-6)


def test_content_box_rejects_bad_sizes():
    with pytest.raises(ValueError):
        avs.content_box(1920, 1080, 0, 960)


# --- dropout detector details -------------------------------------------------


def test_dropouts_ignore_encoder_priming_at_the_recording_edges():
    rng = np.random.default_rng(1)
    orig = rng.standard_normal(SR * 5) * 0.2
    rec = orig.copy() * 0.8
    rec[:170] = 0.0  # AAC priming decodes as silence at sample 0
    rec[-300:] = 0.0  # stop edge
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["dropout_count"] == 0
    assert out["gain"] == pytest.approx(0.8, abs=0.05)


@pytest.mark.parametrize(
    ("gap_ms", "start_s"), [(20, 2.0137), (30, 2.0137), (40, 3.0561)]
)
def test_short_unaligned_gap_is_a_dropout(gap_ms, start_s):
    """A lost NDI audio buffer is 10-50 ms and lands anywhere. A grid-aligned
    50 ms block would miss gaps like these (review finding, #147)."""
    rng = np.random.default_rng(6)
    orig = rng.standard_normal(SR * 5) * 0.2
    rec = orig * 0.8
    i = int(start_s * SR)
    rec[i : i + int(gap_ms * SR / 1000)] = 0.0
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["dropout_count"] == 1, out["dropout_events"]
    ev = out["dropout_events"][0]
    assert start_s - 0.001 <= ev["start_s"]
    assert ev["start_s"] + ev["ms"] / 1000 <= start_s + gap_ms / 1000 + 0.001
    assert avs.verdict(0.99, 0.99, 0.01, 0.0, out["dropout_count"])[0] == "fail"


def test_12ms_gap_is_caught_at_every_phase():
    """Sliding 10 ms window, 1 ms hop: any gap >= 11 ms holds a whole window
    whatever its phase. A fixed 10 ms grid missed a 12 ms gap 14 times in 20."""
    rng = np.random.default_rng(12)
    orig = rng.standard_normal(SR * 3) * 0.2
    gap = int(0.012 * SR)
    for phase in range(0, 80, 4):  # every half-millisecond of a 10 ms period
        rec = orig * 0.8
        i = SR + phase
        rec[i : i + gap] = 0.0
        assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == 1, phase


def test_fractional_lag_does_not_blind_the_detector():
    """An LS gain shrinks under a sub-sample lag; the RMS-ratio level does not."""
    rng = np.random.default_rng(13)
    orig = rng.standard_normal(SR * 4) * 0.2
    rec = 0.8 * (orig + np.roll(orig, 1)) / 2  # half-sample-like smear
    i = 2 * SR
    rec[i : i + int(0.03 * SR)] = 0.0
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["dropout_count"] == 1
    assert out["level"] >= abs(out["gain"])


def test_near_silence_in_the_original_is_never_loud():
    rng = np.random.default_rng(14)
    orig = rng.standard_normal(SR * 6) * 0.2
    orig[SR * 2 : SR * 5] *= 0.01  # 50 % of the take at ~-54 dBFS (below the floor)
    rec = orig * 0.8
    rec[SR * 2 : SR * 5] = 0.0  # an encoder rounding near-silence to zero
    assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == 0


def test_gap_in_an_audible_quieter_passage_is_caught():
    """Review finding (#147): the old min(p20, 0.5 x median) gate skipped an
    audible passage 12 dB under the take's level. A lost buffer there must
    still fail (the -45 dBFS floor and -20 dB relative bound keep it loud)."""
    rng = np.random.default_rng(15)
    orig = rng.standard_normal(SR * 6) * 0.2
    orig[SR * 3 : SR * 3 + SR // 2] *= 0.25  # -12 dB, clearly audible
    for gap_ms in (20, 50):
        rec = orig * 0.8
        i = SR * 3 + 1000
        rec[i : i + int(gap_ms * SR / 1000)] = 0.0
        assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == 1, gap_ms


def test_hard_onsets_with_a_one_sample_slip_are_not_dropouts():
    """A window that merely clips a hard onset must not be judged against a
    recording one sample late: the original has to be loud THROUGHOUT."""
    rng = np.random.default_rng(16)
    orig = np.zeros(SR * 6)
    t = 0
    while t < len(orig) - SR // 4:
        n = int(rng.integers(SR // 20, SR // 5))
        orig[t : t + n] = rng.standard_normal(n) * 0.3  # hard-edged burst
        t += n + int(rng.integers(SR // 20, SR // 5))  # digital silence between
    rec = 0.8 * np.concatenate(([0.0], orig[:-1]))  # one sample late
    assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == 0


@pytest.mark.parametrize(("apart_ms", "events"), [(40, 1), (60, 2)])
def test_dropout_runs_closer_than_one_block_merge(apart_ms, events):
    rng = np.random.default_rng(17)
    orig = rng.standard_normal(SR * 4) * 0.2
    rec = orig * 0.8
    g = int(0.02 * SR)
    i = 2 * SR
    j = i + g + int(apart_ms * SR / 1000)
    rec[i : i + g] = 0.0
    rec[j : j + g] = 0.0
    assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == events


def test_a_block_holding_a_dropout_is_not_also_a_glitch():
    rng = np.random.default_rng(18)
    orig = rng.standard_normal(SR * 4) * 0.2
    rec = orig * 0.8
    i = 2 * SR + 20  # inside one 50 ms block
    rec[i : i + int(0.03 * SR)] = 0.0
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["dropout_count"] == 1
    assert out["glitch_blocks"] == 0


def test_a_video_analysis_error_still_fails_found_dropouts():
    """measure() turns a picture-step exception into NaN match/contrast/A/V."""
    nan = float("nan")
    assert avs.verdict(0.99, nan, nan, nan, 2)[0] == "fail"
    status, _, sides = avs.verdict(0.99, nan, nan, nan, 0)
    assert (status, sides) == ("cannot_measure", ["video"])


def test_clean_recording_has_no_dropouts_or_glitches():
    rng = np.random.default_rng(8)
    orig = rng.standard_normal(SR * 8) * np.repeat(rng.uniform(0.05, 1.0, 80), SR // 10)
    rec = 0.7 * orig + rng.standard_normal(len(orig)) * 0.003
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["dropout_count"] == 0, out["dropout_events"]
    assert out["glitch_blocks"] == 0
    assert out["median_rel_err"] < 0.1


def test_quiet_original_passage_is_not_a_dropout():
    rng = np.random.default_rng(2)
    orig = rng.standard_normal(SR * 6) * 0.2
    orig[SR * 3 : SR * 3 + SR // 2] *= 0.001  # the song itself is silent here
    rec = orig * 0.5
    rec[SR * 3 : SR * 3 + SR // 2] = 0.0
    assert avs.dropout_blocks(rec, orig, SR)["dropout_count"] == 0


def test_glitch_is_reported_but_does_not_fail():
    rng = np.random.default_rng(4)
    orig = rng.standard_normal(SR * 6) * 0.2
    rec = orig.copy()
    rec[SR * 2 : SR * 2 + 400] = rng.standard_normal(400) * 0.2  # garbage, not silence
    out = avs.dropout_blocks(rec, orig, SR)
    assert out["glitch_blocks"] >= 1
    assert out["dropout_count"] == 0
    assert avs.verdict(0.99, 0.99, 0.01, 5.0, out["dropout_count"])[0] == "pass"


# --- verdict / exit codes -----------------------------------------------------


def test_verdict_boundaries():
    assert avs.verdict(0.99, 0.99, 0.01, 40.0, 0) == ("pass", [], [])
    assert avs.verdict(0.99, 0.99, 0.01, -40.0, 0) == ("pass", [], [])
    assert avs.verdict(0.99, 0.99, 0.01, 40.1, 0)[0] == "fail"
    assert avs.verdict(0.99, 0.99, 0.01, -40.1, 0)[0] == "fail"
    assert avs.verdict(0.99, 0.99, 0.01, 0.0, 1)[0] == "fail"
    assert avs.verdict(0.9, 0.95, 0.002, 0.0, 0)[0] == "pass"
    assert avs.verdict(0.8999, 0.99, 0.01, 0.0, 0)[0] == "cannot_measure"
    assert avs.verdict(0.99, 0.9499, 0.01, 0.0, 0)[0] == "cannot_measure"
    assert avs.verdict(0.99, 0.99, 0.0019, 0.0, 0)[0] == "cannot_measure"
    assert avs.verdict(float("nan"), 0.99, 0.01, 0.0, 0)[0] == "cannot_measure"
    assert avs.verdict(0.99, 0.99, 0.01, float("nan"), 0)[0] == "fail"
    assert avs.verdict(0.99, 0.99, 0.01, 30.0, 0, max_av_ms=20.0)[0] == "fail"


def test_audio_side_cannot_measure_wins_over_everything():
    status, reasons, sides = avs.verdict(0.5, 0.99, 0.01, 300.0, 2)
    assert status == "cannot_measure"
    assert sides == ["audio"]
    # The dropouts are still reported, but they are not a verdict on their own.
    assert reasons == ["audio correlation 0.500 < 0.9", "2 audio dropout(s)"]


def test_dropouts_fail_even_when_the_picture_is_unmeasurable():
    """Review finding (#147): a still/overlaid picture must never turn found
    dropouts into a retakeable cannot-measure. Dropouts are audio-only
    evidence."""
    status, reasons, sides = avs.verdict(0.99, 0.5, 0.0001, 0.0, 3)
    assert status == "fail"
    assert sides == []
    assert "3 audio dropout(s)" in reasons


def test_picture_side_cannot_measure_is_marked_video_only():
    status, reasons, sides = avs.verdict(0.99, 0.5, 0.01, 999.0, 0)
    assert status == "cannot_measure"
    assert sides == ["video"]
    assert not any("A/V" in r for r in reasons)  # A/V is not judged without a picture


def test_exit_codes():
    assert [avs.exit_code(s) for s in ("pass", "fail", "cannot_measure")] == [0, 1, 2]


def test_audio_offset_rejects_silence_and_oversized_recording():
    with pytest.raises(ValueError, match="silence"):
        avs.audio_offset(np.zeros(100), np.ones(1000))
    with pytest.raises(ValueError, match="no longer"):
        avs.audio_offset(np.ones(1000), np.ones(100))


def test_frame_without_pts_is_a_named_error():
    with pytest.raises(RuntimeError, match="without a usable pts_time"):
        avs._pts("[Parsed_showinfo_0 @ 0x1] n:   0 pts:NOPTS pts_time:NOPTS", "rec.mkv")
    assert avs._pts(
        "[Parsed_ashowinfo_0 @ 0x1] n:0 pts:-1024 pts_time:-0.0213333", "a"
    ) == (pytest.approx(-0.0213333))


def test_main_reports_analysis_error_as_cannot_measure(capsys, tmp_path):
    missing = str(tmp_path / "nope.mkv")
    rc = avs.main(
        [
            "--recording", missing, "--orig-audio", missing, "--orig-video", missing,
            "--ffmpeg", str(tmp_path / "no-ffmpeg"),
        ]
    )  # fmt: skip
    out = capsys.readouterr()
    assert rc == 2
    assert '"status": "cannot_measure"' in out.out
    assert "analysis error" in out.out
    assert "AV-SYNC status=cannot_measure" in out.err
