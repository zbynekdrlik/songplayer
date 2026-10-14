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
report: boundaries and the refreshes each was shown for, lost frames, lost
boundaries, jumps back, cadence errors, loops, and the blend weights against
the pacer's rule for the clip's fps on SongPlayer's 30 fps grid
(``.claude/rules/genlock.md``: 25 on 30 = plain, 213, 170, 128, 85, 42), at
the grid phase fitted per pass through the clip. The clip LOOPS (the test
playlist's mode): the song's end and the new play are not the measured
cadence, so the steps at a loop seam are reported under ``loop_seams`` and
kept out of the verdict; a jump to the start from mid-clip is a jump back.

The decode is pure (numpy only) and unit-tested on frames drawn by
``wall_clip.draw_frame``. The capture glue is tiny and untested: a video
file through ffmpeg, or the Windows desktop duplication of one output
through ``dxcam`` (the box's ``ops\\probe-venv``), polled for every refresh.

Usage::

    python scripts/wall_frames.py --fps 25 --video rec.mp4 [--width 1920]
    python scripts/wall_frames.py --fps 25 --dxcam 1 --seconds 20
                                  [--region L,T,R,B] [--x-range 0,1]

The report is JSON on stdout; exit 0 = clean, 1 = defects, 2 = nothing
decoded.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import subprocess
import sys
import time
from collections import Counter
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path
from typing import Iterable, Iterator

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
# The central part of a cell that is sampled (of its width), and of a strip
# row's height.
CELL_CORE = 0.5
MIN_STRIP_ROWS = 3
# The bands of one picture agree when their positions are this close (Q8).
AGREE_Q8 = 8
# analyse(): a step or a weight off by more than this (Q8) is an error.
Q8_TOLERANCE = 8
# A loop: the clip's last WRAP_FRAMES frames to its first WRAP_FRAMES.
WRAP_FRAMES = 3
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


def cell_slices(width: int, x_range: tuple[float, float]) -> list[slice]:
    """The sampled columns of each of the strip's cells, in a ``width`` px
    capture whose clip spans ``x_range`` (fractions of the capture width)."""
    x0 = x_range[0] * width
    span = (x_range[1] - x_range[0]) * width
    edges = clip.cell_fractions()
    slices = []
    for left, right in zip(edges[:-1], edges[1:]):
        centre = x0 + span * (left + right) / 2
        half = max(0.5, span * (right - left) * CELL_CORE / 2)
        start = max(0, int(math.floor(centre - half)))
        stop = min(width, int(math.ceil(centre + half)))
        if stop <= start:
            raise ValueError("the strip's cells fall outside the capture")
        slices.append(slice(start, stop))
    return slices


def cell_profile(luma: np.ndarray, slices: list[slice]) -> np.ndarray:
    """Per picture row, the mean of each cell's sampled columns: (rows, 20)."""
    return np.stack([luma[:, s].mean(axis=1) for s in slices], axis=1)


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


def read_frame(image: np.ndarray, x_range: tuple[float, float] = (0.0, 1.0)) -> Reading:
    """Decode one captured picture: every strip row, then their consensus."""
    luma = to_luma(image)
    profile = cell_profile(luma, cell_slices(luma.shape[1], x_range))
    runs = strip_rows(profile)
    if not runs:
        return Reading("no_strip")
    readings = [decode_levels(strip_levels(profile, rows)) for rows in runs]
    good = [r for r in readings if r.ok]
    if not good:
        status = Counter(r.status for r in readings).most_common(1)[0][0]
        return Reading(status, strips=len(runs))
    positions = sorted(r.position() for r in good)
    if positions[-1] - positions[0] > AGREE_Q8 / 256:
        return Reading("disagree", strips=len(runs))
    median = positions[(len(positions) - 1) // 2]
    chosen = next(r for r in good if r.position() == median)
    return Reading("ok", chosen.index, chosen.next_index, chosen.weight_q8, len(runs))


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
    reading: Reading
    refreshes: int = 1


def boundaries(readings: list[Reading]) -> list[Boundary]:
    """Group the decoded readings into the boundaries they show: a reading
    within ``Q8_TOLERANCE`` of the one before shows the same picture (a
    weight near 256 can read as the plain next frame). An undecoded reading
    between two such readings does not split them."""
    runs: list[Boundary] = []
    for n, reading in enumerate(readings):
        if not reading.ok:
            continue
        if runs and abs(runs[-1].reading.position() - reading.position()) <= (
            Q8_TOLERANCE / 256
        ):
            runs[-1].refreshes += 1
        else:
            runs.append(Boundary(n, reading))
    return runs


@dataclass(frozen=True)
class Step:
    """From one boundary to the next."""

    sample: int
    delta: float
    count: int
    wrapped: bool
    kind: str | None


def steps_between(
    runs: list[Boundary], n_frames: int, fps: int, grid_fps: int = GRID_FPS
) -> list[Step]:
    """Each step's clip advance (frames) and its grid boundaries, judged:
    ``jump_back`` (the clip went back), ``cadence`` (not a whole number of
    grid steps), ``lost`` (two or more grid steps: a boundary never shown)
    or ``None``. The clip's loop (its last frames to its first) is
    ``wrapped``, never a jump back."""
    step = fps / grid_fps
    tolerance = Q8_TOLERANCE / 256
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
        kind = None
        if delta < -tolerance:
            kind = "jump_back"
        elif count == 0 or abs(delta - count * step) > tolerance:
            kind = "cadence"
        elif count > 1:
            kind = "lost"
        out.append(Step(run.first_sample, delta, count, wrapped, kind))
    return out


def seam_steps(steps: list[Step]) -> set[int]:
    """The steps at a loop seam: the wrap itself and the step on each side
    (the song's end and the new play are not the measured cadence)."""
    seam: set[int] = set()
    for n, s in enumerate(steps):
        if s.wrapped:
            seam.update(k for k in (n - 1, n, n + 1) if 0 <= k < len(steps))
    return seam


def analyse(
    readings: list[Reading],
    fps: int,
    grid_fps: int = GRID_FPS,
    frames: int | None = None,
    times: list[float] | None = None,
) -> dict:
    """The report of a capture of the ``fps`` clip (see the module doc).

    Errors at a loop seam (``seam_steps``) are reported under
    ``loop_seams``; the verdict counts the steady cadence only."""
    n_frames = clip.frame_count(fps, clip.CLIP_SECONDS) if frames is None else frames
    runs = boundaries(readings)
    steps = steps_between(runs, n_frames, fps, grid_fps)
    seam = seam_steps(steps)
    steady = [s for n, s in enumerate(steps) if n not in seam]
    at_seam = [s for n, s in enumerate(steps) if n in seam]
    jumps = [s.sample for s in steady if s.kind == "jump_back"]
    lost_frames, seam_lost = lost_frame_list(runs, steps, n_frames)
    weights = weight_report(runs, steps, seam, fps, grid_fps)
    report: dict = {
        "fps": fps,
        "grid_fps": grid_fps,
        "frames_in_clip": n_frames,
        "samples": len(readings),
        "decoded": sum(r.ok for r in readings),
        "undecoded": dict(Counter(r.status for r in readings if not r.ok)),
        "boundaries": len(runs),
        "refreshes_per_boundary": {
            str(k): v for k, v in sorted(Counter(b.refreshes for b in runs).items())
        },
        "frames_seen": len(frames_seen(runs, steps, n_frames)),
        "lost_frames": len(lost_frames),
        "lost_frame_list": lost_frames[:LIST_MAX],
        "lost_boundaries": sum(s.count - 1 for s in steady if s.kind == "lost"),
        "jumps_back": len(jumps),
        "jump_back_samples": jumps[:LIST_MAX],
        "cadence_errors": sum(s.kind == "cadence" for s in steady),
        "loops": sum(s.wrapped for s in steps),
        "loop_seams": {
            "steps": len(at_seam),
            "errors": sum(s.kind is not None for s in at_seam)
            + seam_lost
            + weights.pop("seam_mismatches"),
        },
        "weights": weights,
    }
    if times is not None:
        report["capture"] = capture_report(times)
    defects = (
        report["lost_frames"]
        + report["lost_boundaries"]
        + report["jumps_back"]
        + report["cadence_errors"]
        + weights["mismatches"]
    )
    report["verdict"] = "clean" if defects == 0 and runs else "defects"
    return report


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


def lost_frame_list(
    runs: list[Boundary], steps: list[Step], n_frames: int
) -> tuple[list[int], int]:
    """The clip frames no boundary showed between two that were shown (the
    steady ones), and how many more were lost across a loop seam."""
    ordered = sorted(frames_seen(runs, steps, n_frames))
    lost: list[int] = []
    at_seam = 0
    for a, b in zip(ordered[:-1], ordered[1:]):
        if a // n_frames != b // n_frames:
            at_seam += b - a - 1
        else:
            lost.extend(frame % n_frames for frame in range(a + 1, b))
    return lost, at_seam


def weight_report(
    runs: list[Boundary],
    steps: list[Step],
    seam: set[int],
    fps: int,
    grid_fps: int,
) -> dict:
    """The observed weights against the pacer's rule, at the phase fitted
    per loop (a new play may start on another phase). A boundary next to a
    seam step counts under ``seam_mismatches``."""
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
    for segment in loop_segments(runs, steps):
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


def loop_segments(runs: list[Boundary], steps: list[Step]) -> list[list[int]]:
    """The boundaries of each pass through the clip, split at its loops."""
    segments: list[list[int]] = [[]] if runs else []
    for n in range(len(runs)):
        if n and steps[n - 1].wrapped:
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
    """How far (Q8, modulo whole frames: a frame off is the step check's)
    a boundary's position is from what the pacer shows ``count`` grid steps
    into a grid at ``phase``."""
    expected = phase + count * Fraction(fps, grid_fps)
    shown = math.floor(expected) + Fraction(blend_q8(expected, fps, grid_fps), 256)
    off = (position - float(shown)) % 1
    return 256 * min(off, 1 - off)


def capture_report(times: list[float]) -> dict:
    """The capture's own cadence: the interval between pictures (ms)."""
    gaps = [(b - a) * 1000 for a, b in zip(times[:-1], times[1:])]
    if not gaps:
        return {"interval_ms_p50": None, "interval_ms_max": None, "gaps": 0}
    p50 = statistics.median(gaps)
    return {
        "interval_ms_p50": round(p50, 3),
        "interval_ms_max": round(max(gaps), 3),
        "gaps": sum(g > 1.5 * p50 for g in gaps),
    }


def frames_from_video(
    path: Path, width: int | None, ffmpeg: str = "ffmpeg", ffprobe: str = "ffprobe"
) -> Iterator[np.ndarray]:
    """Every picture of a video file as grey (scaled to ``width`` if given)."""
    query = ["-select_streams", "v:0", "-show_entries", "stream=width,height"]
    probe = subprocess.run(
        [ffprobe, "-v", "error", *query, "-of", "csv=p=0", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    src_w, src_h = (int(v) for v in probe.stdout.strip().split(","))
    out_w = width or src_w
    out_h = round(src_h * out_w / src_w / 2) * 2
    scale = ["-vf", f"scale={out_w}:{out_h}"]
    raw = ["-f", "rawvideo", "-pix_fmt", "gray", "-"]
    args = [ffmpeg, "-v", "error", "-i", str(path), *scale, *raw]
    proc = subprocess.Popen(args, stdout=subprocess.PIPE)
    assert proc.stdout is not None
    size = out_w * out_h
    while chunk := proc.stdout.read(size):
        if len(chunk) < size:
            break
        yield np.frombuffer(chunk, dtype=np.uint8).reshape(out_h, out_w)
    if proc.wait() != 0:
        raise RuntimeError(f"ffmpeg exited {proc.returncode} reading {path}")


def frames_from_dxcam(
    output: int, region: tuple[int, int, int, int] | None, seconds: float
) -> Iterator[tuple[float, np.ndarray]]:
    """Every new picture of desktop output ``output`` for ``seconds``
    (Windows, dxcam), with its capture time. Polled without a sleep: a
    refresh the poll misses reads as a one-refresh boundary."""
    import dxcam  # Windows only (the box's ops\probe-venv)

    camera = dxcam.create(output_idx=output, output_color="GRAY")
    deadline = time.perf_counter() + seconds
    while (now := time.perf_counter()) < deadline:
        picture = camera.grab(region=region)
        if picture is not None:
            yield now, picture
    del camera


def read_all(
    pictures: Iterable[np.ndarray], x_range: tuple[float, float]
) -> list[Reading]:
    return [read_frame(p, x_range) for p in pictures]


def parse_pair(text: str) -> tuple[float, float]:
    a, b = (float(v) for v in text.split(","))
    return a, b


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fps", type=int, required=True, help="the clip's fps")
    parser.add_argument("--grid-fps", type=int, default=GRID_FPS)
    parser.add_argument("--frames", type=int, help="frames in the clip")
    parser.add_argument("--x-range", type=parse_pair, default=(0.0, 1.0))
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--video", type=Path)
    source.add_argument("--dxcam", type=int, metavar="OUTPUT")
    parser.add_argument("--width", type=int, help="scale a video to this width")
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument("--region", help="dxcam region L,T,R,B")
    args = parser.parse_args(argv)
    times = None
    if args.video is not None:
        readings = read_all(frames_from_video(args.video, args.width), args.x_range)
    else:
        region = tuple(int(v) for v in args.region.split(",")) if args.region else None
        times, readings = [], []
        for t, picture in frames_from_dxcam(args.dxcam, region, args.seconds):
            times.append(t)
            readings.append(read_frame(picture, args.x_range))
    report = analyse(readings, args.fps, args.grid_fps, args.frames, times)
    print(json.dumps(report, indent=2))
    if not report["decoded"]:
        return 2
    return 0 if report["verdict"] == "clean" else 1


if __name__ == "__main__":
    sys.exit(main())
