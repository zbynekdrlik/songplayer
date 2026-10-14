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
* the pacer's output blended by the program's own 8-bit arithmetic reads
  clean at every grid phase (512 per frame) at 24 / 25 / 30 fps;
* ``analyse``: a clean 25-on-30 run through a loop of the clip, then a lost
  boundary, a lost frame, a jump back, a wrong weight, judder and a stall,
  each counted where it belongs; what the capture did not see (an unreadable
  picture, a blind span) excuses at most one boundary per unseen refresh,
  and such a capture, or one that read too little, is incomplete;
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
    # Half a frame apart (a blend over a plain picture) is torn too.
    torn = frame(100).copy()
    torn[270:] = blend(frame(100), frame(101), 0.5)[270:]
    assert wf.read_frame(torn).status == "disagree"


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


def test_the_reader_s_dead_band_is_the_pacer_s():
    rust = Path(__file__).resolve().parents[2] / "crates" / "sp-server" / "src"
    source = (rust / "playback" / "pacer_blend.rs").read_text()
    found = re.search(r"pub const BLEND_DEAD_BAND_100NS: i64 = ([0-9_]+);", source)
    assert found, "the pacer's dead band constant"
    assert wf.DEAD_BAND_S == Fraction(int(found.group(1).replace("_", "")), 10**7)


def test_a_step_s_slack_is_the_dead_band_or_the_level_edge():
    q8 = wf.Q8_TOLERANCE / 256
    assert wf.step_tolerance(25) == pytest.approx(q8 + 0.05), "2 ms x 25"
    assert wf.step_tolerance(24) == pytest.approx(q8 + 0.048)
    assert wf.step_tolerance(15) == pytest.approx(q8 + wf.LEVEL_EDGE), "0.03 < edge"
    assert wf.SAME_PICTURE == pytest.approx((8 + 0.04 * 256) / 256)


def test_where_the_reader_may_read_a_boundary():
    def at(frames: int, hundredths: int, fps: int = 25) -> list[Fraction]:
        return wf.readable_positions(Fraction(frames * 100 + hundredths, 100), fps)

    assert at(5, 0) == [5]
    assert at(5, 1) == [5], "the dead band"
    assert at(5, 50) == [Fraction(11, 2)]
    assert at(5, 95) == [5 + Fraction(243, 256), 6], "243: inside the edge band"
    assert at(5, 92) == [5 + Fraction(235, 256)], "235: outside it"
    assert at(5, 6) == [5 + Fraction(15, 256), 5], "15: inside it"
    assert at(5, 70, fps=30) == [5], "30 fps never blends"


def program_level(shown: int, later: int, q8: int) -> float:
    """A strip cell's level after the program's byte blend (``nv12_mix.rs``:
    ``(f·(256 − w) + t·w + 128) >> 8``) of black / white luma."""
    f = clip.Y_WHITE if shown else clip.Y_BLACK
    t = clip.Y_WHITE if later else clip.Y_BLACK
    y = (f * (256 - q8) + t * q8 + 128) >> 8
    return (y - clip.Y_BLACK) / (clip.Y_WHITE - clip.Y_BLACK)


def reading_at(position: Fraction, fps: int, frames: int) -> wf.Reading:
    """What the reader reads of the pacer's boundary at clip ``position``:
    the strip blended by the program's own 8-bit arithmetic at the pacer's
    weight, read by the reader's level rule."""
    index = int(position)
    q8 = wf.blend_q8(position, fps)
    if index + 1 >= frames:
        q8 = 0  # the song's end: no next frame to blend
    shown = clip.code_bits(index)
    later = clip.code_bits(index + 1) if q8 else shown
    levels = np.array([program_level(a, b, q8) for a, b in zip(shown, later)])
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


def at_60_hz(readings: list[wf.Reading]) -> list[float]:
    return [n / 60 for n in range(len(readings))]


def test_every_grid_phase_of_the_program_s_own_blend_reads_clean():
    # Review rounds 1-2: the pacer's exact output read as cadence errors at
    # phases inside its dead band, then as weight mismatches where the
    # program's 8-bit blend rounds a weight near 256 across the reader's
    # edge. Every phase, in 512ths of a frame, reads clean.
    for fps in (24, 25):
        for k in range(512):
            readings = pacer_readings(fps, 100 + Fraction(k, 512), 30, frames=1500)
            report = wf.analyse(readings, fps, times=at_60_hz(readings))
            assert report["verdict"] == "clean", (fps, k, report)
    for k in range(0, 512, 37):
        readings = pacer_readings(30, 100 + Fraction(k, 512), 30, frames=1500)
        assert wf.analyse(readings, 30, refresh_hz=60)["verdict"] == "clean", k


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
    # The pacer's 170 and 128 come out of the program's byte blend (16 / 235,
    # rounded) as levels the reader reads as 169 and 127: within the Q8
    # tolerance of the rule.
    assert weights["observed_first_cycle"] == [0, 213, 169, 127, 85, 42]
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


def test_a_jump_to_the_start_from_beyond_the_last_frames_is_a_jump_back():
    readings = pacer_readings(30, Fraction(46), 5, frames=60)  # 46..50
    readings += pacer_readings(30, Fraction(0), 5, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 0, "50 is 10 frames before the end: no loop"
    assert report["jumps_back"] == 1


def test_a_few_wrong_boundaries_do_not_move_the_fitted_phase():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    for n in (3, 9, 15):
        readings[n] = wf.Reading("ok", n, n + 1, 102, 8)
    report = wf.analyse(readings, 30)
    assert report["weights"]["phase_frames"] == [pytest.approx(0, abs=0.003)]
    assert report["weights"]["mismatches"] == 3


def test_a_new_play_after_a_jump_back_is_fitted_on_its_own_phase():
    readings = pacer_readings(25, Fraction(300), 18, frames=1500)
    readings += pacer_readings(25, Fraction(100) + Fraction(1, 2), 18, frames=1500)
    report = wf.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["weights"]["phase_frames"] == [
        pytest.approx(0, abs=0.003),
        pytest.approx(0.5, abs=0.003),
    ]
    assert report["weights"]["mismatches"] == 0


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


def test_a_small_step_back_within_the_slack_is_a_cadence_error_not_a_jump():
    readings = pacer_readings(30, Fraction(0), 12, frames=1800)
    # After boundary 11 (11.0), one reading 10.922: 0.078 back, under the
    # 30 fps slack (0.091), over SAME_PICTURE (0.071).
    readings += [wf.Reading("ok", 10, 11, 236, 8)] * 2
    report = wf.analyse(readings, 30)
    assert report["jumps_back"] == 0
    assert report["cadence_errors"] == 1


def test_a_wrong_weight_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[8] == wf.Reading("ok", 3, 4, 85, 8)
    readings[8:10] = [wf.Reading("ok", 3, 4, 120, 8)] * 2
    report = wf.analyse(readings, 25)
    assert report["weights"]["mismatches"] == 1
    assert report["verdict"] == "defects"


def test_a_repeated_boundary_is_judder_at_a_known_refresh_rate():
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


def test_a_wall_holding_a_picture_while_the_capture_polls_is_a_stall():
    # Desktop duplication delivers no new picture while the wall holds one:
    # the capture times jump 200 ms, with every poll on time (no blind span).
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    times = at_60_hz(readings)
    keep = readings[:14] + readings[26:]  # boundaries 7..12 never presented
    kept_times = times[:14] + times[26:]
    report = wf.analyse(keep, 30, times=kept_times)
    assert report["lost_boundaries"] == 6
    assert report["lost_frames"] == 6
    assert report["refreshes"]["uneven"] == 1, "boundary 6 held 200+ ms"
    assert report["unverified"] == {"steps": 0, "boundaries": 0, "frames": 0}
    assert report["verdict"] == "defects"
    # The same gap where the capture itself was blind: unverified, as far as
    # the unseen refreshes reach. Boundary 13's first refresh fell in the
    # gap too: the boundaries on both sides of it are not judged.
    keep = readings[:14] + readings[27:]
    kept_times = times[:14] + times[27:]
    blind = [(kept_times[13], kept_times[14])]
    report = wf.analyse(keep, 30, times=kept_times, blind_spans=blind)
    assert report["lost_boundaries"] == 0 and report["lost_frames"] == 0
    assert report["unverified"] == {"steps": 1, "boundaries": 6, "frames": 6}
    assert report["refreshes"]["uneven"] == 0, "the boundaries beside the gap"
    assert report["capture"]["blind_spans"] == 1
    assert report["verdict"] == "incomplete"


def test_one_unreadable_picture_can_hide_one_boundary_and_no_more():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    # Boundary 7 on screen for one refresh, unreadable: excused.
    one = readings[:14] + [wf.Reading("no_strip")] + readings[16:]
    report = wf.analyse(one, 30)
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_boundaries"] == report["lost_frames"] == 0
    assert report["verdict"] == "incomplete"
    # A 1000-frame skip across that one picture: one boundary excused.
    skip = readings[:14] + [wf.Reading("no_strip")]
    skip += pacer_readings(30, Fraction(1010), 10, frames=1800)
    report = wf.analyse(skip, 30)
    assert report["lost_boundaries"] == 1002
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 2}
    assert report["lost_frames"] == 1001
    assert report["verdict"] == "defects"


def test_an_off_grid_step_is_a_cadence_error_even_across_an_unreadable_picture():
    readings = pacer_readings(25, Fraction(0), 20, frames=1500)
    readings[14:16] = [wf.Reading("no_strip")] * 2
    readings[16:18] = [wf.Reading("ok", 6, 7, 140, 8)] * 2  # 6.547, not 6.667
    report = wf.analyse(readings, 25)
    assert report["cadence_errors"] >= 1
    assert report["verdict"] == "defects"


def test_undecoded_pictures_are_counted_and_lower_the_coverage():
    readings = pacer_readings(30, Fraction(0), 5, frames=1800)
    readings.insert(1, wf.Reading("no_strip"))
    readings.append(wf.Reading("multi", strips=8))
    report = wf.analyse(readings, 30, times=at_60_hz(readings))
    assert report["undecoded"] == {"no_strip": 1, "multi": 1}
    assert report["boundaries"] == 5
    assert report["capture"]["interval_ms_p50"] == pytest.approx(16.667, abs=0.01)
    assert report["capture"]["blind_spans"] == 0
    assert report["refreshes"]["refresh_hz"] == pytest.approx(60)
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete", "10 of 12 pictures read"


def test_a_capture_that_read_too_little_is_incomplete_never_clean():
    readings = pacer_readings(30, Fraction(0), 3, frames=1800)
    readings += [wf.Reading("no_strip")] * 300
    report = wf.analyse(readings, 30)
    assert report["decoded"] == 6
    assert report["lost_frames"] == report["lost_boundaries"] == 0
    assert report["verdict"] == "incomplete"


def test_an_unreadable_picture_inside_a_boundary_is_not_judged():
    readings = pacer_readings(30, Fraction(0), 12, frames=1800)
    # Boundary 5: read, unreadable, read: on screen 3 refreshes, but what the
    # middle one showed is not known.
    readings[10:12] = [readings[10], wf.Reading("no_strip"), readings[11]]
    report = wf.analyse(readings, 30, times=at_60_hz(readings))
    assert report["refreshes"]["uneven"] == 0
    assert report["unverified"]["steps"] == 0
    assert report["verdict"] == "clean", "one unread picture in 25"


def test_the_loop_seam_s_boundaries_are_not_judged_for_judder():
    readings = pacer_readings(30, Fraction(55), 12, frames=60)
    # The wrap is from 59 (boundary 4) to 0 (boundary 5): seam steps 3, 4
    # and 5 lead to boundaries 4, 5 and 6. The song's end holds 59 longer and
    # the new play's second boundary comes late: neither is judged.
    readings[8:10] = readings[8:10] * 3  # 59 for 6 refreshes
    readings[16:18] = readings[16:18] * 2  # 1 (boundary 6) for 4
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60)
    assert report["loops"] == 1
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "clean"
    # Judder on the boundary BEFORE the song's last one is judged.
    readings[6:8] = readings[6:8] * 2  # 58 (boundary 3) for 4
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60)
    assert report["refreshes"]["uneven"] == 1
    assert report["refreshes"]["uneven_samples"] == [6]


def test_a_frame_missing_at_the_loop_seam_is_a_seam_error_not_a_loss():
    readings = pacer_readings(30, Fraction(54), 5, frames=60)  # 54..58
    readings += pacer_readings(30, Fraction(0), 5, frames=60)  # 59 never shown
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["lost_frames"] == 0
    assert report["loop_seams"]["errors"] == 2, "the 2-step wrap + frame 59"
    assert report["verdict"] == "clean"


def test_video_recording_gaps_are_blind_spans():
    times = [0, 1 / 30, 2 / 30, 4 / 30, 5 / 30, 6 / 30]
    assert wf.pts_gaps(times) == [(2 / 30, 4 / 30)]
    assert wf.pts_gaps([0.0]) == []


def test_a_late_poll_is_a_blind_span_and_a_poll_on_time_is_not():
    polls = [0.0, 0.001, 0.002, 0.012, 0.013, 0.014, 0.0245]
    assert wf.late_polls(polls) == [(0.002, 0.012), (0.014, 0.0245)]
    assert wf.late_polls(polls, late=0.0102) == [(0.014, 0.0245)]
    assert wf.late_polls([0.0]) == []


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


def test_the_sampled_columns_sit_inside_each_cell():
    for width, x_range in ((3840, (0.0, 1.0)), (7680, (0.25, 0.75)), (480, (0, 1))):
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
            eighth = (right - left) / 2 / wf.CELL_SAMPLES
            assert (cell >= left + quarter - 1).all(), (width, k)
            assert (cell <= right - quarter).all(), (width, k)
            assert cell[0] <= left + quarter + eighth, (width, k)
            assert cell[-1] >= right - quarter - eighth - 1, (width, k)
    with pytest.raises(ValueError):
        wf.cell_columns(960, (0.5, 1.5))


def test_a_video_is_read_frame_by_frame_as_stored_with_its_pts():
    args = wf.video_read_args("ffmpeg", Path("rec.mp4"), 960, 540)
    assert args[0] == "ffmpeg" and args[args.index("-i") + 1] == "rec.mp4"
    assert args[args.index("-fps_mode") + 1] == "passthrough"
    graph = args[args.index("-vf") + 1]
    assert graph.startswith("showinfo,scale=960:540"), graph
    assert args[args.index("-pix_fmt") + 1] == "gray"
    assert "-v" not in args and "-loglevel" not in args, "showinfo logs at info"
    assert args[-1] == "pipe:1"
    assert not any("ffprobe" in a for a in args)


def test_main_reads_a_video_and_exits_by_the_verdict(monkeypatch, capsys):
    clean = [wf.read_frame(frame(n, 30, 480, 270)) for n in range(6)]
    times = [n / 30 for n in range(6)]
    videos = {
        "clean.mp4": (clean, times),
        "lost.mp4": (clean[:2] + clean[3:], times[:5]),
        "dropped.mp4": (clean[:2] + clean[3:], times[:2] + times[3:]),
        "blank.mp4": ([wf.Reading("no_strip")] * 3, times[:3]),
    }

    def fake_video(path, width, x_range):
        assert width == 480 and x_range == (0.0, 1.0)
        return videos[path.name]

    monkeypatch.setattr(wf, "read_video", fake_video)
    args = ["--fps", "30", "--grid-fps", "30", "--width", "480", "--video"]
    assert wf.main([*args, "clean.mp4"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["boundaries"] == 6 and report["verdict"] == "clean"
    assert report["refreshes"]["refresh_hz"] == pytest.approx(30)
    # A frame missing while the pts run on: the wall lost it.
    assert wf.main([*args, "lost.mp4"]) == 1
    assert json.loads(capsys.readouterr().out)["lost_frame_list"] == [2]
    # A frame missing with its pts: the recorder dropped it (blind span).
    assert wf.main([*args, "dropped.mp4"]) == 3
    assert json.loads(capsys.readouterr().out)["unverified"]["frames"] == 1
    assert wf.main([*args, "blank.mp4"]) == 2
    assert json.loads(capsys.readouterr().out)["decoded"] == 0
    with pytest.raises(SystemExit):
        wf.main(["--fps", "15", "--video", "clean.mp4"])
    capsys.readouterr()
