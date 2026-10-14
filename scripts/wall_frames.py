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

What a run of readings means — boundaries, judder, losses, the blend
weights, the verdict — is ``wall_analysis.analyse``.

The decode is pure (numpy only) and unit-tested on frames drawn by
``wall_clip.draw_frame``. The capture glue is tiny: a video file through
ffmpeg (``av_sync_check``'s reader: every stored frame, with its pts;
no ffprobe; a pts gap is a span the recording was blind, and a recording
slower than the wall refreshes is blind everywhere), or the Windows
desktop duplication of one output through ``dxcam`` (the box's
``ops\\probe-venv``; ``--refresh-hz`` required): only each picture's
sampled columns are kept, as luma, and decoded AFTER the capture; every
poll is timed, and a poll more than half a refresh after the one before is
a blind span (a poll on time that finds no new picture is the wall holding
one).

Usage::

    python scripts/wall_frames.py --fps 25 --video rec.mp4 [--width 1920]
                                  [--refresh-hz 60]
    python scripts/wall_frames.py --fps 25 --dxcam 1 --refresh-hz 60
                                  --seconds 20 [--region L,T,R,B]
                                  [--x-range 0,1]

The report is JSON on stdout; exit 0 = clean, 1 = defects, 3 = incomplete,
4 = nothing decoded, 5 = the capture or its reading failed (the error on
stderr), 2 = a usage error: a broken capture never reads as a broken wall.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
import tempfile
import time
import traceback
from collections import Counter
from pathlib import Path

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
    check_present_rate,
    late_polls,
    video_blind_spans,
)

FULL_SCALE = 255.0
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
# main()'s exit codes (2 is argparse's usage error).
EXIT_CODES = {"clean": 0, "defects": 1, "incomplete": 3}
EXIT_NOTHING_DECODED = 4
EXIT_FAILED = 5


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


def read_video(
    path: Path, width: int | None, x_range: tuple[float, float], ffmpeg: str = "ffmpeg"
) -> tuple[list[Reading], list[float]]:
    """Every picture of a video file, read one at a time (scaled to
    ``width`` if given), and its pts (s): ``av_sync_check``'s command line
    and readers of the size and pts (the box has no ffprobe), streamed, so
    a 4K recording is never held whole. ffmpeg is killed if the read
    fails."""
    src_w, src_h = av_sync_check.probe_video_size(ffmpeg, str(path))
    out_w = width or src_w
    out_h = round(src_h * out_w / src_w / 2) * 2
    size = out_w * out_h
    readings = []
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
                    readings.append(read_frame(picture.reshape(out_h, out_w), x_range))
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
    return readings, times


def capture_dxcam(
    output: int,
    region: tuple[int, int, int, int] | None,
    seconds: float,
    x_range: tuple[float, float],
) -> tuple[list[float], list[np.ndarray], list[float]]:
    """Every new picture of desktop output ``output`` for ``seconds``
    (Windows, dxcam with ``output_color="BGRA"``: no colour conversion, so
    dxcam 0.3+ never loads OpenCV; dxcam 0.0.5 imports it whatever the
    colour): its capture time and its strip columns as luma (decoded after
    the capture, so the decode never delays a grab), and every poll's time.
    A poll that finds nothing sleeps 1 ms (Python 3.11+ on Windows sleeps
    on a high-resolution timer)."""
    import dxcam  # Windows only (the box's ops\probe-venv)

    camera = dxcam.create(output_idx=output, output_color="BGRA")
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
    return times, pictures, polls


def parse_pair(text: str) -> tuple[float, float]:
    a, b = (float(v) for v in text.split(","))
    return a, b


def capture(args: argparse.Namespace) -> dict:
    """Capture or read the pictures ``args`` names, then the report."""
    if args.video is not None:
        readings, times = read_video(args.video, args.width, args.x_range)
        blind = video_blind_spans(times, args.refresh_hz)
        span = None  # the first picture's pts to the last one's
    else:
        region = tuple(int(v) for v in args.region.split(",")) if args.region else None
        times, sampled, polls = capture_dxcam(
            args.dxcam, region, args.seconds, args.x_range
        )
        readings = [read_columns(s) for s in sampled]
        # A picture's time is its poll's: a poll more than half a refresh
        # late can no longer time it to the refresh.
        blind = late_polls(polls, 0.5 / args.refresh_hz)
        check_present_rate(times, args.refresh_hz, blind)
        span = (polls[0], polls[-1]) if polls else None
    return analyse(
        readings,
        args.fps,
        args.grid_fps,
        args.frames,
        times,
        args.refresh_hz,
        blind,
        span,
        args.min_boundaries,
    )


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
    parser.add_argument("--refresh-hz", type=float, help="the wall's refresh rate")
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument("--region", help="dxcam region L,T,R,B")
    parser.add_argument(
        "--min-boundaries",
        type=int,
        default=MIN_BOUNDARIES,
        help="boundaries a clean capture judges for judder",
    )
    args = parser.parse_args(argv)
    if args.dxcam is not None and not args.refresh_hz:
        # dxcam's 1 ms polls skew a rate read from its pictures (58.8 Hz
        # for 60): the wall's own rate is the reference.
        parser.error("--dxcam needs the wall's --refresh-hz")
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
