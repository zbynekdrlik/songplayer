"""Tests for scripts/wall_frames.py, the wall clip reader (#272): reading a
picture, and the pacer's rule it is read against.

Every picture is drawn by ``wall_clip.draw_frame`` (the clips' own code), then
treated the way the chain treats it: a two-frame blend (#251's weights), a
vertical crop (a wall shows a band), the SNV band scaled x4, noise, colour
channels, a clip that spans only part of the capture's width. Cases:

* a clean frame reads its index, a 30/70 blend reads both indices and the
  weight of the later one (whichever way its grey cell flips);
* any quarter-height band, and SNV's x4-scaled band with noise, still read;
  the bands agree on their median;
* the copies are found where the clip sits in the capture (v2: by their
  rulers, ``test_wall_locate.py`` tests the search under any zoom);
* what cannot be read is named, never guessed: no strip, two grey cells, a
  grey cell between codes that are not neighbours, a torn picture;
* the pacer's rule: 25 on 30 = plain, 213, 170, 128, 85, 42; 24 on 30 =
  plain, 204, 153, 102, 51; nothing for 30, 15 or 50 fps; its dead band is
  the pacer's own constant;
* end to end: pictures blended by the rule are read back to the rule.

``test_wall_frames_analyse.py`` tests what a run of readings means.
"""

from __future__ import annotations

import re
from fractions import Fraction
from pathlib import Path

import numpy as np
import pytest

import wall_clip as clip
import wall_analysis as wa
import wall_frames as wf
from wall_fakes import (
    blend,
    flips_up,
    frame,
    noisy,
)


def test_a_clean_frame_reads_its_index_in_every_band():
    reading = wf.read_frame(frame(1234))
    assert reading == wa.Reading("ok", 1234, None, 0, clip.BANDS * clip.COPIES)
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


def test_a_clip_spanning_part_of_the_capture_is_found_where_it_sits():
    picture = frame(321)
    side = np.full((picture.shape[0], 480), clip.Y_BACKGROUND, dtype=np.uint8)
    capture = np.hstack([side, picture, side])
    reading = wf.read_frame(capture)
    assert (reading.status, reading.index) == ("ok", 321)
    lay = clip.layout(960, 540)
    copies = wf.locate(capture)
    assert [c.x0 for c in copies] == [480 + x for x in lay.copies]
    assert {c.cell for c in copies} == {lay.cell}


def test_a_picture_with_no_strip_is_named():
    blank = np.full((540, 960), clip.Y_BACKGROUND, dtype=np.uint8)
    assert wf.read_frame(blank) == wa.Reading("no_strip")
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
    assert reading.status == "disagree"
    assert reading.strips == clip.BANDS * clip.COPIES
    # Half a frame apart (a blend over a plain picture) is torn too.
    torn = frame(100).copy()
    torn[270:] = blend(frame(100), frame(101), 0.5)[270:]
    assert wf.read_frame(torn).status == "disagree"


def test_decode_levels_reads_plain_and_blended_strips():
    bits = np.array(clip.code_bits(9), dtype=float)
    assert wf.decode_levels(bits) == wa.Reading("ok", 9, None, 0, 1)
    k = int(np.flatnonzero(bits != np.array(clip.code_bits(10)))[0])
    blended = bits.copy()
    blended[k] = 0.25 if bits[k] == 0 else 0.75
    assert wf.decode_levels(blended) == wa.Reading("ok", 9, 10, 64, 1)
    near_white = bits.copy()
    near_white[k] = 0.97 if bits[k] == 0 else 0.03
    assert wf.decode_levels(near_white) == wa.Reading("ok", 10, None, 0, 1)
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
    """``picture`` with cell ``k`` of every copy's strip painted ``value``."""
    lay = clip.layout(picture.shape[1], picture.shape[0])
    out = picture.copy()
    for copy in range(clip.COPIES):
        edges = lay.cell_edges(copy)
        for y0, y1 in lay.strips:
            out[y0:y1, edges[k] : edges[k + 1]] = value
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
    assert wa.expected_weights(25) == [0, 213, 170, 128, 85, 42]
    assert wa.expected_weights(24) == [0, 204, 153, 102, 51]
    assert wa.expected_weights(30) == [0]
    assert wa.expected_weights(15) == [0, 0], "a 2-slot hold, never blended"
    assert wa.expected_weights(50) == [0, 0, 0]
    assert wa.cycle_length(25) == 6 and wa.cycle_length(24) == 5
    # Inside the 2 ms dead band after the shown frame: plain.
    assert wa.blend_q8(Fraction(1, 25), 25) == 0
    assert wa.blend_q8(Fraction(1, 10), 25) == 25
    # From another phase.
    assert wa.expected_weights(25, phase=Fraction(1, 2)) == [128, 85, 42, 0, 213, 170]


def test_the_reader_s_dead_band_is_the_pacer_s():
    rust = Path(__file__).resolve().parents[2] / "crates" / "sp-server" / "src"
    source = (rust / "playback" / "pacer_blend.rs").read_text()
    found = re.search(r"pub const BLEND_DEAD_BAND_100NS: i64 = ([0-9_]+);", source)
    assert found, "the pacer's dead band constant"
    assert wa.DEAD_BAND_S == Fraction(int(found.group(1).replace("_", "")), 10**7)


def test_a_step_s_slack_is_the_dead_band_or_the_level_edge():
    q8 = wa.Q8_TOLERANCE / 256
    assert wa.step_tolerance(25) == pytest.approx(q8 + 0.05), "2 ms x 25"
    assert wa.step_tolerance(24) == pytest.approx(q8 + 0.048)
    assert wa.step_tolerance(15) == pytest.approx(q8 + wa.LEVEL_EDGE), "0.03 < edge"
    assert wa.SAME_PICTURE == pytest.approx((8 + 0.04 * 256) / 256)


def test_where_the_reader_may_read_a_boundary():
    def at(frames: int, hundredths: int, fps: int = 25) -> list[Fraction]:
        return wa.readable_positions(Fraction(frames * 100 + hundredths, 100), fps)

    assert at(5, 0) == [5]
    assert at(5, 1) == [5], "the dead band"
    assert at(5, 50) == [Fraction(11, 2)]
    assert at(5, 95) == [5 + Fraction(243, 256), 6], "243: inside the edge band"
    assert at(5, 92) == [5 + Fraction(235, 256)], "235: outside it"
    assert at(5, 6) == [5 + Fraction(15, 256), 5], "15: inside it"
    assert at(5, 70, fps=30) == [5], "30 fps never blends"


def test_pictures_blended_by_the_rule_read_back_to_the_rule():
    readings = []
    step = Fraction(25, 30)
    for k in range(22):
        position = k * step
        index = int(position)
        q8 = wa.blend_q8(position, 25)
        picture = blend(frame(index, 25), frame(index + 1, 25), q8 / 256)
        readings.append(wf.read_frame(picture))
    report = wa.analyse(readings, 25, refresh_hz=30)
    assert report["boundaries"] == 22
    observed = report["weights"]["observed_first_cycle"]
    for got, want in zip(observed, [0, 213, 170, 128, 85, 42]):
        assert abs(got - want) <= 2, observed
    report = wa.analyse(readings, 25, refresh_hz=30, min_boundaries=20)
    assert report["verdict"] == "clean"


def test_the_sampled_columns_sit_inside_each_cell():
    for x0, size in ((120.0, 20.0), (1037.25, 41.5), (3.0, 2.0), (0.4, 7.3)):
        columns = wf.Copy(x0, size, (0, 0)).columns()
        assert columns.shape == (wf.COPY_COLUMNS,) == (clip.CELLS * wf.CELL_SAMPLES,)
        for k in range(clip.CELLS):
            cell = columns[k * wf.CELL_SAMPLES : (k + 1) * wf.CELL_SAMPLES]
            left = x0 + size * k
            right = left + size
            quarter = size / 4
            eighth = size / 2 / wf.CELL_SAMPLES
            assert (cell >= left + quarter - 1).all(), (x0, size, k)
            assert (cell <= right - quarter).all(), (x0, size, k)
            assert cell[0] <= left + quarter + eighth, (x0, size, k)
            assert cell[-1] >= right - quarter - eighth - 1, (x0, size, k)
            assert (np.diff(cell) >= 0).all()
    # Each sample at the middle of its eighth of the core: a 20 px cell from
    # 120 samples 125.625, 126.875, ... 134.375.
    columns = wf.Copy(120.0, 20.0, (0, 0)).columns()
    assert columns[: wf.CELL_SAMPLES].tolist() == [
        125,
        126,
        128,
        129,
        130,
        131,
        133,
        134,
    ]
    assert columns[-wf.CELL_SAMPLES :].tolist() == [
        505,
        506,
        508,
        509,
        510,
        511,
        513,
        514,
    ]


def test_the_bands_agree_on_their_median():
    # Bands read 0.28 (top), 0.30 (middle), 0.32 (bottom) of frame 1001.
    a, b = frame(1001), frame(1002)
    picture = blend(a, b, 0.30)
    picture[:135] = blend(a, b, 0.28)[:135]
    picture[405:] = blend(a, b, 0.32)[405:]
    reading = wf.read_frame(picture)
    assert reading.ok and (reading.index, reading.next_index) == (1001, 1002)
    assert abs(reading.weight_q8 - 77) <= 1, reading
