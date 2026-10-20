"""Tests for the wall reader's search (#272, clip v2): the clip read under the
operator's zoom of its width (SNV, 20.10.2026: ~2x, only the middle half of
the width reached the wall, and the reader read nothing on every deploy;
#272 comment 6281310892), and the pieces that find its copies.

Every picture is drawn by ``wall_clip.draw_frame``, then shown the way a
wall shows it (``wall_view``: zoomed, offset, bilinear like Arena's
scaler), blurred, noised, dimmed. Cases:

* reading: 1x / 1.5x / 2x / 3x of the width at offsets across it; a quarter
  of the width and of the height across every phase of the copies; blur; a
  dimmed clip beside a bright overlay; indices across the whole 30 fps
  clip, whole and 4x scaled, with no copy found where none is;
* a copy is placed by its ruler only: a strip with its ruler covered, an
  index-0 strip (it looks like a ruler of 16/18 of a cell), the digits and a
  copy the capture cuts never place one;
* copies that read different pictures are named, and a capture reads only
  the copies that agree (more than half of them);
* a copy reads only the strips right under its own ruler: another slice
  of the output cut in under its strip is never read for it (review round
  1: a seam 7 rows under the strip read ``disagree`` on a clean wall);
* the pieces, at every bound: ``bright_mask``'s local midpoint, its reach
  and contrast floor; ``ruler_candidates``' bars, cell floor and rows;
  ``group_candidates``; ``ruler_rows``; ``ruled_strips``' gap;
  ``read_sums``; ``capture_region`` (at most ``MAX_COPIES``, the middle
  ones); ``find_clip``'s bounded search, in a given area too.
"""

from __future__ import annotations

import numpy as np
import pytest

import wall_analysis as wa
import wall_clip as clip
import wall_frames as wf
import wall_locate as wl
from wall_fakes import blend, blurred, frame, noisy, wall_view


def big(index: int, fps: int = 25) -> np.ndarray:
    """A 1920 x 1080 frame: the 4K clip halved (10 px cells)."""
    return frame(index, fps, 1920, 1080)


@pytest.mark.parametrize("zoom", [1.0, 1.5, 2.0, 3.0])
def test_the_clip_reads_under_any_zoom_of_its_width(zoom):
    picture = blend(big(1499), big(1500), 0.664)
    shown = 1920 / zoom
    for n, left in enumerate(np.linspace(0, 1920 - shown, 7)):
        top = (n * 37) % 270
        # A quarter of the height, at the wall's 2x.
        view = wall_view(picture, zoom, left, top, 1920, 540, zoom_y=2.0)
        reading = wf.read_frame(noisy(view, 6.0, seed=n))
        assert reading.ok, (zoom, left, top, reading)
        assert (reading.index, reading.next_index) == (1499, 1500)
        assert abs(reading.weight_q8 - 170) <= 3, reading


def test_a_quarter_of_the_width_and_height_reads_across_every_phase():
    # A copy is 200 px at 1920: 131 offsets 11 px apart cover 131 phases.
    picture = big(1234, 30)
    for left in range(0, 1920 - 480 + 1, 11):
        top = (left * 7) % (1080 - 270)
        window = picture[top : top + 270, left : left + 480]
        view = wall_view(window, 2.0, 0, 0, 960, 540)
        reading = wf.read_frame(view)
        assert (reading.status, reading.index) == ("ok", 1234), (left, top, reading)


def test_a_blurred_view_still_reads():
    picture = blend(big(700, 24), big(701, 24), 0.4)
    view = wall_view(picture, 2.0, 600, 400, 1920, 540)
    for radius in (1, 2, 3):
        reading = wf.read_frame(noisy(blurred(view, radius), 4.0, seed=radius))
        assert reading.ok, (radius, reading)
        assert (reading.index, reading.next_index) == (700, 701)
        assert abs(reading.weight_q8 - 102) <= 3, (radius, reading)


def test_a_dimmed_clip_beside_a_bright_overlay_still_reads():
    view = wall_view(big(321, 30), 1.0, 0, 0, 1920, 540)
    dimmed = (view.astype(np.float64) * 0.5).astype(np.uint8)
    dimmed[:, 1500:] = 255  # an overlay over the right of every row
    lay = clip.layout(1920, 1080)
    y = (lay.rulers[0][0] + lay.rulers[0][1]) // 2
    bar = dimmed[y, lay.copies[0] + lay.cell // 2]
    # One midpoint per row (its own lowest and highest level) would take the
    # dimmed bars for dark: the threshold has to be local.
    assert (int(dimmed[y].min()) + int(dimmed[y].max())) / 2 > bar
    assert wl.bright_mask(wl.to_luma(dimmed))[y, lay.copies[0] + lay.cell // 2]
    reading = wf.read_frame(dimmed)
    assert (reading.status, reading.index) == ("ok", 321), reading


def test_indices_across_the_clip_read_with_no_copy_found_where_none_is():
    lay = clip.layout(960, 540)
    for index in range(0, 1800, 17):
        picture = frame(index, 30)
        copies = wl.locate(picture)
        assert [c.x0 for c in copies] == list(lay.copies), index
        assert {c.cell for c in copies} == {lay.cell}, index
        reading = wf.read_sums(wl.cell_sums(picture, copies), copies)
        assert reading == wa.Reading("ok", index, None, 0, 72), index
        assert {c.strips for c in copies} == {lay.strips}, index
    # SNV's band of the 4K clip scaled 4x, nearest (the digits' strokes
    # 8 px wide: at the old 200 they read as a ruler and a strip).
    for index in range(1, 1800, 97):
        band = big(index, 30)[405:675, 480:1440]
        wall = np.repeat(np.repeat(band, 4, axis=0), 4, axis=1)
        reading = wf.read_frame(wall)
        assert (reading.status, reading.index) == ("ok", index), (index, reading)
        assert reading.strips == 6, "3 whole copies (660-1260) in 2 bands"


def test_a_copy_reads_only_the_strips_under_its_own_ruler():
    # Review round 1: another slice of the output (SNV's is made of slices)
    # cut in a few rows under the strip a capture reads put a second strip
    # into the grabbed rows; read for every copy, the picture read
    # `disagree` on a clean wall.
    upper = wall_view(big(777, 30), 2.0, 480, 405, 1920, 540)
    located = wf.find_clip(lambda region: upper, None, 3.0)
    left, top, right, bottom = located.region
    (s0, s1), *_ = located.copies[0].strips
    seam = top + s1 + 7
    # The slice under the seam shows another moment, one copy to the right
    # (its references in the same columns), its strip 2 rows under the seam.
    shift = (seam + 2 - (top + s0)) / 2
    lower = wall_view(big(999, 30), 2.0, 480 + 200, 405 - shift, 1920, 540)
    picture = upper.copy()
    picture[seam:] = lower[seam:]
    grabbed = picture[top:bottom, left:right]
    copies = list(located.copies)
    sums = wl.cell_sums(grabbed, copies)
    runs = wl.strip_rows(sums[:, 0, :] / wl.CELL_SAMPLES)
    assert len(runs) == 2, "the other slice's strip is in the grabbed rows"
    assert wf.read_sums(sums, copies) == wa.Reading("ok", 777, None, 0, len(copies))
    # A whole picture down to there: the other strip is under no ruler of
    # these copies, so it is not read either.
    whole = picture[: seam + 30]
    assert wf.read_frame(whole) == wa.Reading("ok", 777, None, 0, len(wl.locate(whole)))


def test_a_strip_with_its_ruler_covered_is_never_read():
    lay = clip.layout(960, 540)
    picture = frame(1234).copy()
    for y0, y1 in lay.rulers:
        picture[y0:y1] = clip.Y_BACKGROUND
    assert wl.locate(picture) == []
    assert wf.read_frame(picture) == wa.Reading("no_strip")


def test_an_index_0_strip_looks_like_a_ruler_but_places_no_copy():
    lay = clip.layout(960, 540)
    picture = frame(0)
    y0, y1 = lay.strips[0]
    rows, x0, cell = wl.ruler_candidates(wl.bright_mask(wl.to_luma(picture[y0:y1])))
    # Its 16 black bits between the white references: a candidate ruler of
    # 16/18 of a cell, on every strip row.
    assert set(np.round(cell, 6)) == {round(lay.cell * 16 / 18, 6)}
    assert len(set(rows.tolist())) == y1 - y0
    copies = wl.locate(picture)
    assert [c.x0 for c in copies] == list(lay.copies)
    assert wf.read_frame(picture) == wa.Reading("ok", 0, None, 0, 72)


def test_the_digits_never_place_a_copy():
    assert clip.Y_DIGIT - clip.Y_BACKGROUND < wl.CONTRAST_MIN * wl.FULL_SCALE
    lay = clip.layout(1920, 1080)
    y0, y1 = lay.digit_rows[3]
    digits = np.repeat(np.repeat(big(1499)[y0:y1], 4, axis=0), 4, axis=1)
    assert (digits == clip.Y_DIGIT).any()
    assert not wl.bright_mask(wl.to_luma(digits)).any()


def test_a_copy_the_capture_cuts_is_left_out():
    lay = clip.layout(960, 540)
    picture = frame(555)
    end = lay.cell_edges(4)[-1]
    found = [c.x0 for c in wl.locate(picture[:, :end])]
    assert found == list(lay.copies[:5])
    # Cut into copy 4's last cell, its sampled core first: it is left out.
    for cut in (2, 3, lay.cell, 10 * lay.cell):
        found = [c.x0 for c in wl.locate(picture[:, : end - cut])]
        assert found == list(lay.copies[:4]), cut
    start = lay.copies[2]
    found = [c.x0 + start for c in wl.locate(picture[:, start:])]
    assert found == list(lay.copies[2:])
    for cut in (2, 3, lay.cell):
        found = [c.x0 + start + cut for c in wl.locate(picture[:, start + cut :])]
        assert found == list(lay.copies[3:]), cut
    # One pixel off either end leaves every sampled core whole: still read.
    found = [c.x0 + start + 1 for c in wl.locate(picture[:, start + 1 : end - 1])]
    assert found == list(lay.copies[2:5])
    # Wherever the capture cuts, a copy found is read inside it.
    for left in range(0, 2 * clip.CELLS * lay.cell, 3):
        for right in (end - left // 3, end + 1 + left // 3):
            part = picture[:, left:right]
            for copy in wl.locate(part):
                columns = copy.columns()
                assert 0 <= columns[0] and columns[-1] < part.shape[1], (left, right)


def odd_copy(picture: np.ndarray, other: np.ndarray, copies: list[int]) -> np.ndarray:
    """``picture`` with the strips of ``copies`` taken from ``other``."""
    lay = clip.layout(picture.shape[1], picture.shape[0])
    out = picture.copy()
    for copy in copies:
        x0, x1 = lay.cell_edges(copy)[0], lay.cell_edges(copy)[-1]
        for y0, y1 in lay.strips:
            out[y0:y1, x0:x1] = other[y0:y1, x0:x1]
    return out


def test_copies_that_read_different_pictures_disagree():
    lay = clip.layout(960, 540)
    picture = odd_copy(frame(100), frame(300), [4])
    reading = wf.read_frame(picture)
    assert reading.status == "disagree" and reading.strips == 72
    # A capture reads at the eight that agree, never at the odd one.
    copies = wf.clip_copies(picture)
    assert [c.x0 for c in copies] == [x for k, x in enumerate(lay.copies) if k != 4]
    # A copy that reads no picture (two grey cells) does not stop the rest;
    # every strip read is counted.
    picture = odd_copy(frame(100), blend(frame(0), frame(2), 0.5), [4])
    assert wf.read_frame(picture) == wa.Reading("ok", 100, None, 0, 72)


def test_a_capture_reads_only_copies_that_agree_with_more_than_half():
    lay = clip.layout(960, 540)
    assert [c.x0 for c in wf.clip_copies(frame(100))] == list(lay.copies)
    # Five against four: the five.
    picture = odd_copy(frame(100), frame(300), [0, 2, 4, 6])
    found = [c.x0 for c in wf.clip_copies(picture)]
    assert found == [lay.copies[k] for k in (1, 3, 5, 7, 8)]
    # One against one, or four against four: no majority, none.
    two = odd_copy(frame(100), frame(300), [1])[:, : lay.cell_edges(1)[-1]]
    assert len(wl.locate(two)) == 2 and wf.clip_copies(two) == []
    eight = odd_copy(frame(100), frame(300), [0, 2, 4, 6])
    eight = eight[:, : lay.cell_edges(7)[-1]]
    assert len(wl.locate(eight)) == 8 and wf.clip_copies(eight) == []
    # Within SAME_PICTURE of the middle reading is the same picture.
    near = odd_copy(frame(100), blend(frame(100), frame(101), 0.07), [3])
    assert len(wf.clip_copies(near)) == clip.COPIES
    # The middle of an even count is the lower one: of 100, 100, 100 + 15/256
    # and 100 + 30/256, the last is past SAME_PICTURE (18.24/256) from it.
    four = odd_copy(frame(100), blend(frame(100), frame(101), 15 / 256), [2])
    four = odd_copy(four, blend(frame(100), frame(101), 30 / 256), [3])
    four = four[:, : lay.cell_edges(3)[-1]]
    found = [c.x0 for c in wf.clip_copies(four)]
    assert found == list(lay.copies[:3])
    # A torn picture: every copy's bands disagree, no copy reads it.
    torn = frame(100).copy()
    torn[270:] = frame(300)[270:]
    assert wf.clip_copies(torn) == []
    assert wf.clip_copies(np.full((540, 960), 40, dtype=np.uint8)) == []


def row_mask(levels: list[float], block_px: int) -> np.ndarray:
    """``bright_mask`` of one row of 64 blocks of ``block_px`` px, block
    ``k`` at ``levels[k]``."""
    row = np.repeat(np.array(levels, dtype=np.float32), block_px)[None, :]
    return wl.bright_mask(row)[0]


def test_bright_mask_splits_at_the_local_midpoint_within_its_reach():
    assert (wl.LOCAL_BLOCKS, wl.LOCAL_REACH) == (64, 16)
    # A dim bar (100 on 0) 16 blocks from a 255 one is in its window (dark
    # at the midpoint 127.5); 17 blocks away it is not (bright on its own).
    for gap, bright in ((16, False), (17, True)):
        levels = [0.0] * 64
        levels[0] = 255.0
        levels[gap] = 100.0
        assert row_mask(levels, 10)[gap * 10] == bright, gap
        levels = levels[::-1]
        assert row_mask(levels, 10)[(63 - gap) * 10] == bright, -gap
    # Never bright where the local levels differ by no more than 25 % of
    # full scale (63.75): 63.75 apart is flat, 64 apart splits.
    for spread, bright in ((63.0, False), (63.75, False), (64.0, True)):
        levels = [40.0, 40.0 + spread] * 32
        assert row_mask(levels, 10)[10] == bright, spread
        assert not row_mask(levels, 10)[0]
    # Bright means ABOVE the midpoint: a level right on it is not.
    levels = [0.0, 200.0, 100.0] + [0.0] * 61
    mask = row_mask(levels, 10)
    assert mask[10] and not mask[20] and not mask[0]
    # A row that is not a whole number of blocks: the last block is padded
    # with its own edge level.
    row = np.full((1, 650), 0.0, dtype=np.float32)
    row[0, 640:] = 200.0
    mask = wl.bright_mask(row)[0]
    assert mask[640:].all() and not mask[:640].any()
    # ... never with a level of its own: a flat bright tail stays flat.
    row = np.full((1, 650), 200.0, dtype=np.float32)
    row[0, :10] = 0.0
    assert not wl.bright_mask(row)[0][400:].any()
    # A block is the row's width over LOCAL_BLOCKS, rounded UP: 703 px make
    # 64 blocks of 11 px, so px 176 (block 16) still sees block 0 and px 187
    # (block 17) does not.
    row = np.zeros((1, 703), dtype=np.float32)
    row[0, :11] = 255.0
    row[0, 176:198] = 100.0
    mask = wl.bright_mask(row)[0]
    assert not mask[176:187].any() and mask[187:198].all()


def ruler_row(*runs: tuple[bool, int]) -> np.ndarray:
    """A one-row mask from (bright, length) runs."""
    return np.concatenate([np.full(n, b, dtype=bool) for b, n in runs])[None, :]


def test_ruler_candidates_are_dark_runs_of_18_cells_between_bars():
    rows, x0, cell = wl.ruler_candidates(
        ruler_row((True, 10), (False, 180), (True, 10))
    )
    assert (rows.tolist(), x0.tolist(), cell.tolist()) == ([0], [0.0], [10.0])
    # Two copies side by side: a 2-cell bar at their junction.
    mask = ruler_row((False, 5), (True, 10), (False, 180), (True, 20), (False, 180))
    mask = np.concatenate([mask, ruler_row((True, 10), (False, 5))], axis=1)
    _, x0, cell = wl.ruler_candidates(mask)
    assert x0.tolist() == [5.0, 205.0] and cell.tolist() == [10.0, 10.0]
    # A bar of at least BAR_MIN (0.75) of the cell on each side: 15 px of a
    # 20 px cell is one, 14 px is not.
    for left, right, found in ((15, 20, 1), (14, 20, 0), (20, 15, 1), (20, 14, 0)):
        mask = ruler_row((True, left), (False, 360), (True, right))
        assert len(wl.ruler_candidates(mask)[0]) == found, (left, right)
    # A cell of at least MIN_CELL_PX (2): 36 dark px is one, 35 is not.
    for dark, found in ((36, 1), (35, 0)):
        mask = ruler_row((True, 2), (False, dark), (True, 2))
        assert len(wl.ruler_candidates(mask)[0]) == found, dark
    # Runs of two rows never pair: each row's own bars only.
    mask = np.zeros((2, 220), dtype=bool)
    mask[0, :10] = True
    mask[1, 190:200] = True
    assert len(wl.ruler_candidates(mask)[0]) == 0
    mask[1, :10] = True
    rows, x0, _ = wl.ruler_candidates(mask)
    assert rows.tolist() == [1] and x0.tolist() == [0.0]


def group(x0: list[float], cell: list[float]) -> list[tuple[float, float]]:
    return wl.group_candidates(np.array(x0), np.array(cell))


def test_group_candidates_takes_one_group_per_copy():
    # Each group stands for its middle candidate by left edge: one row's own
    # reading (100.0 with its 10.0, never 99.5 with 9.95).
    groups = group(
        [100.0, 101.0, 99.0, 99.5, 300.0, 302.0], [10.0, 10.2, 9.9, 9.9, 10.0, 9.8]
    )
    assert groups == [(100.0, 10.0), (302.0, 9.8)]
    assert group([7.0, 8.0], [10.0, 10.1]) == [(8.0, 10.1)], "the upper middle of two"
    # Half a cell from the group's first candidate is another copy; just
    # under it, the same one.
    for dx, count in ((4.9, 1), (5.0, 2)):
        assert len(group([0.0, dx], [10.0, 10.0])) == count, dx
    assert len(group([0.0, 4.0, 8.0], [10.0, 10.0, 10.0])) == 2, "never a chain"
    # Cells within CELL_MATCH (5 %) of the group's first are the same copy.
    for other, count in ((10.4, 1), (10.5, 2), (10.6, 2), (9.6, 1), (9.4, 2)):
        assert len(group([0.0, 0.0], [10.0, other])) == count, other
    # A wide group further left still takes its candidate past a narrow one.
    assert group([0.0, 30.0, 40.0], [100.0, 10.0, 100.0]) == [
        (40.0, 100.0),
        (30.0, 10.0),
    ]
    # The look back stops a widest cell to the left: a candidate past it
    # starts a group, one within it still joins.
    assert len(group([0.0, 30.0, 39.0], [20.0, 2.0, 20.0])) == 3
    assert group([0.0, 30.0, 9.0], [20.0, 20.0, 20.0]) == [(9.0, 20.0), (30.0, 20.0)]
    assert group([], []) == []


def profile_of(*rows: tuple[list[float], int]) -> np.ndarray:
    """A cell profile: each (20 cell levels, how many rows)."""
    return np.concatenate(
        [np.tile(np.array(cells, dtype=float), (n, 1)) for cells, n in rows]
    )


RULER = [235.0] + [16.0] * 18 + [235.0]
BLANK = [40.0] * 20
STRIP = [16.0, 235.0] + [16.0, 235.0] * 8 + [235.0, 16.0]


def test_ruler_rows_need_white_ends_over_even_black_cells():
    assert wl.ruler_rows(profile_of((BLANK, 2), (RULER, 3), (BLANK, 2))) == [(2, 5)]
    assert wl.ruler_rows(profile_of((RULER, 2), (BLANK, 1))) == [], "3 rows at least"
    assert wl.ruler_rows(profile_of((STRIP, 5))) == [], "a strip is no ruler"
    # Contrast over 25 % of full scale: ends 80 over 16 is, 79.75 is not.
    for white, found in ((80.0, 1), (79.75, 0)):
        cells = [white] + [16.0] * 18 + [white]
        assert len(wl.ruler_rows(profile_of((cells, 3)))) == found, white
    # Ends within REF_TOLERANCE (0.2) of the contrast of each other: 236 and
    # 196 over 16 are exactly 40 apart, 0.2 of their contrast 200.
    for right, found in ((197.0, 1), (196.0, 0)):
        cells = [236.0] + [16.0] * 18 + [right]
        assert len(wl.ruler_rows(profile_of((cells, 3)))) == found, right
    # The dark cells even, their spread under REF_TOLERANCE of the contrast:
    # one cell 36 up between ends of 198 is exactly 0.2 of the contrast 180.
    for lift, found in ((35.0, 1), (36.0, 0)):
        cells = [198.0] + [16.0] * 17 + [16.0 + lift] + [198.0]
        assert len(wl.ruler_rows(profile_of((cells, 3)))) == found, lift


def test_a_strip_belongs_to_the_ruler_right_above_it():
    def strip_under(ruler_rows: int, gap: int) -> tuple[int, int] | None:
        rows = profile_of(
            (BLANK, 1), (RULER, ruler_rows), (BLANK, gap), (STRIP, 5), (BLANK, 1)
        )
        return wl.ruled_strips(rows)

    assert strip_under(4, 0) == ((5, 10),)
    # Within the ruler's height plus GAP_SLACK (2) rows, never further.
    assert strip_under(4, 6) == ((11, 16),)
    assert strip_under(4, 7) == ()
    assert strip_under(6, 8) == ((15, 20),)
    assert strip_under(6, 9) == ()
    # A strip above the ruler is not its strip.
    above = profile_of((STRIP, 5), (RULER, 4), (BLANK, 3))
    assert wl.ruled_strips(above) == ()
    # Only the strips right under a ruler: here the second ruler's, as the
    # first ruler's strip lies too far under it.
    rows = profile_of(
        (RULER, 3),
        (BLANK, 20),
        (STRIP, 4),
        (BLANK, 2),
        (RULER, 3),
        (BLANK, 1),
        (STRIP, 4),
    )
    assert wl.ruled_strips(rows) == ((33, 37),)
    # Every band's: each ruler with its strip, top to bottom.
    rows = profile_of((RULER, 3), (STRIP, 4), (BLANK, 9), (RULER, 3), (STRIP, 4))
    assert wl.ruled_strips(rows) == ((3, 7), (19, 23))


def test_a_copy_s_cells_are_summed_exactly_and_read_where_it_was_found():
    picture = frame(42)
    copies = wl.locate(picture)
    sums = wl.cell_sums(picture, copies)
    assert sums.shape == (540, 9, clip.CELLS) and sums.dtype == np.uint16
    columns = copies[0].columns()[: wl.CELL_SAMPLES]
    y = clip.layout(960, 540).strips[0][0] + 5
    assert sums[y, 0, 0] == picture[y, columns].astype(int).sum()
    assert wf.read_sums(sums, copies).index == 42
    # A colour capture: its channels' mean (alpha left out), summed, rounded:
    # a third more on each of 8 samples is 2.67 more, read 3.
    alpha = np.full_like(picture, 255)
    bgra = np.dstack([picture, picture, picture + 1, alpha])
    assert np.array_equal(wl.cell_sums(bgra, copies), sums + 3)
    with pytest.raises(ValueError, match="not of 8 copies"):
        wf.read_sums(sums, copies[:8])
    with pytest.raises(ValueError, match="not of 9 copies"):
        wf.read_sums(sums[:, :, :19], copies)
    with pytest.raises(ValueError, match="not of 9 copies"):
        wf.read_sums(sums[:, 0, :], copies)
    assert wl.copy_columns([]).shape == (0,)
    assert wf.read_sums(wl.cell_sums(picture, []), []) == wa.Reading("no_strip")
    # Each copy reads only the rows of the strips it was found with: with
    # the first band's strip alone, one reading per copy.
    first = [wl.Copy(c.x0, c.cell, c.strips[:1]) for c in copies]
    assert wf.read_sums(sums, first) == wa.Reading("ok", 42, None, 0, 9)
    # A strip run overlapping one by a single row is read; one that only
    # touches it is not.
    y0, y1 = copies[0].strips[0]
    for strips, count in (
        (((y0 - 5, y0 + 1),), 9),
        (((y1 - 1, y1 + 5),), 9),
        (((y0 - 5, y0),), 0),
        (((y1, y1 + 5),), 0),
    ):
        moved = [wl.Copy(c.x0, c.cell, strips) for c in copies]
        assert wf.read_sums(sums, moved).strips == count, strips


def test_capture_region_grabs_the_strip_rows_of_the_most_copies():
    upper = [wl.Copy(100.0 + 220 * k, 10.0, ((40, 60),)) for k in range(3)]
    lower = [wl.Copy(500.0 + 220 * k, 10.0, ((240, 280),)) for k in range(4)]
    region, inside = wl.capture_region(upper + lower, (1080, 1920))
    # The four copies of the lower band, their strip rows +/- a quarter of
    # their height, their columns +/- a cell.
    assert region == (490, 230, 1370, 290)
    assert inside == [wl.Copy(10.0 + 220 * k, 10.0, ((10, 50),)) for k in range(4)]
    # A tie: the topmost.
    region, inside = wl.capture_region(upper + lower[:3], (1080, 1920))
    assert region == (90, 35, 750, 65) and len(inside) == 3
    # Two copies, or one: all of them.
    assert len(wl.capture_region(upper[:2], (1080, 1920))[1]) == 2
    # Clamped to the picture.
    edge = [wl.Copy(3.0, 10.0, ((1, 41),))]
    assert wl.capture_region(edge, (44, 210))[0] == (0, 0, 210, 44)
    # Fractional edges: whole pixels around them.
    one = [wl.Copy(100.5, 10.25, ((40, 60),))]
    region, inside = wl.capture_region(one, (1080, 1920))
    assert region == (90, 35, 316, 65)
    assert inside == [wl.Copy(10.5, 10.25, ((5, 25),))]
    # A strip of 3 rows still gets a row to spare each side.
    short = [wl.Copy(100.0, 10.0, ((40, 43),))]
    assert wl.capture_region(short, (1080, 1920))[0][1::2] == (39, 44)
    # Strips that only touch are two sets: the larger (the one below) wins.
    touching = [wl.Copy(100.0, 10.0, ((40, 60),))] + [
        wl.Copy(400.0 + 220 * k, 10.0, ((60, 80),)) for k in range(2)
    ]
    assert wl.capture_region(touching, (1080, 1920))[0][1::2] == (55, 85)
    touching = [wl.Copy(100.0, 10.0, ((60, 80),))] + [
        wl.Copy(400.0 + 220 * k, 10.0, ((40, 60),)) for k in range(2)
    ]
    assert wl.capture_region(touching, (1080, 1920))[0][1::2] == (35, 65)
    # A copy found in two bands: each band is a set, a copy keeps the one
    # strip of the set chosen.
    both = [wl.Copy(100.0 + 220 * k, 10.0, ((40, 60), (240, 280))) for k in range(2)]
    region, inside = wl.capture_region(both + lower[:1], (1080, 1920))
    assert region[1::2] == (230, 290) and len(inside) == 3
    assert {c.strips for c in inside} == {((10, 50),)}
    # At most MAX_COPIES (4), the middle ones by left edge.
    nine = [wl.Copy(100.0 + 220 * k, 10.0, ((40, 60),)) for k in range(9)]
    region, inside = wl.capture_region(nine[::-1], (1080, 2400))
    assert wl.MAX_COPIES == 4
    assert [c.x0 + region[0] for c in inside] == [540.0, 760.0, 980.0, 1200.0]
    assert region[::2] == (530, 1410)
    five = nine[:5]
    assert [c.x0 for c in wl.capture_region(five, (1080, 2400))[1]] == [
        10.0,
        230.0,
        450.0,
        670.0,
    ]


def test_find_clip_looks_until_a_picture_shows_the_clip():
    picture = frame(77)
    blank = np.full_like(picture, clip.Y_BACKGROUND)
    shown = [None, blank, None, picture]
    asked, paused = [], []
    now = [0.0]

    def grab(region):
        asked.append(region)
        now[0] += 0.1
        return shown.pop(0)

    located = wf.find_clip(grab, None, 3.0, clock=lambda: now[0], pause=paused.append)
    assert asked == [None] * 4 and paused == [0.001, 0.001]
    lay = clip.layout(960, 540)
    region, copies = wl.capture_region(wf.clip_copies(picture), picture.shape)
    assert located == wf.Located(tuple(copies), region)
    assert region[1::2] == (lay.strips[0][0] - 4, lay.strips[0][1] + 4)
    assert [c.x0 + region[0] for c in located.copies] == list(lay.copies[2:6])
    assert located.report() == {"copies": 4, "cell_px": 5.0, "region": list(region)}
    # An area given: looked for in it, and only the strip rows and columns
    # of what was found there grabbed, in the output's coordinates.
    asked.clear()
    given = (100, 50, 900, 540)

    def in_area(region):
        asked.append(region)
        return picture[region[1] : region[3], region[0] : region[2]]

    located = wf.find_clip(in_area, given, 3.0)
    assert asked == [given]
    left, top = located.region[:2]
    assert left > 100 and top > 50 and located.region[2:] < (900, 540)
    assert [c.x0 + left for c in located.copies] == list(lay.copies[2:6])
    band = lay.strips[1]
    assert [c.strips for c in located.copies] == [((band[0] - top, band[1] - top),)] * 4
    # Nothing shown for the whole bound: None, never a guess.
    now[0] = 0.0
    tries = []

    def nothing(region):
        tries.append(now[0])
        now[0] += 0.5
        return blank

    assert wf.find_clip(nothing, None, 3.0, clock=lambda: now[0]) is None
    assert tries == [0.0, 0.5, 1.0, 1.5, 2.0, 2.5]
    assert wf.LOCATE_S == 10.0


def test_what_was_located_is_reported_with_the_middle_cell():
    copies = tuple(
        wl.Copy(100.0 * k, cell, ((0, 9),)) for k, cell in enumerate((10.0, 10.5, 20.0))
    )
    assert wf.Located(copies, (1, 2, 3, 4)).report() == {
        "copies": 3,
        "cell_px": 10.5,
        "region": [1, 2, 3, 4],
    }
    assert wf.Located(copies[:1], None).report()["region"] is None
    assert (
        wf.Located((wl.Copy(0.0, 7.333333, ((0, 9),)),), None).report()["cell_px"]
        == 7.33
    )
