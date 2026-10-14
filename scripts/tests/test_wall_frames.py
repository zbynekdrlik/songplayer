"""Tests for scripts/wall_frames.py, the wall clip reader (#272).

Every picture is drawn by ``wall_clip.draw_frame`` (the clips' own code), then
treated the way the chain treats it: a two-frame blend (#251's weights), a
vertical crop (a wall shows a band), the SNV band scaled x4, noise, colour
channels, a clip that spans only part of the capture's width. Cases:

* a clean frame reads its index, a 30/70 blend reads both indices and the
  weight of the later one (whichever way its grey cell flips);
* any quarter-height band, and SNV's x4-scaled band with noise, still read;
* what cannot be read is named, never guessed: no strip, two grey cells, a
  grey cell between codes that are not neighbours, a torn picture;
* the pacer's rule: 25 on 30 = plain, 213, 170, 128, 85, 42; 24 on 30 =
  plain, 204, 153, 102, 51; nothing for 30, 15 or 50 fps;
* ``analyse``: a clean 25-on-30 run through a loop of the clip, then a lost
  boundary, a lost frame, a jump back, a wrong weight and a repeated
  boundary, each counted where it belongs;
* end to end: pictures blended by the rule are read back to the rule.
"""

from __future__ import annotations

import dataclasses
import functools
import json
import re
from fractions import Fraction
from pathlib import Path

import numpy as np
import pytest

import wall_clip as clip
import wall_frames as wf


@functools.lru_cache(maxsize=None)
def _frame(index: int, fps: int, width: int, height: int) -> np.ndarray:
    picture = clip.draw_frame(index, fps, clip.layout(width, height))
    picture.setflags(write=False)
    return picture


def frame(index: int, fps: int = 25, width: int = 960, height: int = 540):
    return _frame(index, fps, width, height)


def blend(a: np.ndarray, b: np.ndarray, weight: float) -> np.ndarray:
    """``b`` over ``a`` at ``weight``, rounded to 8 bits like the program."""
    mixed = a.astype(np.float64) * (1 - weight) + b.astype(np.float64) * weight
    return np.clip(np.round(mixed), 0, 255).astype(np.uint8)


def noisy(picture: np.ndarray, sigma: float, seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    out = picture.astype(np.float64) + rng.normal(0, sigma, picture.shape)
    return np.clip(np.round(out), 0, 255).astype(np.uint8)


def flips_up(index: int) -> bool:
    """Whether the cell that changes from ``index`` to ``index + 1`` goes
    black -> white."""
    return clip.gray(index + 1) > clip.gray(index)


def test_a_clean_frame_reads_its_index_in_every_band():
    reading = wf.read_frame(frame(1234))
    assert reading == wf.Reading("ok", 1234, None, 0, clip.BANDS)
    assert reading.position() == 1234


@pytest.mark.parametrize("index", [6, 7])
def test_a_30_70_blend_reads_both_frames_and_the_later_one_s_weight(index):
    # 6 -> 7 flips a cell black -> white, 7 -> 8 white -> black: both ways.
    assert flips_up(6) != flips_up(7)
    for weight, q8 in ((0.3, 77), (0.7, 179)):
        reading = wf.read_frame(blend(frame(index), frame(index + 1), weight))
        assert reading.ok, reading
        assert (reading.index, reading.next_index) == (index, index + 1)
        assert abs(reading.weight_q8 - q8) <= 1, (weight, reading)


def test_any_quarter_band_of_the_picture_still_reads():
    picture = blend(frame(1001), frame(1002), 0.5)
    height = picture.shape[0]
    for top in range(0, height - height // 4 + 1, 3):
        band = picture[top : top + height // 4]
        reading = wf.read_frame(band)
        assert reading.ok, (top, reading)
        assert (reading.index, reading.next_index) == (1001, 1002)
        assert abs(reading.weight_q8 - 128) <= 1
        assert reading.strips >= 1


def test_snv_s_band_scaled_by_four_with_noise_reads():
    # SNV's wall: ~270 of 1080 source rows, x4, on a 7680 x 1080 output.
    picture = blend(frame(1499, 25, 1920, 1080), frame(1500, 25, 1920, 1080), 0.664)
    band = picture[405:675]
    wall = np.repeat(np.repeat(band, 4, axis=0), 4, axis=1)
    assert wall.shape == (1080, 7680)
    reading = wf.read_frame(noisy(wall, 6.0, seed=272))
    assert reading.ok, reading
    assert (reading.index, reading.next_index) == (1499, 1500)
    assert abs(reading.weight_q8 - 170) <= 2


def test_a_grey_capture_in_colour_reads_the_same():
    picture = blend(frame(42), frame(43), 0.25)
    rgb = np.stack([picture] * 3, axis=2)
    bgra = np.dstack([picture, picture, picture, np.full_like(picture, 255)])
    single = picture[:, :, None]
    expected = wf.read_frame(picture)
    assert expected.ok and expected.index == 42
    for capture in (rgb, bgra, single):
        assert wf.read_frame(capture) == expected


def test_a_clip_spanning_part_of_the_capture_reads_with_its_x_range():
    picture = frame(321)
    side = np.full((picture.shape[0], 480), clip.Y_BACKGROUND, dtype=np.uint8)
    capture = np.hstack([side, picture, side])
    reading = wf.read_frame(capture, x_range=(480 / 1920, 1440 / 1920))
    assert (reading.status, reading.index) == ("ok", 321)
    assert not wf.read_frame(capture).ok, "read on the wrong columns"


def test_a_picture_with_no_strip_is_named():
    blank = np.full((540, 960), clip.Y_BACKGROUND, dtype=np.uint8)
    assert wf.read_frame(blank) == wf.Reading("no_strip")
    white = np.full((540, 960), 255, dtype=np.uint8)
    assert wf.read_frame(white).status == "no_strip"


def test_two_grey_cells_are_named_never_guessed():
    # 0 and 2 differ in two cells: a blend that skipped frame 1.
    assert bin(clip.gray(0) ^ clip.gray(2)).count("1") == 2
    reading = wf.read_frame(blend(frame(0), frame(2), 0.5))
    assert reading.status == "multi" and reading.index is None


def test_one_grey_cell_between_codes_that_are_not_neighbours_is_named():
    far = clip.gray_inverse(1 << 15)
    assert far == 65_535
    reading = wf.read_frame(blend(frame(0), frame(far), 0.5))
    assert reading.status == "non_adjacent" and reading.index is None


def test_a_torn_picture_disagrees():
    torn = frame(100).copy()
    torn[270:] = frame(300)[270:]
    reading = wf.read_frame(torn)
    assert reading.status == "disagree" and reading.strips == clip.BANDS


def test_decode_levels_reads_plain_and_blended_strips():
    bits = np.array(clip.code_bits(9), dtype=float)
    assert wf.decode_levels(bits) == wf.Reading("ok", 9, None, 0, 1)
    k = int(np.flatnonzero(bits != np.array(clip.code_bits(10)))[0])
    blended = bits.copy()
    blended[k] = 0.25 if bits[k] == 0 else 0.75
    assert wf.decode_levels(blended) == wf.Reading("ok", 9, 10, 64, 1)
    near_white = bits.copy()
    near_white[k] = 0.97 if bits[k] == 0 else 0.03
    assert wf.decode_levels(near_white) == wf.Reading("ok", 10, None, 0, 1)
    # The weight is rounded to Q8, not cut: 0.3 x 256 = 76.8 -> 77.
    blended[k] = 0.3 if bits[k] == 0 else 0.7
    assert wf.decode_levels(blended).weight_q8 == 77


def test_luma_is_the_mean_of_the_colour_channels():
    rgb = np.zeros((2, 2, 3), dtype=np.uint8)
    rgb[:, :] = (30, 60, 90)
    assert (wf.to_luma(rgb) == 60).all()
    bgra = np.dstack([rgb, np.full((2, 2), 255, dtype=np.uint8)])
    assert (wf.to_luma(bgra) == 60).all(), "alpha is not luma"
    assert wf.to_luma(rgb[:, :, :1]).shape == (2, 2)
    with pytest.raises(ValueError):
        wf.to_luma(np.zeros(4))


def strip_cell(picture: np.ndarray, k: int, value: int) -> np.ndarray:
    """``picture`` with cell ``k`` of every strip painted ``value``."""
    lay = clip.layout(picture.shape[1], picture.shape[0])
    out = picture.copy()
    for y0, y1 in lay.strips:
        out[y0:y1, lay.cell_edges[k] : lay.cell_edges[k + 1]] = value
    return out


def test_reference_cells_that_disagree_are_not_a_strip():
    # Every bit white (43690) or black (0), so only the reference check can
    # tell: the right black reference grey, or the right white one grey.
    assert clip.code_bits(43_690) == [1] * 16
    assert wf.read_frame(frame(43_690)).index == 43_690
    assert wf.read_frame(strip_cell(frame(43_690), 19, 120)).status == "no_strip"
    assert wf.read_frame(frame(0)).index == 0
    assert wf.read_frame(strip_cell(frame(0), 18, 120)).status == "no_strip"


def test_a_washed_out_picture_is_not_read():
    faded = (100 + frame(500).astype(np.float64) * 0.1).astype(np.uint8)
    assert wf.read_frame(faded).status == "no_strip"


def test_the_pacer_s_blend_rule():
    assert wf.expected_weights(25) == [0, 213, 170, 128, 85, 42]
    assert wf.expected_weights(24) == [0, 204, 153, 102, 51]
    assert wf.expected_weights(30) == [0]
    assert wf.expected_weights(15) == [0, 0], "a 2-slot hold, never blended"
    assert wf.expected_weights(50) == [0, 0, 0]
    assert wf.cycle_length(25) == 6 and wf.cycle_length(24) == 5
    # Inside the 2 ms dead band after the shown frame: plain.
    assert wf.blend_q8(Fraction(1, 25), 25) == 0
    assert wf.blend_q8(Fraction(1, 10), 25) == 25
    # From another phase.
    assert wf.expected_weights(25, phase=Fraction(1, 2)) == [128, 85, 42, 0, 213, 170]


def reading_at(position: Fraction, fps: int, frames: int) -> wf.Reading:
    """What the reader reads of the pacer's boundary at clip ``position``:
    the strip's levels blended by the pacer's weight, read by the reader's
    own rule (a weight within its level edge of 0 or 256 reads plain)."""
    index = int(position)
    q8 = wf.blend_q8(position, fps)
    if index + 1 >= frames:
        q8 = 0  # the song's end: no next frame to blend
    levels = np.array(clip.code_bits(index), dtype=float)
    if q8:
        later = np.array(clip.code_bits(index + 1), dtype=float)
        levels = levels + (later - levels) * (q8 / 256)
    return dataclasses.replace(wf.decode_levels(levels), strips=8)


def pacer_readings(
    fps: int, start: Fraction, boundaries: int, frames: int, refreshes: int = 2
) -> list[wf.Reading]:
    """What a capture of the pacer's output reads: one reading per refresh,
    ``refreshes`` per boundary, from clip position ``start``. The clip loops
    after ``frames`` frames; the loop's new play starts on a boundary."""
    out = []
    position = start
    step = Fraction(fps, wf.GRID_FPS)
    for _ in range(boundaries):
        out.extend([reading_at(position, fps, frames)] * refreshes)
        position += step
        if position >= frames:
            position = Fraction(0)
    return out


def test_a_clean_25_on_30_capture_through_a_loop_is_clean():
    readings = pacer_readings(25, Fraction(40), 60, frames=60)
    report = wf.analyse(readings, 25, frames=60)
    assert report["samples"] == report["decoded"] == 120
    assert report["boundaries"] == 60
    assert report["refreshes_per_boundary"] == {"2": 60}
    assert report["loops"] == 1
    assert report["lost_frames"] == 0
    assert report["lost_boundaries"] == 0
    assert report["jumps_back"] == 0
    assert report["cadence_errors"] == 0
    weights = report["weights"]
    assert weights["observed_first_cycle"] == [0, 213, 170, 128, 85, 42]
    assert weights["expected_cycle_fitted"] == [0, 213, 170, 128, 85, 42]
    assert weights["phase_frames"] == [pytest.approx(0, abs=0.002)] * 2
    assert weights["mismatches"] == 0
    # At the seam: the song's last boundary holds frame 59 (no next frame
    # to blend: a short step, and the 42 it would show), then the new play
    # starts frame 0 one step later (a 1-frame step on a 5/6 grid).
    assert report["loop_seams"] == {"steps": 3, "errors": 3}
    assert report["verdict"] == "clean"


def test_a_clean_30_on_30_capture_counts_every_frame():
    readings = pacer_readings(30, Fraction(10), 40, frames=1800, refreshes=2)
    report = wf.analyse(readings, 30)
    assert report["frames_in_clip"] == 1800
    assert report["frames_seen"] == 40
    assert report["verdict"] == "clean"


def test_a_30_on_30_loop_is_a_seamless_wrap():
    readings = pacer_readings(30, Fraction(55), 10, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["frames_seen"] == 10
    assert report["loop_seams"] == {"steps": 3, "errors": 0}
    assert report["verdict"] == "clean"


def test_a_few_wrong_boundaries_do_not_move_the_fitted_phase():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    for n in (3, 9, 15):
        readings[n] = wf.Reading("ok", n, n + 1, 102, 8)
    report = wf.analyse(readings, 30)
    assert report["weights"]["phase_frames"] == [pytest.approx(0, abs=0.003)]
    assert report["weights"]["mismatches"] == 3


def test_a_lost_boundary_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[10] == wf.Reading("ok", 4, 5, 42, 8)
    del readings[10:12]  # that boundary never shown
    report = wf.analyse(readings, 25)
    assert report["lost_boundaries"] == 1
    assert report["lost_frames"] == 0, "its frames show in its neighbours"
    assert report["verdict"] == "defects"


def test_a_lost_frame_is_counted():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    del readings[14:16]  # frame 7
    report = wf.analyse(readings, 30)
    assert report["lost_frames"] == 1 and report["lost_frame_list"] == [7]
    assert report["lost_boundaries"] == 1
    assert report["verdict"] == "defects"


def test_a_jump_back_is_counted_with_its_sample():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    readings[30:32] = [wf.Reading("ok", 3, 4, 85, 8)] * 2
    report = wf.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["jump_back_samples"] == [30]
    assert report["verdict"] == "defects"


def test_a_wrong_weight_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[8] == wf.Reading("ok", 3, 4, 85, 8)
    readings[8:10] = [wf.Reading("ok", 3, 4, 120, 8)] * 2
    report = wf.analyse(readings, 25)
    assert report["weights"]["mismatches"] == 1
    assert report["verdict"] == "defects"


def test_a_repeated_boundary_shows_as_a_longer_run():
    readings = pacer_readings(30, Fraction(0), 10, frames=1800)
    readings[4:4] = readings[4:6]  # boundary 2 shown for 4 refreshes
    report = wf.analyse(readings, 30)
    assert report["refreshes_per_boundary"] == {"2": 9, "4": 1}
    assert report["boundaries"] == 10
    assert report["refreshes"] is None, "no refresh rate, no refresh check"
    assert report["verdict"] == "clean"
    # At 60 Hz every boundary must be shown twice: a 4-refresh one is judder.
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["refreshes"] == {
        "refresh_hz": 60,
        "expected": [2],
        "uneven": 1,
        "uneven_samples": [4],
    }
    assert report["verdict"] == "defects"


def test_undecoded_readings_are_counted_and_do_not_split_a_boundary():
    readings = pacer_readings(30, Fraction(0), 5, frames=1800)
    readings.insert(1, wf.Reading("no_strip"))
    readings.append(wf.Reading("multi", strips=8))
    report = wf.analyse(readings, 30, times=[n / 60 for n in range(len(readings))])
    assert report["undecoded"] == {"no_strip": 1, "multi": 1}
    assert report["boundaries"] == 5
    assert report["capture"]["interval_ms_p50"] == pytest.approx(16.667, abs=0.01)
    assert report["capture"]["gaps"] == 0
    # The refresh rate read from the capture times; the boundary with the
    # unreadable picture inside it and the capture's edges are not judged.
    assert report["refreshes"]["refresh_hz"] == pytest.approx(60)
    assert report["refreshes"]["expected"] == [2]
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "clean"


def test_a_jump_to_the_start_mid_clip_is_a_jump_back_not_a_loop():
    readings = pacer_readings(30, Fraction(900), 5, frames=1800)
    readings += pacer_readings(30, Fraction(0), 5, frames=1800)
    report = wf.analyse(readings, 30)
    assert report["loops"] == 0
    assert report["jumps_back"] == 1


def test_an_empty_capture_is_not_clean():
    report = wf.analyse([wf.Reading("no_strip")] * 3, 25)
    assert report["decoded"] == 0 and report["boundaries"] == 0
    assert report["verdict"] == "defects"


def test_pictures_blended_by_the_rule_read_back_to_the_rule():
    readings = []
    step = Fraction(25, 30)
    for k in range(13):
        position = k * step
        index = int(position)
        q8 = wf.blend_q8(position, 25)
        picture = blend(
            frame(index, 25, 480, 270), frame(index + 1, 25, 480, 270), q8 / 256
        )
        readings.append(wf.read_frame(picture))
    report = wf.analyse(readings, 25)
    assert report["boundaries"] == 13
    observed = report["weights"]["observed_first_cycle"]
    for got, want in zip(observed, [0, 213, 170, 128, 85, 42]):
        assert abs(got - want) <= 2, observed
    assert report["verdict"] == "clean", report


def test_main_reads_a_video_and_exits_by_the_verdict(monkeypatch, capsys):
    clean = [frame(n, 30, 480, 270) for n in range(6)]
    pictures = {"clean.mp4": clean, "lost.mp4": clean[:2] + clean[3:]}
    pictures["blank.mp4"] = [np.zeros((270, 480), dtype=np.uint8)] * 3

    def fake_video(path, width):
        assert width == 480
        return iter(pictures[path.name])

    monkeypatch.setattr(wf, "frames_from_video", fake_video)
    args = ["--fps", "30", "--grid-fps", "30", "--width", "480", "--video"]
    assert wf.main([*args, "clean.mp4"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["boundaries"] == 6 and report["verdict"] == "clean"
    assert wf.main([*args, "lost.mp4"]) == 1
    assert json.loads(capsys.readouterr().out)["lost_frame_list"] == [2]
    assert wf.main([*args, "blank.mp4"]) == 2
    assert json.loads(capsys.readouterr().out)["decoded"] == 0


def test_every_grid_phase_of_a_clean_capture_is_clean():
    # Review round 1: at a phase inside the pacer's dead band (0.03-0.05 of
    # a frame) the plain boundary read as a cadence error. Every phase, in
    # hundredths of a frame, of the pacer's exact output reads clean.
    for fps in (24, 25, 30):
        for k in range(100):
            start = 100 + Fraction(k, 100)
            readings = pacer_readings(fps, start, 40, frames=1500)
            report = wf.analyse(readings, fps, refresh_hz=60)
            failed = {
                key: report[key]
                for key in ("lost_frames", "lost_boundaries", "cadence_errors")
                if report[key]
            }
            assert not failed, (fps, k, failed)
            assert report["weights"]["mismatches"] == 0, (fps, k, report["weights"])
            assert report["refreshes"]["uneven"] == 0, (fps, k)
            assert report["verdict"] == "clean", (fps, k)


def test_a_step_across_an_unreadable_picture_is_unverified_not_lost():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    readings[14:16] = [wf.Reading("no_strip")] * 2  # boundary 7 unreadable
    report = wf.analyse(readings, 30)
    assert report["unverified"] == {"steps": 1, "frames": 1}
    assert report["lost_frames"] == 0 and report["lost_boundaries"] == 0
    assert report["undecoded"] == {"no_strip": 2}
    assert report["verdict"] == "clean"


def test_a_step_across_a_capture_gap_is_unverified_not_lost():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    times = [n / 60 for n in range(len(readings))]
    del readings[14:16]  # boundary 7 never captured...
    del times[14:16]  # ...because the capture missed those refreshes
    report = wf.analyse(readings, 30, times=times)
    assert report["capture"]["gaps"] == 1
    assert report["unverified"] == {"steps": 1, "frames": 1}
    assert report["lost_frames"] == 0 and report["lost_boundaries"] == 0
    assert report["refreshes"]["uneven"] == 0, "the runs beside the gap"
    assert report["verdict"] == "clean"
    # The same boundary missing with no capture gap: the wall lost it.
    report = wf.analyse(readings, 30, times=[n / 60 for n in range(len(readings))])
    assert report["lost_frames"] == 1 and report["lost_boundaries"] == 1
    assert report["unverified"] == {"steps": 0, "frames": 0}


def test_uneven_refreshes_are_judder():
    # Every frame shown, in order, but 3 / 1 refreshes instead of 2 / 2.
    readings = []
    for b in range(20):
        readings += [reading_at(Fraction(b), 30, 1800)] * (3 if b % 2 == 0 else 1)
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["lost_frames"] == report["cadence_errors"] == 0
    assert report["refreshes_per_boundary"] == {"1": 10, "3": 10}
    assert report["refreshes"]["uneven"] == 18, "all but the capture's edges"
    assert report["refreshes"]["uneven_samples"][:3] == [3, 4, 7]
    assert report["verdict"] == "defects"
    # A capture rate the grid does not divide allows both counts around it.
    report = wf.analyse(readings, 30, refresh_hz=50)
    assert report["refreshes"]["expected"] == [1, 2]
    assert report["refreshes"]["uneven"] == 9, "the 3-refresh ones inside"


def test_the_reader_s_dead_band_is_the_pacer_s():
    rust = Path(__file__).resolve().parents[2] / "crates" / "sp-server" / "src"
    source = (rust / "playback" / "pacer_blend.rs").read_text()
    found = re.search(r"pub const BLEND_DEAD_BAND_100NS: i64 = ([0-9_]+);", source)
    assert found, "the pacer's dead band constant"
    assert wf.DEAD_BAND_S == Fraction(int(found.group(1).replace("_", "")), 10**7)


def test_a_weight_at_the_reader_s_edge_is_read_as_the_plain_frame():
    assert wf.shown_position(Fraction(500, 100), 25) == 5
    assert wf.shown_position(Fraction(501, 100), 25) == 5, "the dead band"
    assert wf.shown_position(Fraction(550, 100), 25) == Fraction(11, 2)
    assert wf.shown_position(Fraction(597, 100), 25) == 6, "248 of 256 reads 6"
    assert wf.blend_q8(Fraction(597, 100), 25) == 248
    assert wf.shown_position(Fraction(595, 100), 25) == 5 + Fraction(243, 256)
    assert wf.shown_position(Fraction(570, 100), 30) == 5, "30 fps never blends"


def test_the_sampled_columns_sit_inside_each_cell():
    for width, x_range in ((3840, (0.0, 1.0)), (7680, (0.25, 0.75))):
        columns = wf.cell_columns(width, x_range)
        assert columns.shape == (clip.CELLS * wf.CELL_SAMPLES,)
        x0 = x_range[0] * width
        span = (x_range[1] - x_range[0]) * width
        edges = clip.cell_fractions()
        for k in range(clip.CELLS):
            cell = columns[k * wf.CELL_SAMPLES : (k + 1) * wf.CELL_SAMPLES]
            left = x0 + span * edges[k]
            right = x0 + span * edges[k + 1]
            quarter = (right - left) / 4
            assert (cell >= left + quarter - 1).all(), (width, k)
            assert (cell <= right - quarter).all(), (width, k)
            assert len(set(cell.tolist())) == wf.CELL_SAMPLES, (width, k)
    with pytest.raises(ValueError):
        wf.cell_columns(960, (0.5, 1.5))


def test_a_video_is_read_frame_by_frame_as_stored():
    args = wf.video_read_args("ffmpeg", Path("rec.mp4"), 960, 540)
    assert args[0] == "ffmpeg" and args[args.index("-i") + 1] == "rec.mp4"
    assert args[args.index("-fps_mode") + 1] == "passthrough"
    assert args[args.index("-vf") + 1].startswith("scale=960:540")
    assert args[args.index("-pix_fmt") + 1] == "gray"
    assert args[-1] == "pipe:1"
    assert not any("ffprobe" in a for a in args)
