"""Read SongPlayer's wall measurement clips back from a capture (#272).

``wall_clip.py`` draws each clip frame's index as a 16-cell Gray-code strip
in self-contained copies side by side (v2: nine per band, each with its own
references), with a static ruler over each copy, repeated in eight bands.
The wall shows whatever part of it the operator's mapping puts there (SNV,
18.10.2026: the clip at ~2x, its middle half), so the reader FINDS the
copies, wherever they sit and whatever their scale (``locate``):

- per picture row, the bright and dark runs at the row's local midpoint
  (``bright_mask``); a dark run of 18 cells between two bright bars of at
  least ``BAR_MIN`` of a cell is a candidate ruler, giving a copy's left edge
  and cell width (``ruler_candidates``);
- the candidates of one copy found on several rows are grouped, and a group
  is a copy only where, at its own columns, rows that read as its ruler
  (``ruler_rows``) sit right above rows that read as a strip (``strip_rows``).
  The strip's own references cannot place a copy: the bit cells of most
  indices form them at a wrong phase too (``wall_clip``'s module doc).

Each picture is then read at the copies' sampled columns (``read_columns``):
every strip row of every copy, normalised by its own black and white
reference cells, decodes to ``(index, next_index, weight_q8)``:

- a plain frame: every cell black or white -> ``(index, None, 0)``;
- a boundary that blends two consecutive frames (#251): exactly one cell is
  grey, the two candidate codes decode to ``i`` and ``i + 1`` -> ``(i, i + 1,
  w)`` with ``w`` the later frame's weight in Q8, the program's own unit
  (``FrameBlend``: 256 = all of the later frame);
- anything else is named, never guessed: ``no_strip`` (no copy found, or no
  strip row at its columns), ``multi`` (two or more grey cells),
  ``non_adjacent`` (one grey cell whose candidates are not consecutive),
  ``disagree`` (the copies and bands of one picture read different
  positions: a torn picture).

What a run of readings means — boundaries, judder, losses, the blend
weights, the verdict — is ``wall_analysis.analyse``.

The decode is pure (numpy only) and unit-tested on frames drawn by
``wall_clip.draw_frame``. The capture glue is tiny. The copies are found ONCE
per capture, on its first picture whose copies read as one picture
(``clip_copies``), and only their sampled columns are read from every
picture: a video file through ffmpeg (``av_sync_check``'s reader: every
stored frame, with its pts; no ffprobe; a pts gap is a span the recording
was blind, and a recording slower than the wall refreshes is blind
everywhere), or the Windows desktop duplication of one output through
``dxcam`` (the box's ``ops\\probe-venv``; ``--refresh-hz`` required): with
no ``--region``, the copies are looked for on the whole output and the
capture grabs only the strip rows and columns of the copies found
(``capture_region``); each picture's sampled columns are kept, as luma, and
decoded AFTER the capture; every poll is timed, and a poll more than half a
refresh after the one before is a blind span (a poll on time that finds no
new picture is the wall holding one).

Usage::

    python scripts/wall_frames.py --fps 25 --video rec.mp4 [--width 1920]
                                  [--refresh-hz 60]
    python scripts/wall_frames.py --fps 25 --dxcam 1 --refresh-hz 60
                                  --seconds 20 [--region L,T,R,B]

The report is JSON on stdout (``located``: the copies found, their cell and
the region grabbed); exit 0 = clean, 1 = defects, 3 = incomplete, 4 =
nothing decoded (also: no copy of the clip found), 5 = the capture or its
reading failed (the error on stderr), 2 = a usage error: a broken capture
never reads as a broken wall.
"""

from __future__ import annotations

import argparse
import bisect
import json
import math
import statistics
import subprocess
import sys
import tempfile
import time
import traceback
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import numpy as np

import av_sync_check
import wall_clip as clip
from wall_analysis import (
    GRID_FPS,
    LEVEL_EDGE,
    MIN_BOUNDARIES,
    SAME_PICTURE,
    Reading,
    analyse,
    check_locked,
    check_present_rate,
)

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
# One copy's sampled columns.
COPY_COLUMNS = clip.CELLS * CELL_SAMPLES
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
# A copy's strip starts under its ruler within the ruler's own height plus
# GAP_SLACK rows (the clip: half the ruler's height).
GAP_SLACK = 2
# How long a dxcam capture looks for the copies before its timed polls (s):
# a look at a whole 7680x1080 output takes ~0.6 s on dev1.
LOCATE_S = 10.0
# main()'s exit codes (2 is argparse's usage error).
EXIT_CODES = {"clean": 0, "defects": 1, "incomplete": 3}
EXIT_NOTHING_DECODED = 4
EXIT_FAILED = 5


@dataclass(frozen=True)
class Copy:
    """One copy of the strip in a capture: its left edge and cell width
    (px, fractional), and the rows its strip was found on."""

    x0: float
    cell: float
    strip_rows: tuple[int, int]

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


def ruled_strip(profile: np.ndarray) -> tuple[int, int] | None:
    """The first strip run that starts right under a ruler run (within the
    ruler's height plus ``GAP_SLACK`` rows), or None."""
    strips = strip_rows(profile)
    for r0, r1 in ruler_rows(profile):
        for s0, s1 in strips:
            if 0 <= s0 - r1 <= r1 - r0 + GAP_SLACK:
                return s0, s1
    return None


def locate(picture: np.ndarray) -> list[Copy]:
    """Every copy of the clip's strip in a picture, by its ruler, left to
    right (see the module doc)."""
    luma = to_luma(picture)
    _, x0s, cells = ruler_candidates(bright_mask(luma))
    copies: list[Copy] = []
    for x0, cell in group_candidates(x0s, cells):
        columns = Copy(x0, cell, (0, 0)).columns()  # inside: group_candidates
        rows = ruled_strip(cell_profile(luma[:, columns]))
        if rows is not None:
            copies.append(Copy(x0, cell, rows))
    return copies


def copy_columns(copies: list[Copy]) -> np.ndarray:
    """The sampled columns of every copy, copy after copy."""
    if not copies:
        return np.zeros(0, dtype=int)
    return np.concatenate([c.columns() for c in copies])


def sample_copies(picture: np.ndarray, copies: list[Copy]) -> np.ndarray:
    """The copies' sampled columns of a captured picture (all its rows)."""
    return np.asarray(picture)[:, copy_columns(copies)]


def strip_levels(profile: np.ndarray, rows: tuple[int, int]) -> np.ndarray:
    """The 16 bit cells' levels (0 = the black refs, 1 = the white refs),
    from the core rows of one strip run."""
    y0, y1 = rows
    trim = int((y1 - y0) * (1 - CELL_CORE) / 2)
    cells = profile[y0 + trim : y1 - trim].mean(axis=0)
    black = (cells[0] + cells[-1]) / 2
    white = (cells[1] + cells[-2]) / 2
    return (cells[clip.REF_CELLS : -clip.REF_CELLS] - black) / (white - black)


def decode_levels(levels: np.ndarray, edge: float = LEVEL_EDGE) -> Reading:
    """One strip's 16 levels -> its reading (``strips`` = 1)."""
    bits = [int(level > 0.5) for level in levels]
    grey = [k for k, level in enumerate(levels) if edge < level < 1 - edge]
    if not grey:
        return Reading("ok", clip.bits_index(bits), None, 0, 1)
    if len(grey) > 1:
        return Reading("multi", strips=1)
    k = grey[0]
    low_bits = bits[:k] + [0] + bits[k + 1 :]
    high_bits = bits[:k] + [1] + bits[k + 1 :]
    low, high = clip.bits_index(low_bits), clip.bits_index(high_bits)
    if abs(low - high) != 1:
        return Reading("non_adjacent", strips=1)
    earlier, later = min(low, high), max(low, high)
    later_bit = 1 if later == high else 0
    level = float(levels[k])
    weight = level if later_bit else 1 - level
    return Reading("ok", earlier, later, int(round(weight * 256)), 1)


def read_columns(sampled: np.ndarray) -> Reading:
    """Decode one picture from its copies' sampled columns
    (``sample_copies``): every strip row of every copy, then their consensus
    (they must read the same picture, ``SAME_PICTURE``)."""
    luma = to_luma(sampled)
    if luma.shape[1] % COPY_COLUMNS:
        raise ValueError(f"{luma.shape[1]} columns are no whole copies")
    readings = []
    for k in range(luma.shape[1] // COPY_COLUMNS):
        profile = cell_profile(luma[:, k * COPY_COLUMNS : (k + 1) * COPY_COLUMNS])
        for rows in strip_rows(profile):
            readings.append(decode_levels(strip_levels(profile, rows)))
    if not readings:
        return Reading("no_strip")
    good = [r for r in readings if r.ok]
    if not good:
        status = Counter(r.status for r in readings).most_common(1)[0][0]
        return Reading(status, strips=len(readings))
    positions = sorted(r.position() for r in good)
    if positions[-1] - positions[0] > SAME_PICTURE:
        return Reading("disagree", strips=len(readings))
    median = positions[(len(positions) - 1) // 2]
    chosen = next(r for r in good if r.position() == median)
    return Reading(
        "ok", chosen.index, chosen.next_index, chosen.weight_q8, len(readings)
    )


def read_frame(image: np.ndarray) -> Reading:
    """Decode one captured picture: find its copies, then read them (none
    found: ``no_strip``)."""
    luma = to_luma(image)
    return read_columns(sample_copies(luma, locate(luma)))


def clip_copies(picture: np.ndarray) -> list[Copy]:
    """The copies a capture reads every picture at: those of ``picture``
    that read it as one picture (within ``SAME_PICTURE``) with MORE than half
    of the copies that read it at all. A copy found where none is (an
    overlay drawing a ruler over a strip) reads another index and is left
    out; with no such majority (a torn picture, two copies reading two
    pictures), none: the caller looks at the next picture."""
    luma = to_luma(picture)
    good = []
    for copy in locate(luma):
        reading = read_columns(sample_copies(luma, [copy]))
        if reading.ok:
            good.append((copy, reading.position()))
    if not good:
        return []
    middle = sorted(p for _, p in good)[(len(good) - 1) // 2]
    kept = [c for c, p in good if abs(p - middle) <= SAME_PICTURE]
    return kept if 2 * len(kept) > len(good) else []


@dataclass(frozen=True)
class Located:
    """The copies a capture reads, and the dxcam region it grabbed."""

    copies: tuple[Copy, ...]
    region: tuple[int, int, int, int] | None

    def report(self) -> dict:
        return {
            "copies": len(self.copies),
            "cell_px": round(statistics.median(c.cell for c in self.copies), 2),
            "region": list(self.region) if self.region else None,
        }


def read_video(
    path: Path, width: int | None, ffmpeg: str = "ffmpeg"
) -> tuple[list[Reading], list[float], Located | None]:
    """Every picture of a video file, read one at a time (scaled to
    ``width`` if given), and its pts (s): ``av_sync_check``'s command line
    and readers of the size and pts (the box has no ffprobe), streamed, so
    a 4K recording is never held whole. The copies are found on the first
    picture that shows them (``clip_copies``; a picture before it reads
    ``no_strip``). ffmpeg is killed if the read fails."""
    src_w, src_h = av_sync_check.probe_video_size(ffmpeg, str(path))
    out_w = width or src_w
    out_h = round(src_h * out_w / src_w / 2) * 2
    size = out_w * out_h
    readings = []
    copies: list[Copy] = []
    with tempfile.TemporaryFile() as log:
        args = av_sync_check.gray_video_args(ffmpeg, str(path), out_w, out_h)
        # The context closes ffmpeg's stdout and waits for it, also after
        # the kill when a read fails.
        with subprocess.Popen(args, stdout=subprocess.PIPE, stderr=log) as proc:
            assert proc.stdout is not None
            try:
                while chunk := proc.stdout.read(size):
                    if len(chunk) < size:
                        break
                    picture = np.frombuffer(chunk, dtype=np.uint8)
                    picture = picture.reshape(out_h, out_w)
                    copies = copies or clip_copies(picture)
                    if copies:
                        sampled = sample_copies(picture, copies)
                        readings.append(read_columns(sampled))
                    else:
                        readings.append(Reading("no_strip"))
            except BaseException:
                proc.kill()
                raise
            code = proc.wait()
        log.seek(0)
        err = log.read().decode(errors="replace")
    if code != 0:
        raise RuntimeError(f"ffmpeg exited {code} reading {path}: {err[-2000:]}")
    times = av_sync_check.showinfo_pts(err, str(path))
    if len(times) != len(readings):
        raise RuntimeError(f"read {len(readings)} pictures but {len(times)} pts")
    return readings, times, Located(tuple(copies), None) if copies else None


def capture_region(
    copies: list[Copy], shape: tuple[int, ...]
) -> tuple[tuple[int, int, int, int], list[Copy]]:
    """The region a dxcam capture grabs, for copies found on the whole
    output: the strip rows (a quarter of their height to spare) and the
    columns (a cell to spare) of the largest set of copies whose strips
    share rows (the topmost on a tie), and those copies within it. A
    smaller grab is a faster one: a whole 7680x1080 output is too slow to
    time each refresh (#272 lane 2)."""
    height, width = shape[0], shape[1]
    sets: list[list[Copy]] = []
    # Top-down: a copy shares rows with a set when its strip starts above
    # the set's lowest end (it cannot end above the set's first start).
    for copy in sorted(copies, key=lambda c: c.strip_rows):
        home = next(
            (s for s in sets if copy.strip_rows[0] < max(c.strip_rows[1] for c in s)),
            None,
        )
        if home is None:
            sets.append([copy])
        else:
            home.append(copy)
    chosen = max(sets, key=len)
    y0 = min(c.strip_rows[0] for c in chosen)
    y1 = max(c.strip_rows[1] for c in chosen)
    spare = max(1, (y1 - y0) // 4)
    top, bottom = max(0, y0 - spare), min(height, y1 + spare)
    left = max(0, math.floor(min(c.x0 - c.cell for c in chosen)))
    right = min(width, math.ceil(max(c.x0 + (clip.CELLS + 1) * c.cell for c in chosen)))
    inside = [
        Copy(c.x0 - left, c.cell, (c.strip_rows[0] - top, c.strip_rows[1] - top))
        for c in chosen
    ]
    return (left, top, right, bottom), inside


def find_clip(
    grab: Callable[..., np.ndarray | None],
    region: tuple[int, int, int, int] | None,
    seconds: float,
    clock: Callable[[], float] = time.perf_counter,
    pause: Callable[[float], None] = time.sleep,
) -> Located | None:
    """Grab pictures (``region``, or the whole output) until one shows copies
    that read as one picture (``clip_copies``), for at most ``seconds``:
    the copies and the region to capture (``region``, or with none given
    ``capture_region``); None if no picture showed them. ``grab`` takes the
    region by keyword, as dxcam's does; a grab that finds no new picture
    pauses 1 ms."""
    deadline = clock() + seconds
    while clock() < deadline:
        picture = grab(region=region)
        if picture is None:
            pause(0.001)
            continue
        copies = clip_copies(picture)
        if not copies:
            continue
        if region is None:
            region, copies = capture_region(copies, np.asarray(picture).shape)
        return Located(tuple(copies), region)
    return None


def capture_dxcam(
    output: int, region: tuple[int, int, int, int] | None, seconds: float
) -> tuple[list[float], list[np.ndarray], list[float], list[float], Located | None]:
    """Every new picture of desktop output ``output`` for ``seconds``
    (Windows, dxcam with ``output_color="BGRA"``: no colour conversion, so
    dxcam 0.3+ never loads OpenCV; dxcam 0.0.5 imports it whatever the
    colour), after ``find_clip`` found the copies (``LOCATE_S``): its poll's
    time (stamped before the grab), the copies' columns as luma (decoded
    after the capture, so the decode never delays a grab), every poll's
    time, when each picture's acquire was over at the latest
    (``acquire_ended``), and what was located (None: no copy found, nothing
    captured). The frame was on screen between the stamp and that end — a
    grab preempted before its acquire must not move the picture early
    (review round 7). The end is stamped right after dxcam's
    ``AcquireNextFrame`` call when this dxcam exposes it, else after the
    whole grab. A poll that finds nothing sleeps 1 ms (Python 3.11+ on
    Windows sleeps on a high-resolution timer)."""
    import dxcam  # Windows only (the box's ops\probe-venv)

    camera = dxcam.create(output_idx=output, output_color="BGRA")
    acquired = time_acquire(camera)
    times: list[float] = []
    pictures: list[np.ndarray] = []
    polls: list[float] = []
    acquire_ended: list[float] = []
    located = find_clip(camera.grab, region, LOCATE_S)
    if located is None:
        del camera
        return times, pictures, polls, acquire_ended, None
    columns = copy_columns(list(located.copies))
    deadline = time.perf_counter() + seconds
    while (now := time.perf_counter()) < deadline:
        polls.append(now)
        acquired[0] = None
        picture = camera.grab(region=located.region)
        if picture is None:
            time.sleep(0.001)
            continue
        ended = acquired[0] if acquired[0] is not None else time.perf_counter()
        times.append(now)
        acquire_ended.append(ended)
        pictures.append(to_luma(picture[:, columns]).astype(np.uint8))
    del camera
    return times, pictures, polls, acquire_ended, located


def time_acquire(camera: object) -> list[float | None]:
    """Stamp the end of each of ``camera``'s acquires: dxcam's duplicator
    calls ``AcquireNextFrame`` in ``update_frame``, so the stamp right after
    it returns bounds when the frame was taken. The returned cell holds the
    latest stamp (reset by the caller before each grab); a dxcam with no
    such duplicator leaves it ``None`` and the caller stamps after the grab."""
    acquired: list[float | None] = [None]
    duplicator = getattr(camera, "_duplicator", None)
    update = getattr(duplicator, "update_frame", None)
    if update is None:
        return acquired

    def timed_update(*args, **kwargs):
        try:
            return update(*args, **kwargs)
        finally:
            acquired[0] = time.perf_counter()

    try:
        duplicator.update_frame = timed_update
    except AttributeError:  # a frozen or slotted duplicator: whole grabs
        pass
    return acquired


def poll_blind(times: list[float], polls: list[float]) -> list[float]:
    """Each dxcam picture's blind span: the time since the poll before the
    one that got it (an empty poll proves nothing new was presented); the
    first poll's picture: unknown (``inf``)."""
    out = []
    for t in times:
        k = bisect.bisect_left(polls, t)
        out.append(t - polls[k - 1] if k else math.inf)
    return out


def capture(args: argparse.Namespace) -> dict:
    """Capture or read the pictures ``args`` names, then the report."""
    late = None
    if args.video is not None:
        readings, times, located = read_video(args.video, args.width)
        if args.locked:
            check_locked(times, args.refresh_hz)
        blind = None  # since the picture before
        span = None  # the first picture's pts to the last one's
    else:
        times, sampled, polls, ended, located = capture_dxcam(
            args.dxcam, args.region, args.seconds
        )
        if located is None:
            print(
                f"wall_frames: no copy of the clip's strip found on output "
                f"{args.dxcam} in {LOCATE_S:g} s (region {args.region}): nothing "
                "captured",
                file=sys.stderr,
            )
            report = analyse([], args.fps, args.grid_fps, args.frames)
            report["located"] = None
            return report
        readings = [read_columns(s) for s in sampled]
        blind = poll_blind(times, polls)
        late = [max(0.0, e - t) for t, e in zip(times, ended)]
        check_present_rate(times, args.refresh_hz, blind, late)
        span = (polls[0], polls[-1]) if polls else None
    report = analyse(
        readings,
        args.fps,
        args.grid_fps,
        args.frames,
        times=times,
        refresh_hz=args.refresh_hz,
        blind=blind,
        span=span,
        min_boundaries=args.min_boundaries,
        locked=args.locked,
        late=late,
    )
    report["located"] = located.report() if located else None
    return report


def parse_region(text: str) -> tuple[int, int, int, int]:
    try:
        left, top, right, bottom = (int(v) for v in text.split(","))
    except ValueError:
        raise argparse.ArgumentTypeError(f"not L,T,R,B: {text!r}") from None
    return left, top, right, bottom


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fps", type=int, required=True, choices=clip.FPS_SET)
    parser.add_argument("--grid-fps", type=int, default=GRID_FPS)
    parser.add_argument("--frames", type=int, help="frames in the clip")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--video", type=Path)
    source.add_argument("--dxcam", type=int, metavar="OUTPUT")
    parser.add_argument("--width", type=int, help="scale a video to this width")
    # The wall's own rate: one read from a capture is the capture's (a
    # camera's; dxcam's 1 ms polls read 58.8 Hz for 60).
    parser.add_argument(
        "--refresh-hz", type=float, required=True, help="the wall's refresh rate"
    )
    parser.add_argument(
        "--locked",
        action="store_true",
        help="each video picture is one wall refresh (the wall's own signal)",
    )
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument(
        "--region",
        type=parse_region,
        help="dxcam region L,T,R,B (default: found from the copies on the output)",
    )
    parser.add_argument(
        "--min-boundaries",
        type=int,
        default=MIN_BOUNDARIES,
        help="boundaries a clean capture judges for judder",
    )
    args = parser.parse_args(argv)
    if args.locked and args.dxcam is not None:
        parser.error("--locked is for a --video of the wall's own signal")
    try:
        report = capture(args)
    except Exception as exc:  # a broken capture is never a broken wall
        traceback.print_exc()
        print(f"wall_frames: the capture failed: {exc}", file=sys.stderr)
        return EXIT_FAILED
    print(json.dumps(report, indent=2))
    if not report["decoded"]:
        return EXIT_NOTHING_DECODED
    return EXIT_CODES[report["verdict"]]


if __name__ == "__main__":
    sys.exit(main())
