"""Read SongPlayer's wall measurement clips back from a capture (#272 lane 1).

``wall_clip.py`` draws each clip frame's index as a 16-cell Gray-code strip,
repeated in eight bands. This reader finds every strip row in a captured
picture, normalises its cells by the strip's own black and white reference
cells, and decodes each picture to ``(index, next_index, weight_q8)``:

- a plain frame: every cell black or white -> ``(index, None, 0)``;
- a boundary that blends two consecutive frames (#251): exactly one cell is
  grey, the two candidate codes decode to ``i`` and ``i + 1`` -> ``(i, i + 1,
  w)`` with ``w`` the later frame's weight in Q8, the program's own unit
  (``FrameBlend``: 256 = all of the later frame);
- anything else is named, never guessed: ``no_strip`` (no strip row found),
  ``multi`` (two or more grey cells), ``non_adjacent`` (one grey cell whose
  candidates are not consecutive), ``disagree`` (the bands of one picture
  read different positions: a torn picture).

``analyse`` turns a run of readings (one per captured refresh) into the
report: boundaries and the refreshes each was on screen for (another count
than the capture rate gives is judder; with capture times a stall that held
a picture shows here too), lost frames, lost boundaries, jumps back,
cadence errors, loops, and the blend weights against the pacer's rule for
the clip's fps on SongPlayer's 30 fps grid (``.claude/rules/genlock.md``:
25 on 30 = plain, 213, 170, 128, 85, 42), at the grid phase fitted per play.
What the capture did not see (unreadable pictures, spans it was blind) can
excuse at most one boundary per unseen refresh, as ``unverified``; anything
beyond that is lost. A capture with unverified steps or under
``MIN_COVERAGE`` decoded pictures is ``incomplete``, never ``clean``. The
clip LOOPS (the test playlist's mode): the song's end and the new play are
not the measured cadence, so the steps at a loop seam are reported under
``loop_seams`` and kept out of the verdict; a jump to the start from
mid-clip is a jump back.

The decode is pure (numpy only) and unit-tested on frames drawn by
``wall_clip.draw_frame``. The capture glue is tiny: a video file through
ffmpeg (every stored frame, ``-fps_mode passthrough``, with its pts from
``showinfo``; the size read the repo's way, ``av_sync_check``, no ffprobe;
a pts gap is a span the recording was blind), or the Windows desktop
duplication of one output through ``dxcam`` (the box's ``ops\\probe-venv``):
only each picture's sampled columns are kept, as luma, and decoded AFTER
the capture; every poll is timed, and a poll that came late is a blind span
(a poll on time that finds no new picture is the wall holding one).

Usage::

    python scripts/wall_frames.py --fps 25 --video rec.mp4 [--width 1920]
                                  [--refresh-hz 60]
    python scripts/wall_frames.py --fps 25 --dxcam 1 --seconds 20
                                  [--region L,T,R,B] [--x-range 0,1]

The report is JSON on stdout; exit 0 = clean, 1 = defects, 2 = nothing
decoded, 3 = incomplete.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import subprocess
import sys
import tempfile
import time
from collections import Counter
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path

import numpy as np

import wall_clip as clip

GRID_FPS = 30
# The pacer's dead band (`BLEND_DEAD_BAND_100NS`, 2 ms): no blend that close
# after the shown frame, and only for a gap strictly inside (1, 2) slots by it.
DEAD_BAND_S = Fraction(2, 1000)

FULL_SCALE = 255.0
# A cell whose level is within LEVEL_EDGE of black or white is a plain bit.
LEVEL_EDGE = 0.04
# A strip row: white refs - black refs > CONTRAST_MIN of the full scale, the
# two black refs (and the two white ones) within REF_TOLERANCE of that
# contrast of each other, every bit cell within it of [black, white].
CONTRAST_MIN = 0.25
REF_TOLERANCE = 0.2
# The central part of a cell that is sampled (of its width) at CELL_SAMPLES
# columns, and of a strip row's height.
CELL_CORE = 0.5
CELL_SAMPLES = 8
MIN_STRIP_ROWS = 3
# analyse(): a step or a weight off by more than this (Q8) is an error.
Q8_TOLERANCE = 8
# Two readings show the same picture within the Q8 tolerance plus the level
# edge (one picture near a weight of 0 or 256 can read grey or plain).
SAME_PICTURE = (Q8_TOLERANCE + 256 * LEVEL_EDGE) / 256
# A loop: the clip's last WRAP_FRAMES frames to its first WRAP_FRAMES.
WRAP_FRAMES = 3
# A video pts interval over this many times the median: the recording was
# blind there (it dropped frames).
CAPTURE_GAP = 1.5
# A dxcam poll this late after the one before may have missed a refresh.
POLL_LATE_S = 0.008
# Below this share of decoded pictures a capture is incomplete.
MIN_COVERAGE = 0.95
LIST_MAX = 20


@dataclass(frozen=True)
class Reading:
    """One captured picture: what its strip rows say."""

    status: str
    index: int | None = None
    next_index: int | None = None
    weight_q8: int = 0
    strips: int = 0

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    def position(self) -> float:
        """The clip position it shows, in frames (``index + weight``)."""
        assert self.index is not None
        return self.index + self.weight_q8 / 256


def to_luma(image: np.ndarray) -> np.ndarray:
    """A 2-D float luma plane on 0..255 (grey, BGR(A) or RGB(A) input)."""
    plane = np.asarray(image)
    if plane.ndim == 3:
        plane = plane[:, :, 0] if plane.shape[2] == 1 else plane[:, :, :3].mean(2)
    if plane.ndim != 2:
        raise ValueError(f"not a picture: shape {np.asarray(image).shape}")
    return plane.astype(np.float32)


def cell_columns(width: int, x_range: tuple[float, float]) -> np.ndarray:
    """The ``CELL_SAMPLES`` sampled columns of each of the strip's cells,
    cell by cell, in a ``width`` px capture whose clip spans ``x_range``
    (fractions of the capture width): evenly over each cell's central
    ``CELL_CORE``."""
    x0 = x_range[0] * width
    span = (x_range[1] - x_range[0]) * width
    edges = clip.cell_fractions()
    columns = []
    for left, right in zip(edges[:-1], edges[1:]):
        core = span * (right - left) * CELL_CORE
        start = x0 + span * (left + right) / 2 - core / 2
        for i in range(CELL_SAMPLES):
            column = math.floor(start + core * (i + 0.5) / CELL_SAMPLES)
            if not 0 <= column < width:
                raise ValueError("the strip's cells fall outside the capture")
            columns.append(column)
    return np.array(columns)


def sample_columns(image: np.ndarray, x_range: tuple[float, float]) -> np.ndarray:
    """The strip's sampled columns of a captured picture (all its rows)."""
    picture = np.asarray(image)
    return picture[:, cell_columns(picture.shape[1], x_range)]


def cell_profile(sampled: np.ndarray) -> np.ndarray:
    """Per picture row, the mean of each cell's sampled columns: (rows, 20)."""
    luma = to_luma(sampled)
    return luma.reshape(luma.shape[0], clip.CELLS, CELL_SAMPLES).mean(axis=2)


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
    """Decode one picture from its sampled columns (``sample_columns``):
    every strip row, then their consensus (the bands must read the same
    picture, ``SAME_PICTURE``)."""
    profile = cell_profile(sampled)
    runs = strip_rows(profile)
    if not runs:
        return Reading("no_strip")
    readings = [decode_levels(strip_levels(profile, rows)) for rows in runs]
    good = [r for r in readings if r.ok]
    if not good:
        status = Counter(r.status for r in readings).most_common(1)[0][0]
        return Reading(status, strips=len(runs))
    positions = sorted(r.position() for r in good)
    if positions[-1] - positions[0] > SAME_PICTURE:
        return Reading("disagree", strips=len(runs))
    median = positions[(len(positions) - 1) // 2]
    chosen = next(r for r in good if r.position() == median)
    return Reading("ok", chosen.index, chosen.next_index, chosen.weight_q8, len(runs))


def read_frame(image: np.ndarray, x_range: tuple[float, float] = (0.0, 1.0)) -> Reading:
    """Decode one captured picture."""
    return read_columns(sample_columns(image, x_range))


def blend_q8(position: Fraction, fps: int, grid_fps: int = GRID_FPS) -> int:
    """The weight (Q8, 0 = plain) the pacer shows at clip ``position``
    (frames): ``into * 256 / gap`` floored, none inside the dead band, none
    for a gap not strictly inside (one slot, two slots) by it."""
    gap = Fraction(1, fps)
    slot = Fraction(1, grid_fps)
    if not slot + DEAD_BAND_S < gap < 2 * slot - DEAD_BAND_S:
        return 0
    frac = position - math.floor(position)
    if frac * gap < DEAD_BAND_S:
        return 0
    return math.floor(frac * 256)


def readable_positions(
    position: Fraction, fps: int, grid_fps: int = GRID_FPS
) -> list[Fraction]:
    """Where the reader may read the pacer's boundary at clip ``position``:
    at its weight, and — for a weight within the level edge plus the Q8
    tolerance of 0 or 256 (the program's 8-bit blend rounds it either way of
    the edge) — as the plain frame at that end."""
    whole = math.floor(position)
    q8 = blend_q8(position, fps, grid_fps)
    band = 256 * LEVEL_EDGE + Q8_TOLERANCE
    out = [whole + Fraction(q8, 256)]
    if q8 and q8 >= 256 - band:
        out.append(Fraction(whole + 1))
    if q8 and q8 <= band:
        out.append(Fraction(whole))
    return out


def cycle_length(fps: int, grid_fps: int = GRID_FPS) -> int:
    """Grid boundaries per repeat of the clip/grid pattern (25/30: 6)."""
    return Fraction(fps, grid_fps).denominator


def expected_weights(
    fps: int, grid_fps: int = GRID_FPS, phase: Fraction = Fraction(0)
) -> list[int]:
    """One cycle of the weights the pacer shows, from a boundary at clip
    position ``phase`` (25 on 30 from 0: 0, 213, 170, 128, 85, 42)."""
    step = Fraction(fps, grid_fps)
    return [
        blend_q8(phase + k * step, fps, grid_fps)
        for k in range(cycle_length(fps, grid_fps))
    ]


@dataclass
class Boundary:
    """Consecutive decoded readings that show the same picture."""

    first_sample: int
    last_sample: int
    reading: Reading
    refreshes: int = 1

    def whole(self) -> bool:
        """Every sample from its first to its last was read (none undecoded)."""
        return self.last_sample - self.first_sample + 1 == self.refreshes


def boundaries(readings: list[Reading]) -> list[Boundary]:
    """Group the decoded readings into the boundaries they show: a reading
    within ``SAME_PICTURE`` of the one before shows the same picture. An
    undecoded reading between two such readings does not split them."""
    runs: list[Boundary] = []
    for n, reading in enumerate(readings):
        if not reading.ok:
            continue
        if runs and abs(runs[-1].reading.position() - reading.position()) <= (
            SAME_PICTURE
        ):
            runs[-1].refreshes += 1
            runs[-1].last_sample = n
        else:
            runs.append(Boundary(n, n, reading))
    return runs


@dataclass(frozen=True)
class Step:
    """From one boundary to the next."""

    sample: int
    delta: float
    count: int
    wrapped: bool
    unseen: int
    kind: str | None
    excused: int = 0
    lost: int = 0


def step_tolerance(fps: int) -> float:
    """A step's slack (frames): the Q8 tolerance, plus the larger of the
    pacer's dead band (a boundary inside it shows the plain frame up to
    ``DEAD_BAND_S`` x fps frames before its position) and the reader's level
    edge (a weight within it of 256 reads as the next frame). Both never
    fall in one step."""
    dead_band = float(DEAD_BAND_S) * fps
    return Q8_TOLERANCE / 256 + max(dead_band, LEVEL_EDGE)


def unseen_refreshes(
    prev: Boundary,
    run: Boundary,
    times: list[float] | None,
    period: float | None,
    blind_spans: list[tuple[float, float]] | None,
) -> int:
    """How many refreshes between two boundaries the capture did not see:
    each unreadable picture, and each refresh inside a span it was blind."""
    unseen = run.first_sample - prev.last_sample - 1
    if times is not None and blind_spans:
        t0, t1 = times[prev.last_sample], times[run.first_sample]
        for a, b in blind_spans:
            overlap = min(b, t1) - max(a, t0)
            if overlap > 0:
                unseen += math.ceil(overlap / period) if period else 1
    return unseen


def steps_between(
    runs: list[Boundary],
    n_frames: int,
    fps: int,
    grid_fps: int = GRID_FPS,
    times: list[float] | None = None,
    period: float | None = None,
    blind_spans: list[tuple[float, float]] | None = None,
) -> list[Step]:
    """Each step's clip advance (frames) and its grid boundaries, judged:
    ``jump_back`` (the clip went back), ``cadence`` (not a whole number of
    grid steps), ``lost`` (boundaries never shown) or ``None``. Boundaries
    a step skips that fit in the refreshes the capture did not see
    (``unseen``, one boundary each) are ``excused``: such a step is
    ``unverified``, and only what does not fit is ``lost``. The clip's loop
    (its last frames to its first) is ``wrapped``, never a jump back."""
    step = fps / grid_fps
    tolerance = step_tolerance(fps)
    out = []
    for prev, run in zip(runs[:-1], runs[1:]):
        before, after = prev.reading.position(), run.reading.position()
        delta = after - before
        wrapped = (
            delta < -n_frames / 2
            and before >= n_frames - WRAP_FRAMES
            and after < WRAP_FRAMES
        )
        if wrapped:
            delta += n_frames
        count = round(delta / step)
        unseen = unseen_refreshes(prev, run, times, period, blind_spans)
        kind = None
        excused = lost = 0
        if delta < -tolerance:
            kind = "jump_back"
        elif count == 0 or abs(delta - count * step) > tolerance:
            kind = "cadence"
        elif count > 1:
            excused = min(count - 1, unseen)
            lost = count - 1 - excused
            kind = "lost" if lost else "unverified"
        out.append(
            Step(run.first_sample, delta, count, wrapped, unseen, kind, excused, lost)
        )
    return out


def seam_steps(steps: list[Step]) -> set[int]:
    """The steps at a loop seam: the wrap itself and the step on each side
    (the song's end and the new play are not the measured cadence)."""
    seam: set[int] = set()
    for n, s in enumerate(steps):
        if s.wrapped:
            seam.update(k for k in (n - 1, n, n + 1) if 0 <= k < len(steps))
    return seam


def pts_gaps(times: list[float]) -> list[tuple[float, float]]:
    """The spans a recording was blind: a pts interval over ``CAPTURE_GAP``
    times the median (the recorder dropped frames there)."""
    if len(times) < 2:
        return []
    limit = CAPTURE_GAP * statistics.median(b - a for a, b in zip(times, times[1:]))
    return [(a, b) for a, b in zip(times, times[1:]) if b - a > limit]


def analyse(
    readings: list[Reading],
    fps: int,
    grid_fps: int = GRID_FPS,
    frames: int | None = None,
    times: list[float] | None = None,
    refresh_hz: float | None = None,
    blind_spans: list[tuple[float, float]] | None = None,
) -> dict:
    """The report of a capture of the ``fps`` clip (see the module doc).

    ``times`` are the pictures' capture times (s), ``blind_spans`` the
    spans the capture could not see. The refresh rate is ``refresh_hz``,
    else read from ``times``, else there is no refresh check. Errors at a
    loop seam (``seam_steps``) are reported under ``loop_seams``; the
    verdict counts the steady cadence."""
    n_frames = clip.frame_count(fps, clip.CLIP_SECONDS) if frames is None else frames
    if refresh_hz is None and times is not None and len(times) > 1:
        refresh_hz = 1 / statistics.median(b - a for a, b in zip(times, times[1:]))
    period = 1 / refresh_hz if refresh_hz else None
    runs = boundaries(readings)
    steps = steps_between(runs, n_frames, fps, grid_fps, times, period, blind_spans)
    seam = seam_steps(steps)
    steady = [s for n, s in enumerate(steps) if n not in seam]
    at_seam = [s for n, s in enumerate(steps) if n in seam]
    jumps = [s.sample for s in steady if s.kind == "jump_back"]
    losses = frame_losses(runs, steps, seam, n_frames, fps, grid_fps)
    weights = weight_report(runs, steps, seam, fps, grid_fps)
    refresh = refresh_report(runs, steps, seam, grid_fps, refresh_hz, times)
    decoded = sum(r.ok for r in readings)
    report: dict = {
        "fps": fps,
        "grid_fps": grid_fps,
        "frames_in_clip": n_frames,
        "samples": len(readings),
        "decoded": decoded,
        "undecoded": dict(Counter(r.status for r in readings if not r.ok)),
        "boundaries": len(runs),
        "refreshes_per_boundary": {
            str(k): v for k, v in sorted(Counter(b.refreshes for b in runs).items())
        },
        "refreshes": refresh,
        "frames_seen": len(frames_seen(runs, steps, n_frames)),
        "lost_frames": len(losses["lost"]),
        "lost_frame_list": losses["lost"][:LIST_MAX],
        "lost_boundaries": sum(s.lost for s in steady if s.kind == "lost"),
        "jumps_back": len(jumps),
        "jump_back_samples": jumps[:LIST_MAX],
        "cadence_errors": sum(s.kind == "cadence" for s in steady),
        "unverified": {
            "steps": sum(s.excused > 0 for s in steady),
            "boundaries": sum(s.excused for s in steady),
            "frames": losses["unverified"],
        },
        "loops": sum(s.wrapped for s in steps),
        "loop_seams": {
            "steps": len(at_seam),
            "errors": sum(s.kind is not None for s in at_seam)
            + losses["seam"]
            + weights.pop("seam_mismatches"),
        },
        "weights": weights,
    }
    if times is not None:
        report["capture"] = capture_report(times, blind_spans or [])
    report["verdict"] = verdict(report)
    return report


def verdict(report: dict) -> str:
    """``defects`` for any loss, jump back, cadence error, weight mismatch
    or judder (or nothing decoded); else ``incomplete`` when the capture did
    not see it all (unverified steps, or under ``MIN_COVERAGE`` decoded);
    else ``clean``."""
    refresh = report["refreshes"]
    defects = (
        report["lost_frames"]
        + report["lost_boundaries"]
        + report["jumps_back"]
        + report["cadence_errors"]
        + report["weights"]["mismatches"]
        + (refresh["uneven"] if refresh else 0)
    )
    if defects or not report["boundaries"]:
        return "defects"
    coverage = report["decoded"] / report["samples"]
    if report["unverified"]["steps"] or coverage < MIN_COVERAGE:
        return "incomplete"
    return "clean"


def frames_seen(runs: list[Boundary], steps: list[Step], n_frames: int) -> set[int]:
    """Every clip frame a boundary showed (alone or blended), numbered on
    across the clip's loops."""
    seen: set[int] = set()
    loops = 0
    for n, run in enumerate(runs):
        if n and steps[n - 1].wrapped:
            loops += 1
        r = run.reading
        assert r.index is not None
        seen.add(r.index + loops * n_frames)
        if r.next_index is not None:
            seen.add(r.next_index + loops * n_frames)
    return seen


def frame_losses(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    n_frames: int,
    fps: int,
    grid_fps: int,
) -> dict:
    """The clip frames no boundary showed, step by step: ``lost`` (the
    frames), ``unverified`` (how many the step's excused boundaries could
    have shown unseen) and ``seam`` (how many, at a loop seam)."""
    seen = frames_seen(runs, steps, n_frames)
    step = fps / grid_fps
    lost: list[int] = []
    unverified = at_seam = 0
    loops = 0
    for n, s in enumerate(steps):
        before = loops
        loops += s.wrapped
        prev, run = runs[n].reading, runs[n + 1].reading
        assert prev.index is not None and run.index is not None
        last = prev.index if prev.next_index is None else prev.next_index
        last += before * n_frames
        first = run.index + loops * n_frames
        missing = [f for f in range(last + 1, first) if f not in seen]
        if n in seam:
            at_seam += len(missing)
            continue
        # The frames the excused boundaries could have shown unseen.
        reach = math.floor(prev.position() + before * n_frames + s.excused * step)
        unseen = [f for f in missing if s.excused and f <= reach + 1]
        unverified += len(unseen)
        lost.extend(f % n_frames for f in missing if f not in unseen)
    return {"lost": lost, "unverified": unverified, "seam": at_seam}


def refresh_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    grid_fps: int,
    refresh_hz: float | None,
    times: list[float] | None,
) -> dict | None:
    """Every boundary on screen for the refreshes the capture rate gives on
    the grid (60 Hz on 30 fps: 2), else ``uneven`` (judder: held too long,
    a stall included, or too short). With capture times a boundary's count
    is the time from its first picture to the next boundary's, so a wall
    that held a picture while the capture saw no new one counts in full.
    Judged only where the capture saw it whole: not the first or last
    boundary (the capture's edges), none with an unreadable picture or a
    blind span inside or beside it, and not the song's last boundary or the
    new play's first two at a loop seam (the boundaries a seam step leads
    to). ``None`` without a refresh rate."""
    if not refresh_hz:
        return None
    per = refresh_hz / grid_fps
    if abs(per - round(per)) <= 0.01 * per:
        expected = [round(per)]
    else:
        expected = [math.floor(per), math.ceil(per)]
    uneven = []
    for n in range(1, len(runs) - 1):
        run = runs[n]
        if not run.whole() or steps[n - 1].unseen or steps[n].unseen:
            continue
        if n - 1 in seam:
            continue
        shown = run.refreshes
        if times is not None:
            on_screen = times[runs[n + 1].first_sample] - times[run.first_sample]
            shown = round(on_screen * refresh_hz)
        if shown not in expected:
            uneven.append(run.first_sample)
    return {
        "refresh_hz": round(refresh_hz, 3),
        "expected": expected,
        "uneven": len(uneven),
        "uneven_samples": uneven[:LIST_MAX],
    }


def weight_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    fps: int,
    grid_fps: int,
) -> dict:
    """The observed weights against the pacer's rule, at the phase fitted
    per play (a loop or a jump back starts a new one, which may sit on
    another phase). A boundary next to a seam step counts under
    ``seam_mismatches``."""
    cycle = cycle_length(fps, grid_fps)
    answer: dict = {
        "expected_cycle_from_0": expected_weights(fps, grid_fps),
        "observed_first_cycle": [b.reading.weight_q8 for b in runs[:cycle]],
        "phase_frames": [],
        "expected_cycle_fitted": [],
        "mismatches": 0,
        "seam_mismatches": 0,
        "max_residual_q8": 0.0,
    }
    near_seam = {k for n in seam for k in (n, n + 1)}
    for segment in play_segments(runs, steps):
        counts = [0]
        for n in segment[1:]:
            counts.append(counts[-1] + steps[n - 1].count)
        positions = [runs[n].reading.position() for n in segment]
        pairs = list(zip(positions, counts))
        fit = [p for n, p in zip(segment, pairs) if n not in near_seam] or pairs
        phase = fit_phase([u for u, _ in fit], [k for _, k in fit], fps, grid_fps)
        answer["phase_frames"].append(round(float(phase), 4))
        if not answer["expected_cycle_fitted"]:
            answer["expected_cycle_fitted"] = expected_weights(fps, grid_fps, phase)
        for n, u, k in zip(segment, positions, counts):
            residual = weight_residual_q8(u, phase, k, fps, grid_fps)
            if n in near_seam:
                answer["seam_mismatches"] += residual > Q8_TOLERANCE
                continue
            answer["mismatches"] += residual > Q8_TOLERANCE
            answer["max_residual_q8"] = max(
                answer["max_residual_q8"], round(residual, 2)
            )
    return answer


def play_segments(runs: list[Boundary], steps: list[Step]) -> list[list[int]]:
    """The boundaries of each play through the clip, split where a new one
    starts: at a loop, and at a jump back."""
    segments: list[list[int]] = [[]] if runs else []
    for n in range(len(runs)):
        if n and (steps[n - 1].wrapped or steps[n - 1].kind == "jump_back"):
            segments.append([])
        segments[-1].append(n)
    return segments


def fit_phase(
    positions: list[float], counts: list[int], fps: int, grid_fps: int
) -> Fraction:
    """The clip position (modulo one frame) of the grid the boundaries sit
    on: each position less its grid steps, centred on their circular mean,
    then their median (a few wrong boundaries do not move it), plus the half
    Q8 step the pacer's floored weight loses on average."""
    step = fps / grid_fps
    offsets = [(u - k * step) % 1 for u, k in zip(positions, counts)]
    angles = [2 * math.pi * x for x in offsets]
    mean = math.atan2(
        sum(math.sin(a) for a in angles), sum(math.cos(a) for a in angles)
    ) / (2 * math.pi)
    spread = [(x - mean + 0.5) % 1 - 0.5 for x in offsets]
    fitted = (mean + statistics.median(spread) + 0.5 / 256) % 1
    return Fraction(fitted).limit_denominator(1 << 16)


def weight_residual_q8(
    position: float, phase: Fraction, count: int, fps: int, grid_fps: int
) -> float:
    """How far (Q8, modulo whole frames: a frame off is the step check's) a
    boundary's position is from the nearest place the reader may read what
    the pacer shows ``count`` grid steps into a grid at ``phase``."""
    expected = phase + count * Fraction(fps, grid_fps)
    best = 1.0
    for shown in readable_positions(expected, fps, grid_fps):
        off = (position - float(shown)) % 1
        best = min(best, off, 1 - off)
    return 256 * best


def capture_report(times: list[float], blind_spans: list[tuple[float, float]]) -> dict:
    """The capture's own cadence: the interval between pictures (ms), and
    the spans it was blind."""
    gaps = [(b - a) * 1000 for a, b in zip(times[:-1], times[1:])]
    blind_ms = sum((b - a) * 1000 for a, b in blind_spans)
    if not gaps:
        return {"interval_ms_p50": None, "interval_ms_max": None, "blind_spans": 0}
    return {
        "interval_ms_p50": round(statistics.median(gaps), 3),
        "interval_ms_max": round(max(gaps), 3),
        "blind_spans": len(blind_spans),
        "blind_ms": round(blind_ms, 3),
    }


def video_read_args(ffmpeg: str, path: Path, width: int, height: int) -> list[str]:
    """ffmpeg's command line for every stored frame of a video, grey at
    ``width`` x ``height``: ``-fps_mode passthrough`` (no frame added or
    dropped to fit a constant rate), each frame's pts on stderr
    (``showinfo``, at ffmpeg's default log level)."""
    graph = f"showinfo,scale={width}:{height}:flags=area,format=gray"
    return [
        ffmpeg, "-hide_banner", "-nostdin", "-copyts", "-i", str(path),
        "-map", "0:v:0", "-vf", graph, "-fps_mode", "passthrough",
        "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1",
    ]  # fmt: skip


def read_video(
    path: Path, width: int | None, x_range: tuple[float, float], ffmpeg: str = "ffmpeg"
) -> tuple[list[Reading], list[float]]:
    """Every picture of a video file, read one at a time (scaled to
    ``width`` if given), and its pts (s)."""
    # The repo's ffmpeg-only size and showinfo readers (no ffprobe on the box).
    from av_sync_check import _info_lines, _pts, probe_video_size

    src_w, src_h = probe_video_size(ffmpeg, str(path))
    out_w = width or src_w
    out_h = round(src_h * out_w / src_w / 2) * 2
    size = out_w * out_h
    readings = []
    with tempfile.TemporaryFile() as log:
        args = video_read_args(ffmpeg, path, out_w, out_h)
        proc = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=log)
        assert proc.stdout is not None
        while chunk := proc.stdout.read(size):
            if len(chunk) < size:
                break
            picture = np.frombuffer(chunk, dtype=np.uint8).reshape(out_h, out_w)
            readings.append(read_frame(picture, x_range))
        code = proc.wait()
        log.seek(0)
        err = log.read().decode(errors="replace")
    if code != 0:
        raise RuntimeError(f"ffmpeg exited {code} reading {path}: {err[-2000:]}")
    times = [_pts(line, str(path)) for line in _info_lines(err, "showinfo")]
    if len(times) != len(readings):
        raise RuntimeError(f"read {len(readings)} pictures but {len(times)} pts")
    return readings, times


def late_polls(
    polls: list[float], late: float = POLL_LATE_S
) -> list[tuple[float, float]]:
    """The spans between two polls more than ``late`` apart: the capture may
    have missed a refresh there (a poll on time that found no new picture
    is the wall holding one)."""
    return [(a, b) for a, b in zip(polls, polls[1:]) if b - a > late]


def capture_dxcam(
    output: int,
    region: tuple[int, int, int, int] | None,
    seconds: float,
    x_range: tuple[float, float],
) -> tuple[list[float], list[np.ndarray], list[tuple[float, float]]]:
    """Every new picture of desktop output ``output`` for ``seconds``
    (Windows, dxcam in its default colour output: no OpenCV needed): its
    capture time and its strip columns as luma (decoded after the capture,
    so the decode never delays a grab), and the spans the capture was blind
    (``late_polls``). A poll that finds nothing sleeps 1 ms (Python 3.11+ on
    Windows sleeps on a high-resolution timer)."""
    import dxcam  # Windows only (the box's ops\probe-venv)

    camera = dxcam.create(output_idx=output)
    times: list[float] = []
    pictures: list[np.ndarray] = []
    polls: list[float] = []
    columns = None
    deadline = time.perf_counter() + seconds
    while (now := time.perf_counter()) < deadline:
        polls.append(now)
        picture = camera.grab(region=region)
        if picture is None:
            time.sleep(0.001)
            continue
        if columns is None:
            columns = cell_columns(picture.shape[1], x_range)
        times.append(now)
        pictures.append(to_luma(picture[:, columns]).astype(np.uint8))
    del camera
    return times, pictures, late_polls(polls)


def parse_pair(text: str) -> tuple[float, float]:
    a, b = (float(v) for v in text.split(","))
    return a, b


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fps", type=int, required=True, choices=clip.FPS_SET)
    parser.add_argument("--grid-fps", type=int, default=GRID_FPS)
    parser.add_argument("--frames", type=int, help="frames in the clip")
    parser.add_argument("--x-range", type=parse_pair, default=(0.0, 1.0))
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--video", type=Path)
    source.add_argument("--dxcam", type=int, metavar="OUTPUT")
    parser.add_argument("--width", type=int, help="scale a video to this width")
    parser.add_argument("--refresh-hz", type=float, help="the refresh rate")
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument("--region", help="dxcam region L,T,R,B")
    args = parser.parse_args(argv)
    if args.video is not None:
        readings, times = read_video(args.video, args.width, args.x_range)
        blind = pts_gaps(times)
    else:
        region = tuple(int(v) for v in args.region.split(",")) if args.region else None
        times, sampled, blind = capture_dxcam(
            args.dxcam, region, args.seconds, args.x_range
        )
        readings = [read_columns(s) for s in sampled]
    report = analyse(
        readings,
        args.fps,
        args.grid_fps,
        args.frames,
        times,
        args.refresh_hz,
        blind,
    )
    print(json.dumps(report, indent=2))
    if not report["decoded"]:
        return 2
    return {"clean": 0, "defects": 1, "incomplete": 3}[report["verdict"]]


if __name__ == "__main__":
    sys.exit(main())
