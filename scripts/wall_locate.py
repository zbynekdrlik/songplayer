"""Where SongPlayer's wall clip sits in a capture (#272, clip v2): the pure
search behind ``wall_frames.py``.

``wall_clip.py`` draws each frame's index as a 16-cell Gray-code strip in
nine self-contained copies per band, each under a static RULER (cells 0 / 19
white, 1-18 black), repeated in eight bands. The wall shows whatever part of
it the operator's mapping puts there (SNV, 18.10.2026: the clip at ~2x, its
middle half), so the copies are FOUND, wherever they sit and whatever their
scale (``locate``):

- per picture row, the bright and dark runs at the row's LOCAL midpoint
  (``bright_mask``); a dark run of 18 cells between two bright bars of at
  least ``BAR_MIN`` of a cell is a candidate ruler, giving a copy's left edge
  and cell width (``ruler_candidates``);
- the candidates of one copy, one per row, are grouped
  (``group_candidates``), and a group is a copy only where, at its own
  columns, strip rows sit right under ruler rows (``ruled_strips``): the
  copy keeps those strips, and only they are ever read for it. The strip's
  own references cannot place a copy: the bit cells of most indices form
  them at a wrong phase too (``wall_clip``'s module doc).

A copy is then read through its cells' sums (``cell_sums``: per row, each
cell's ``CELL_SAMPLES`` sampled columns summed, exact in uint16), so a
capture keeps 20 numbers a row per copy, not its 160 columns; a dxcam
capture grabs only the strip rows and columns of at most ``MAX_COPIES``
copies (``capture_region``). Everything here is pure numpy and unit-tested
(``scripts/tests/test_wall_locate.py``).
"""

from __future__ import annotations

from dataclasses import dataclass, replace

import numpy as np

import wall_clip as clip

FULL_SCALE = 255.0
# A strip row: white refs - black refs > CONTRAST_MIN of the full scale, the
# two black refs (and the two white ones) within REF_TOLERANCE of that
# contrast of each other, every bit cell within it of [black, white]. A
# ruler row: the same contrast between its white end cells and its 18 black
# cells, which lie within REF_TOLERANCE of it of each other.
CONTRAST_MIN = 0.25
REF_TOLERANCE = 0.2
# The central part of a cell that is sampled (of its width) at CELL_SAMPLES
# columns, and of a strip row's height.
CELL_CORE = 0.5
CELL_SAMPLES = 8
MIN_STRIP_ROWS = 3
# A ruler: RULER_DARK cells between its two white end cells.
RULER_DARK = clip.CELLS - 2
# A candidate's cell is at least MIN_CELL_PX wide, and the bright bars beside
# its dark run at least BAR_MIN of a cell (a bar is 1 or 2 cells).
MIN_CELL_PX = 2.0
BAR_MIN = 0.75
# The local midpoint a row is split at: its lowest and highest level over
# LOCAL_REACH blocks on each side, a row being LOCAL_BLOCKS blocks wide (a
# half-row window: a ruler's dark run is under 3/8 of any window that holds
# a whole copy, so the window always holds a bar).
LOCAL_BLOCKS = 64
LOCAL_REACH = 16
# Candidates within half a cell and CELL_MATCH of each other's cell width
# are one copy.
CELL_MATCH = 0.05
# A strip belongs to a ruler it starts under within the ruler's own height
# plus GAP_SLACK rows (the clip: half the ruler's height).
GAP_SLACK = 2
# A capture reads at most this many copies (the middle ones): more add
# little to the consensus and cost memory on the box.
MAX_COPIES = 4


@dataclass(frozen=True)
class Copy:
    """One copy of the strip in a capture: its left edge and cell width
    (px, fractional), and the rows of its strips that sit right under its
    ruler (``ruled_strips``), top to bottom: the only rows read for it."""

    x0: float
    cell: float
    strips: tuple[tuple[int, int], ...]

    def columns(self) -> np.ndarray:
        """The ``CELL_SAMPLES`` sampled columns of each of its cells, cell by
        cell: evenly over each cell's central ``CELL_CORE``."""
        core = self.cell * CELL_CORE
        cells = np.arange(clip.CELLS)[:, None]
        samples = (np.arange(CELL_SAMPLES)[None, :] + 0.5) / CELL_SAMPLES
        start = self.x0 + self.cell * (cells + 0.5) - core / 2
        return np.floor(start + core * samples).astype(int).ravel()


def to_luma(image: np.ndarray) -> np.ndarray:
    """A 2-D float luma plane on 0..255 (grey, BGR(A) or RGB(A) input)."""
    plane = np.asarray(image)
    if plane.ndim == 3:
        plane = plane[:, :, 0] if plane.shape[2] == 1 else plane[:, :, :3].mean(2)
    if plane.ndim != 2:
        raise ValueError(f"not a picture: shape {np.asarray(image).shape}")
    return plane.astype(np.float32)


def bright_mask(luma: np.ndarray) -> np.ndarray:
    """Per pixel, whether it is above its row's LOCAL midpoint (a dimmed clip
    beside a bright overlay still splits); a pixel whose local levels differ
    by no more than ``CONTRAST_MIN`` of the full scale is never bright."""
    rows, width = luma.shape
    block = -(-width // LOCAL_BLOCKS)
    blocks = -(-width // block)
    padded = np.pad(luma, ((0, 0), (0, blocks * block - width)), mode="edge")
    shaped = padded.reshape(rows, blocks, block)
    low, high = shaped.min(axis=2), shaped.max(axis=2)
    low_near, high_near = low.copy(), high.copy()
    for d in range(1, min(LOCAL_REACH, blocks - 1) + 1):
        np.minimum(low_near[:, d:], low[:, :-d], out=low_near[:, d:])
        np.minimum(low_near[:, :-d], low[:, d:], out=low_near[:, :-d])
        np.maximum(high_near[:, d:], high[:, :-d], out=high_near[:, d:])
        np.maximum(high_near[:, :-d], high[:, d:], out=high_near[:, :-d])
    low_px = np.repeat(low_near, block, axis=1)[:, :width]
    high_px = np.repeat(high_near, block, axis=1)[:, :width]
    spread = high_px - low_px > CONTRAST_MIN * FULL_SCALE
    return spread & (luma > (low_px + high_px) / 2)


def ruler_candidates(mask: np.ndarray) -> tuple[np.ndarray, ...]:
    """Every dark run of a row that lies between two bright bars, each at
    least ``BAR_MIN`` of the cell the run gives (``RULER_DARK`` cells), the
    cell at least ``MIN_CELL_PX``: (row, copy's left edge, cell)."""
    padded = np.pad(mask, ((0, 0), (1, 1))).astype(np.int8)
    step = np.diff(padded, axis=1)
    row, start = np.nonzero(step == 1)
    _, end = np.nonzero(step == -1)
    same_row = row[1:] == row[:-1]
    cell = (start[1:] - end[:-1]) / RULER_DARK
    left_bar = end[:-1] - start[:-1]
    right_bar = end[1:] - start[1:]
    ok = same_row & (cell >= MIN_CELL_PX)
    ok &= (left_bar >= BAR_MIN * cell) & (right_bar >= BAR_MIN * cell)
    return row[:-1][ok], (end[:-1] - cell)[ok], cell[ok]


def group_candidates(x0: np.ndarray, cell: np.ndarray) -> list[tuple[float, float]]:
    """Candidates grouped by copy, left to right: in left-edge order, each
    joins the latest group whose first candidate lies within half that one's
    cell and whose cell is within ``CELL_MATCH`` of it. A group stands for
    its middle candidate (by left edge): one row's own reading, never a mix
    of two, so its bars, and with them every column ``Copy.columns``
    samples, lie inside the picture (each bar at least ``BAR_MIN`` of a cell
    inside it reaches past the cell's sampled core)."""
    groups: list[list[int]] = []
    widest = float(cell.max()) if cell.size else 0.0
    for i in np.argsort(x0, kind="stable"):
        home = None
        # The groups are in left-edge order: none further left than the
        # widest cell can take this candidate.
        for g in range(len(groups) - 1, -1, -1):
            first = groups[g][0]
            if x0[i] - x0[first] > widest:
                break
            near = abs(x0[i] - x0[first]) < cell[first] / 2
            if near and abs(cell[i] - cell[first]) < CELL_MATCH * cell[first]:
                home = g
                break
        if home is None:
            groups.append([int(i)])
        else:
            groups[home].append(int(i))
    middle = [members[len(members) // 2] for members in groups]
    return [(float(x0[m]), float(cell[m])) for m in middle]


def cell_profile(sampled: np.ndarray) -> np.ndarray:
    """Per picture row, the mean of each cell's sampled columns: (rows, 20)."""
    luma = to_luma(sampled)
    return luma.reshape(luma.shape[0], clip.CELLS, CELL_SAMPLES).mean(axis=2)


def runs_of(ok: np.ndarray) -> list[tuple[int, int]]:
    """The runs of at least ``MIN_STRIP_ROWS`` true rows, ``[y0, y1)``."""
    runs = []
    start = None
    for y, good in enumerate(list(ok) + [False]):
        if good and start is None:
            start = y
        elif not good and start is not None:
            if y - start >= MIN_STRIP_ROWS:
                runs.append((start, y))
            start = None
    return runs


def strip_rows(profile: np.ndarray) -> list[tuple[int, int]]:
    """The runs of rows ``[y0, y1)`` that carry a strip."""
    black = (profile[:, 0] + profile[:, -1]) / 2
    white = (profile[:, 1] + profile[:, -2]) / 2
    contrast = white - black
    slack = REF_TOLERANCE * contrast
    bits = profile[:, clip.REF_CELLS : -clip.REF_CELLS]
    ok = contrast > CONTRAST_MIN * FULL_SCALE
    ok &= np.abs(profile[:, 0] - profile[:, -1]) < slack
    ok &= np.abs(profile[:, 1] - profile[:, -2]) < slack
    ok &= (bits >= (black - slack)[:, None]).all(axis=1)
    ok &= (bits <= (white + slack)[:, None]).all(axis=1)
    return runs_of(ok)


def ruler_rows(profile: np.ndarray) -> list[tuple[int, int]]:
    """The runs of rows ``[y0, y1)`` that carry a ruler: the end cells
    white, the 18 between them black and even."""
    white = (profile[:, 0] + profile[:, -1]) / 2
    dark = profile[:, 1:-1]
    contrast = white - dark.mean(axis=1)
    slack = REF_TOLERANCE * contrast
    ok = contrast > CONTRAST_MIN * FULL_SCALE
    ok &= np.abs(profile[:, 0] - profile[:, -1]) < slack
    ok &= dark.max(axis=1) - dark.min(axis=1) < slack
    return runs_of(ok)


def ruled_strips(profile: np.ndarray) -> tuple[tuple[int, int], ...]:
    """The strip runs that start right under a ruler run (within the
    ruler's height plus ``GAP_SLACK`` rows), top to bottom."""
    rulers = ruler_rows(profile)
    return tuple(
        (s0, s1)
        for s0, s1 in strip_rows(profile)
        if any(0 <= s0 - r1 <= r1 - r0 + GAP_SLACK for r0, r1 in rulers)
    )


def locate(picture: np.ndarray) -> list[Copy]:
    """Every copy of the clip's strip in a picture, by its ruler, left to
    right, with its ruled strips (see the module doc)."""
    luma = to_luma(picture)
    _, x0s, cells = ruler_candidates(bright_mask(luma))
    copies: list[Copy] = []
    for x0, cell in group_candidates(x0s, cells):
        columns = Copy(x0, cell, ()).columns()  # inside: group_candidates
        strips = ruled_strips(cell_profile(luma[:, columns]))
        if strips:
            copies.append(Copy(x0, cell, strips))
    return copies


def copy_columns(copies: list[Copy]) -> np.ndarray:
    """The sampled columns of every copy, copy after copy."""
    if not copies:
        return np.zeros(0, dtype=int)
    return np.concatenate([c.columns() for c in copies])


def cell_sums(picture: np.ndarray, copies: list[Copy]) -> np.ndarray:
    """Per row of an 8-bit ``picture``, each copy's cells' sums of their
    ``CELL_SAMPLES`` sampled columns: (rows, copies, 20), uint16 (8 x 255
    fits). A grey picture's sums are exact. A colour capture's B, G and R
    are summed in integers and divided by 3 once, rounded: the mean of its
    channels, as ``to_luma``'s, at about a third of a float path's cost
    inside dxcam's timed polls (review round 2: ~1.5 ms a picture of SNV's
    region against 4-5 ms on dev1; ``np.take``, not fancy indexing)."""
    plane = np.take(np.asarray(picture), copy_columns(copies), axis=1)
    rows = plane.shape[0]
    if plane.ndim == 3 and plane.shape[2] > 1:
        shaped = plane[..., :3].reshape(rows, len(copies), clip.CELLS, CELL_SAMPLES, 3)
        return np.rint(shaped.sum(axis=(3, 4), dtype=np.uint32) / 3).astype(np.uint16)
    # Grey: 2-D, or one channel (its axis folds into the reshape).
    shaped = plane.reshape(rows, len(copies), clip.CELLS, CELL_SAMPLES)
    return shaped.sum(axis=3, dtype=np.uint16)


def capture_region(
    copies: list[Copy], shape: tuple[int, ...], max_rows: int | None = None
) -> tuple[tuple[int, int, int, int], list[Copy]]:
    """The part of a picture a dxcam capture grabs, for the copies found on
    it: of the strips that share rows, the largest set (the topmost on a
    tie), its rows with a quarter of their height to spare, and the columns
    of its middle ``MAX_COPIES`` copies with a cell to spare; and those
    copies within that part, each with that strip only. A smaller grab is a
    faster one: a whole 7680x1080 output is too slow to time each refresh
    (#272 lane 2). ``max_rows``: no more rows than that, from the middle of
    the rows all the chosen strips share, with none to spare (a Spout read
    of the whole strip is too slow, ``wall_spout``); where they share fewer
    than ``MIN_STRIP_ROWS`` it changes nothing."""
    height, width = shape[0], shape[1]
    sets: list[list[tuple[Copy, tuple[int, int]]]] = []
    # Top-down: a strip shares rows with a set when it starts above the
    # set's lowest end (it cannot end above the set's first start).
    for copy, strip in sorted(
        ((c, s) for c in copies for s in c.strips), key=lambda item: item[1]
    ):
        home = next((s for s in sets if strip[0] < max(e for _, (_, e) in s)), None)
        if home is None:
            sets.append([(copy, strip)])
        else:
            home.append((copy, strip))
    # The set of the most copies; a copy whose strip split into two runs
    # there counts once, with one strip over both (review round 2).
    merged: dict[Copy, tuple[int, int]] = {}
    for copy, (s0, s1) in max(sets, key=lambda s: len({c for c, _ in s})):
        a, b = merged.get(copy, (s0, s1))
        merged[copy] = (min(a, s0), max(b, s1))
    chosen = sorted(merged.items(), key=lambda item: item[0].x0)
    skip = (len(chosen) - MAX_COPIES) // 2
    chosen = chosen[max(0, skip) :][:MAX_COPIES]
    y0 = min(s0 for _, (s0, _) in chosen)
    y1 = max(s1 for _, (_, s1) in chosen)
    spare = max(1, (y1 - y0) // 4)
    top, bottom = max(0, y0 - spare), min(height, y1 + spare)
    shared0 = max(s0 for _, (s0, _) in chosen)
    shared1 = min(s1 for _, (_, s1) in chosen)
    if max_rows is not None and shared1 - shared0 >= MIN_STRIP_ROWS:
        rows = min(max_rows, shared1 - shared0)
        top = (shared0 + shared1 - rows) // 2
        bottom = top + rows
    left = max(0, int(np.floor(min(c.x0 - c.cell for c, _ in chosen))))
    right = min(
        width, int(np.ceil(max(c.x0 + (clip.CELLS + 1) * c.cell for c, _ in chosen)))
    )
    inside = [
        replace(
            c,
            x0=c.x0 - left,
            strips=((max(s0, top) - top, min(s1, bottom) - top),),
        )
        for c, (s0, s1) in chosen
    ]
    return (left, top, right, bottom), inside
