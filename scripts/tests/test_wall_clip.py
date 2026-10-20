"""Tests for scripts/wall_clip.py, SongPlayer's wall measurement clips (#272).

The clips' frames are pure functions of the index, so the drawing is tested
here directly (numpy only, as in CI's eval env). Cases:

* the reflected Gray code: a round trip, and ONE bit between neighbours
  (a blended boundary then shows exactly one grey cell);
* the layout at 4K: 20 px cells, nine copies per band, eight bands; ANY
  quarter of the width (the operator's ~2x zoom shows half of it, #272 v2)
  holds a whole copy at every offset, and ANY quarter of the height a whole
  band (its ruler right above its strip);
* every supported width is the 4K layout scaled (the tests read 960 and
  1920 px frames for the 4K clip);
* a frame carries its index in every copy of every band, each copy's
  references, ruler and digits, and two consecutive frames' strips differ
  in one cell of each copy only; the ruler never changes, and no strip
  draws it (no run of 18 black cells between white ones);
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


def test_the_4k_layout_has_nine_copies_of_20_px_cells_in_eight_bands():
    lay = clip.layout(clip.WIDTH, clip.HEIGHT)
    assert clip.CLIP_VERSION == 2
    assert (lay.cell, clip.CELLS, clip.COPIES, clip.BANDS) == (20, 20, 9, 8)
    assert lay.copies == tuple(120 + 400 * k for k in range(9))
    for copy in range(clip.COPIES):
        edges = lay.cell_edges(copy)
        assert len(edges) == clip.CELLS + 1
        assert {b - a for a, b in zip(edges[:-1], edges[1:])} == {20}
    assert lay.cell_edges(0)[0] == 120 and lay.cell_edges(8)[-1] == 3720
    assert len(lay.strips) == len(lay.rulers) == len(lay.digit_rows) == 8
    for digits, ruler, strip in zip(lay.digit_rows, lay.rulers, lay.strips):
        assert digits[1] < ruler[0] and ruler[1] < strip[0], "digits, ruler, strip"
        assert ruler[1] - ruler[0] >= 20 and strip[1] - strip[0] >= 60
        # The strip starts under its ruler within half the ruler's height,
        # the gap the reader pairs them by (wall_frames.ruled_strip).
        assert strip[0] - ruler[1] <= (ruler[1] - ruler[0]) // 2 + 1
    assert lay.rulers[0] == (108, 129) and lay.strips[0] == (140, 205)


def test_any_quarter_of_the_width_holds_a_whole_copy_at_any_offset():
    lay = clip.layout(clip.WIDTH, clip.HEIGHT)
    quarter = clip.WIDTH // 4
    spare = []
    for left in range(0, clip.WIDTH - quarter + 1):
        right = left + quarter
        whole = [
            right - lay.cell_edges(k)[-1]
            for k in range(clip.COPIES)
            if left <= lay.cell_edges(k)[0] and lay.cell_edges(k)[-1] <= right
        ]
        assert whole, f"columns [{left}, {right}) hold no whole copy"
        spare.append(max(whole))
    assert min(spare) == 161, "8 cells to spare at the worst offset"
    # Two copies' width minus a pixel: the worst offset of a narrower window.
    narrow = 2 * clip.CELLS * lay.cell - 1
    for left in range(lay.copies[0], lay.copies[-1] - narrow):
        assert any(
            left <= lay.cell_edges(k)[0] and lay.cell_edges(k)[-1] <= left + narrow
            for k in range(clip.COPIES)
        ), left


def test_any_quarter_of_the_height_holds_a_whole_band():
    lay = clip.layout(clip.WIDTH, clip.HEIGHT)
    quarter = clip.HEIGHT // 4
    for top in range(0, clip.HEIGHT - quarter + 1):
        bottom = top + quarter
        whole = [
            band
            for band in range(clip.BANDS)
            if top <= lay.rulers[band][0] and lay.strips[band][1] <= bottom
        ]
        assert whole, f"rows [{top}, {bottom}) hold no whole ruler and strip"


def test_every_supported_width_is_the_4k_layout_scaled():
    four_k = clip.layout(clip.WIDTH, clip.HEIGHT)
    for width in (960, 1920, 3840, 7680):
        lay = clip.layout(width, width * 9 // 16)
        assert lay.cell * clip.CELL_DIV == width
        assert [x / width for x in lay.copies] == pytest.approx(
            [x / clip.WIDTH for x in four_k.copies], abs=1e-12
        )
    with pytest.raises(ValueError, match="no room"):
        clip.layout(191, 108)
    assert clip.layout(192, 108).cell == 1


def cell_value(plane: np.ndarray, lay: clip.Layout, rows, copy: int, k: int) -> int:
    """The luma at the centre of cell ``k`` of ``copy`` in the middle row of
    ``rows``."""
    y = (rows[0] + rows[1]) // 2
    edges = lay.cell_edges(copy)
    return int(plane[y, (edges[k] + edges[k + 1]) // 2])


def test_a_frame_carries_its_index_in_every_copy_of_every_band():
    lay = clip.layout(960, 540)
    plane = clip.draw_frame(1234, 25, lay)
    assert plane.shape == (540, 960) and plane.dtype == np.uint8
    bits = clip.code_bits(1234)
    expected = [clip.Y_WHITE if b else clip.Y_BLACK for b in bits]
    black, white = clip.Y_BLACK, clip.Y_WHITE
    for rows, ruler in zip(lay.strips, lay.rulers):
        for copy in range(clip.COPIES):
            refs = [cell_value(plane, lay, rows, copy, k) for k in (0, 1, 18, 19)]
            assert refs == [black, white, white, black]
            cells = [cell_value(plane, lay, rows, copy, 2 + k) for k in range(16)]
            assert cells == expected
            marks = [cell_value(plane, lay, ruler, copy, k) for k in range(20)]
            assert marks == [white] + [black] * 18 + [white], "the ruler"
        # Outside the copies, left and right: the background.
        for y in ((rows[0] + rows[1]) // 2, (ruler[0] + ruler[1]) // 2):
            assert plane[y, lay.copies[0] - 1] == clip.Y_BACKGROUND
            assert plane[y, lay.cell_edges(8)[-1]] == clip.Y_BACKGROUND
    # Above the first band's digits, and between a ruler and its strip.
    assert (plane[: lay.digit_rows[0][0]] == clip.Y_BACKGROUND).all()
    assert (plane[lay.rulers[0][1] : lay.strips[0][0]] == clip.Y_BACKGROUND).all()


def test_the_digits_sit_inside_each_copy_s_bit_cells():
    assert clip.label(37, 25) == "0037  25 FPS"
    lay = clip.layout(960, 540)
    plane = clip.draw_frame(37, 25, lay)
    for y0, y1 in lay.digit_rows:
        rows = plane[y0:y1]
        inside = np.zeros(lay.width, dtype=bool)
        for copy in range(clip.COPIES):
            edges = lay.cell_edges(copy)
            inner = rows[:, edges[clip.REF_CELLS] : edges[-clip.REF_CELLS - 1]]
            assert (inner == clip.Y_DIGIT).any(), f"copy {copy}'s digits"
            inside[edges[clip.REF_CELLS] : edges[-clip.REF_CELLS - 1]] = True
        assert (rows[:, ~inside] == clip.Y_BACKGROUND).all()
    mask = clip.text_mask("0037", 2)
    assert mask.shape == (clip.FONT_H * 2, (4 * clip.FONT_W + 3) * 2)
    # The 4K clip's digits: 4 px font pixels (the label fits 16 cells).
    big = clip.layout(clip.WIDTH, clip.HEIGHT)
    y0, y1 = big.digit_rows[0]
    drawn = np.flatnonzero((clip.draw_frame(37, 25, big)[y0:y1] == clip.Y_DIGIT).any(1))
    assert drawn[-1] - drawn[0] + 1 == clip.FONT_H * 4


def test_a_label_that_does_not_fit_a_copy_is_refused():
    with pytest.raises(ValueError, match="does not fit"):
        clip.draw_frame(37, 25, clip.layout(480, 270))
    # One font pixel high is enough: 71 units fit 16 cells of 5 px.
    clip.draw_frame(37, 25, clip.layout(960, 270))


def test_consecutive_frames_differ_in_one_strip_cell_of_each_copy():
    lay = clip.layout(960, 540)
    base = clip.static_frame(lay)
    for index in (0, 1, 7, 255, 1023, 1498, 1798):
        a = clip.draw_frame(index, 30, lay, base)
        b = clip.draw_frame(index + 1, 30, lay, base)
        for y0, y1 in lay.strips:
            columns = set(np.flatnonzero((a[y0:y1] != b[y0:y1]).any(axis=0)))
            for copy in range(clip.COPIES):
                edges = lay.cell_edges(copy)
                cells = {
                    k
                    for k in range(clip.CELLS)
                    if columns & set(range(edges[k], edges[k + 1]))
                }
                assert len(cells) == 1, (index, copy, cells)
                assert cells <= set(range(clip.REF_CELLS, clip.CELLS - clip.REF_CELLS))
        for y0, y1 in lay.rulers:
            assert np.array_equal(a[y0:y1], b[y0:y1]), "the ruler never changes"


def test_no_strip_draws_a_ruler():
    # The reader places a copy only by a ruler: 18 dark cells between two
    # white ones. A strip row's longest dark run between white cells is its
    # 16 bit cells, for every 16-bit code.
    for index in range(1 << clip.BITS):
        cells = [0, 1] + clip.code_bits(index) + [1, 0]
        whites = [k for k, c in enumerate(cells) if c]
        gaps = [b - a - 1 for a, b in zip(whites[:-1], whites[1:])]
        assert max(gaps, default=0) <= clip.BITS, index


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
    assert clip.clip_name(25) == "wall-clip-v2-25fps-60s.mp4"
    assert clip.clip_name(30, 2) == "wall-clip-v2-30fps-2s.mp4"
    assert clip.frame_count(24, 60) == 1440
    assert clip.frame_count(25, 60) == 1500
    assert clip.frame_count(30, 60) == 1800


def test_a_size_must_keep_the_cell_fractions():
    assert clip.parse_size("3840x2160") == (3840, 2160)
    assert clip.parse_size("1920X1080") == (1920, 1080)
    for bad in ("1000x500", "1920x1081", "0x0"):
        with pytest.raises(argparse.ArgumentTypeError):
            clip.parse_size(bad)


def fake_ffmpeg(directory: Path, body: str) -> str:
    """A stand-in ffmpeg: a shell script that gets ffmpeg's arguments (the
    output file last) and the raw frames on stdin."""
    script = directory / "ffmpeg"
    script.write_text("#!/bin/sh\nfor last; do :; done\n" + body)
    script.chmod(0o755)
    return str(script)


def test_a_clip_is_renamed_into_place_once_ffmpeg_succeeded(tmp_path):
    ffmpeg = fake_ffmpeg(tmp_path, 'wc -c > "$last"\n')
    out = tmp_path / "clip.mp4"
    clip.write_clip(30, 960, 540, 1, out, ffmpeg)
    frame_bytes = 960 * 540 * 3 // 2
    assert int(out.read_text()) == 30 * frame_bytes, "every frame, Y + U + V"
    assert not (tmp_path / "clip.mp4.partial").exists()


def test_a_failed_encode_leaves_no_partial_file(tmp_path):
    ffmpeg = fake_ffmpeg(
        tmp_path, 'cat > /dev/null\necho half > "$last"\necho boom >&2\nexit 3\n'
    )
    out = tmp_path / "clip.mp4"
    with pytest.raises(RuntimeError, match="ffmpeg exited 3: boom"):
        clip.write_clip(30, 960, 540, 1, out, ffmpeg)
    assert not out.exists()
    assert not (tmp_path / "clip.mp4.partial").exists()


def test_an_ffmpeg_that_stops_reading_names_why_and_leaves_nothing(tmp_path):
    body = 'echo half > "$last"\necho "Unknown encoder libx264" >&2\nexit 1\n'
    ffmpeg = fake_ffmpeg(tmp_path, body)
    out = tmp_path / "clip.mp4"
    stopped = "ffmpeg stopped reading frames: Unknown encoder libx264"
    with pytest.raises(RuntimeError, match=stopped):
        clip.write_clip(30, 960, 540, 1, out, ffmpeg)
    assert not out.exists()
    assert not (tmp_path / "clip.mp4.partial").exists()
