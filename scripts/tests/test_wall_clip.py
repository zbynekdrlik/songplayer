"""Tests for scripts/wall_clip.py, SongPlayer's wall measurement clips (#272).

The clips' frames are pure functions of the index, so the drawing is tested
here directly (numpy only, as in CI's eval env). Cases:

* the reflected Gray code: a round trip, and ONE bit between neighbours
  (a blended boundary then shows exactly one grey cell);
* the layout at 4K: cells well over 100 px, eight bands, and ANY quarter of
  the height (SNV's wall band) holds a whole strip row;
* the cell edges sit at the same fractions on every supported width (the
  reader samples by fraction);
* a frame carries its index in every band, its reference cells and its
  digits, and two consecutive frames' strips differ in one cell only;
* the encode is pinned: one x264 thread, fixed GOP, bitexact, no metadata.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import numpy as np
import pytest

import wall_clip as clip


def test_the_gray_code_round_trips_and_neighbours_differ_in_one_bit():
    for index in range(0, 5000):
        assert clip.gray_inverse(clip.gray(index)) == index
        flipped = clip.gray(index) ^ clip.gray(index + 1)
        assert bin(flipped).count("1") == 1, index
    for index in (0, 1, 2, 3, 1234, 1799, 40_000, 65_535):
        assert clip.bits_index(clip.code_bits(index)) == index
    assert clip.code_bits(1) == [0] * 15 + [1]
    assert clip.code_bits(2) == [0] * 14 + [1, 1]
    with pytest.raises(ValueError):
        clip.code_bits(1 << 16)
    with pytest.raises(ValueError):
        clip.code_bits(-1)


def test_the_4k_layout_has_big_cells_and_a_strip_in_any_quarter_band():
    lay = clip.layout(clip.WIDTH, clip.HEIGHT)
    edges = lay.cell_edges
    assert len(edges) == clip.CELLS + 1 == 21
    widths = {b - a for a, b in zip(edges[:-1], edges[1:])}
    assert widths == {184}, "equal cells, each well over 100 px at 3840"
    assert edges[0] == 80 and edges[-1] == 3760, "most of the width"
    assert len(lay.strips) == clip.BANDS == 8
    for (y0, y1), (d0, d1) in zip(lay.strips, lay.digit_rows):
        assert y1 - y0 >= 80
        assert y1 <= d0 < d1, "the digits sit under the strip"
    quarter = clip.HEIGHT // 4
    for top in range(0, clip.HEIGHT - quarter + 1):
        whole = [s for s in lay.strips if top <= s[0] and s[1] <= top + quarter]
        assert whole, f"rows [{top}, {top + quarter}) hold no whole strip"


def test_the_cell_edges_are_the_same_fractions_on_every_supported_width():
    fractions = clip.cell_fractions()
    for width in (960, 1920, 3840, 7680):
        edges = clip.layout(width, width * 9 // 16).cell_edges
        assert [e / width for e in edges] == pytest.approx(fractions, abs=1e-12)


def cell_value(plane: np.ndarray, lay: clip.Layout, rows: tuple[int, int], k: int):
    """The luma at the centre of cell ``k`` in the middle row of ``rows``."""
    y = (rows[0] + rows[1]) // 2
    x = (lay.cell_edges[k] + lay.cell_edges[k + 1]) // 2
    return int(plane[y, x])


def test_a_frame_carries_its_index_in_every_band():
    lay = clip.layout(960, 540)
    plane = clip.draw_frame(1234, 25, lay)
    assert plane.shape == (540, 960) and plane.dtype == np.uint8
    bits = clip.code_bits(1234)
    for rows in lay.strips:
        refs = [cell_value(plane, lay, rows, k) for k in (0, 1, 18, 19)]
        assert refs == [clip.Y_BLACK, clip.Y_WHITE, clip.Y_WHITE, clip.Y_BLACK]
        cells = [cell_value(plane, lay, rows, clip.REF_CELLS + k) for k in range(16)]
        expected = [clip.Y_WHITE if b else clip.Y_BLACK for b in bits]
        assert cells == expected
        # Outside the strip, left and right: the background.
        middle = (rows[0] + rows[1]) // 2
        assert plane[middle, 0] == plane[middle, -1] == clip.Y_BACKGROUND
    # Between two bands' rows (above the first strip): the background.
    assert (plane[: lay.strips[0][0]] == clip.Y_BACKGROUND).all()


def test_the_digits_sit_inside_the_strip_s_cells():
    assert clip.label(37, 25) == "0037  25 FPS"
    lay = clip.layout(960, 540)
    plane = clip.draw_frame(37, 25, lay)
    inner0 = lay.cell_edges[clip.REF_CELLS]
    inner1 = lay.cell_edges[clip.CELLS - clip.REF_CELLS]
    for y0, y1 in lay.digit_rows:
        rows = plane[y0:y1]
        assert (rows == clip.Y_DIGIT).any(), "the digits are drawn"
        assert (rows[:, :inner0] == clip.Y_BACKGROUND).all()
        assert (rows[:, inner1:] == clip.Y_BACKGROUND).all()
    mask = clip.text_mask("0037", 2)
    assert mask.shape == (clip.FONT_H * 2, (4 * clip.FONT_W + 3) * 2)


def test_consecutive_frames_differ_in_one_strip_cell():
    lay = clip.layout(960, 540)
    base = clip.static_frame(lay)
    for index in (0, 1, 7, 255, 1023, 1498, 1798):
        a = clip.draw_frame(index, 30, lay, base)
        b = clip.draw_frame(index + 1, 30, lay, base)
        for y0, y1 in lay.strips:
            columns = np.flatnonzero((a[y0:y1] != b[y0:y1]).any(axis=0))
            assert columns.size, index
            cells = {
                k
                for k in range(clip.CELLS)
                if lay.cell_edges[k] <= columns[0] < lay.cell_edges[k + 1]
                or lay.cell_edges[k] <= columns[-1] < lay.cell_edges[k + 1]
            }
            assert len(cells) == 1, (index, cells)
            assert cells <= set(range(clip.REF_CELLS, clip.CELLS - clip.REF_CELLS))


def test_drawing_is_deterministic():
    lay = clip.layout(960, 540)
    base = clip.static_frame(lay)
    first = clip.draw_frame(777, 24, lay)
    assert np.array_equal(first, clip.draw_frame(777, 24, lay))
    assert np.array_equal(first, clip.draw_frame(777, 24, lay, base))
    assert np.array_equal(base, clip.static_frame(lay)), "the base is untouched"


def test_the_encode_is_pinned_and_bitexact():
    out = Path("out") / "clip.mp4"
    args = clip.ffmpeg_args(25, 3840, 2160, 60, out, "ffmpeg")
    assert args[0] == "ffmpeg" and args[-1] == str(out)

    def value(flag: str) -> str:
        return args[args.index(flag) + 1]

    assert value("-pix_fmt") == "yuv420p"
    assert value("-s") == "3840x2160"
    assert value("-framerate") == "25"
    assert value("-c:v") == "libx264"
    assert value("-threads") == "1"
    assert value("-g") == value("-keyint_min") == "25"
    assert value("-sc_threshold") == "0"
    assert value("-bf") == "0"
    assert value("-crf") == str(clip.X264_CRF)
    assert value("-preset") == clip.X264_PRESET
    assert value("-t") == "60"
    assert value("-fflags") == value("-flags:v") == value("-flags:a") == "+bitexact"
    assert value("-map_metadata") == "-1"
    assert value("-c:a") == "aac"
    assert value("-ar") == "48000" and value("-ac") == "2"
    assert "anullsrc=r=48000:cl=stereo" in args
    maps = [args[n + 1] for n, a in enumerate(args) if a == "-map"]
    assert maps == ["0:v:0", "1:a:0"], "the video and the silent audio, nothing else"


def test_names_and_counts():
    assert clip.clip_name(25) == "wall-clip-v1-25fps-60s.mp4"
    assert clip.clip_name(30, 2) == "wall-clip-v1-30fps-2s.mp4"
    assert clip.frame_count(24, 60) == 1440
    assert clip.frame_count(25, 60) == 1500
    assert clip.frame_count(30, 60) == 1800


def test_a_size_must_keep_the_cell_fractions():
    assert clip.parse_size("3840x2160") == (3840, 2160)
    assert clip.parse_size("1920X1080") == (1920, 1080)
    for bad in ("1000x500", "1920x1081", "0x0"):
        with pytest.raises(argparse.ArgumentTypeError):
            clip.parse_size(bad)
