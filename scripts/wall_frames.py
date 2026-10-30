"""Read SongPlayer's wall measurement clips back from a capture (#272).

``wall_clip.py`` draws each clip frame's index as a 16-cell Gray-code strip
in self-contained copies side by side (v2: nine per band, each with its own
references, under a static ruler), repeated in eight bands. Where the copies
sit in a capture, at whatever zoom and offset the operator's mapping shows
the clip, is ``wall_locate.locate``; each copy keeps the strips right under
its ruler, and only those are read for it.

Each picture is read through its copies' cell sums (``read_sums``): every
strip of every copy, normalised by its own black and white reference cells,
decodes to ``(index, next_index, weight_q8)``:

- a plain frame: every cell black or white -> ``(index, None, 0)``;
- a boundary that blends two consecutive frames (#251): exactly one cell is
  grey, the two candidate codes decode to ``i`` and ``i + 1`` -> ``(i, i + 1,
  w)`` with ``w`` the later frame's weight in Q8, the program's own unit
  (``FrameBlend``: 256 = all of the later frame);
- anything else is named, never guessed: ``no_strip`` (no copy found, or no
  strip row where it was found), ``multi`` (two or more grey cells),
  ``non_adjacent`` (one grey cell whose candidates are not consecutive),
  ``disagree`` (the copies and bands of one picture read different
  positions: a torn picture).

What a run of readings means — boundaries, judder, losses, the blend
weights, the verdict — is ``wall_analysis.analyse``.

The decode is pure (numpy only) and unit-tested on frames drawn by
``wall_clip.draw_frame``. The capture glue is tiny. The copies are found ONCE
per capture, on a picture whose copies read as one picture with more than
half of them (``clip_copies``), and each picture keeps only their cell sums
(``wall_locate.cell_sums``, 20 numbers a row per copy): a video file through
ffmpeg (``av_sync_check``'s reader: every stored frame, with its pts; no
ffprobe; looked at on every ``LOCATE_EVERY``-th picture until found, at most
``LOCATE_TRIES`` times, so a clip that first shows after ~600 pictures, 10 s
at 60 fps, is not found: trim the recording; a pts gap is a span the
recording was blind, and a recording slower than the wall refreshes is
blind everywhere), or the Windows desktop duplication of one output through
``dxcam`` (the box's ``ops\\probe-venv``; ``--refresh-hz`` required): the
copies are looked for on the whole output (or in ``--region``) and the
capture grabs only their strip rows and columns
(``wall_locate.capture_region``); every poll is timed, and a poll more
than half a refresh after the one before is a blind span (a poll on time
that finds no new picture is the wall holding one). A Spout sender
(``--spout``, ``wall_spout``) is read the same way, from a picture that is
off the desktop: only ``SPOUT_ROWS`` rows of the strips, polled with no sleep.

Usage::

    python scripts/wall_frames.py --fps 25 --video rec.mp4 [--width 1920]
                                  [--refresh-hz 60]
    python scripts/wall_frames.py --fps 25 --dxcam 1 --refresh-hz 60
                                  --seconds 20 [--region L,T,R,B]
    python scripts/wall_frames.py --fps 25 --spout "Arena - Screen 2"
                                  --refresh-hz 60 --seconds 20

The report is JSON on stdout (``located``: the copies read, their cell and
the region grabbed); exit 0 = clean, 1 = defects, 3 = incomplete, 4 =
nothing decoded (also: no copy of the clip found, the reason on stderr), 5 =
the capture or its reading failed (the error on stderr), 2 = a usage error:
a broken capture never reads as a broken wall.
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
    POLL_TIME_ERROR,
    SAME_PICTURE,
    Reading,
    analyse,
    check_locked,
    check_present_rate,
)
from wall_locate import (
    CELL_CORE,
    CELL_SAMPLES,
    Copy,
    capture_region,
    cell_sums,
    locate,
    strip_rows,
)
from wall_spout import GlSender, NewPictures

# How long a dxcam capture looks for the copies before its timed polls (s):
# a look at a whole 7680x1080 output takes ~0.6 s on dev1.
LOCATE_S = 10.0
# The rows a Spout capture reads of each strip: 24 rows poll at ~0.6 ms on SNV,
# the whole strip's 150 at ~1.75 ms, over a tenth of a 60 Hz refresh.
SPOUT_ROWS = 24
# A Spout picture lands when Arena finishes it, not on a refresh: intervals of
# 26-41 ms for 33.3 on SNV. At 0.5 and 1 ms the brackets contradict themselves
# (2-41 boundaries a take), 2 and 3 ms read both clips clean, and at 4 ms and
# more wall-25 leaves boundaries unresolved (30.10.2026).
SPOUT_TIME_ERROR = 0.003
# A recording is looked at on every LOCATE_EVERY-th picture until the copies
# are found (a look takes 0.1-3 s), at most LOCATE_TRIES times; the pictures
# between two looks are kept and read once the copies are found.
LOCATE_EVERY = 15
LOCATE_TRIES = 40
# Copies of one capture are of one scale, within this share of a cell.
SCALE_MATCH = 0.01
# main()'s exit codes (2 is argparse's usage error).
EXIT_CODES = {"clean": 0, "defects": 1, "incomplete": 3}
EXIT_NOTHING_DECODED = 4
EXIT_FAILED = 5


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


def read_sums(sums: np.ndarray, copies: list[Copy]) -> Reading:
    """Decode one picture from its copies' cell sums (``cell_sums``): each
    copy's strip rows that overlap the strips it was found with (another
    slice of the output cut in beside them is never read for it), then
    their consensus (they must read the same picture, ``SAME_PICTURE``)."""
    if sums.shape[1:] != (len(copies), clip.CELLS):
        raise ValueError(f"cell sums {sums.shape} are not of {len(copies)} copies")
    readings = []
    for k, copy in enumerate(copies):
        profile = sums[:, k, :] / CELL_SAMPLES
        for y0, y1 in strip_rows(profile):
            if any(y0 < s1 and s0 < y1 for s0, s1 in copy.strips):
                readings.append(decode_levels(strip_levels(profile, (y0, y1))))
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
    """Decode one captured 8-bit picture: find its copies, then read them
    (none found: ``no_strip``)."""
    copies = locate(image)
    return read_sums(cell_sums(image, copies), copies)


def same_scale(copies: list[Copy]) -> list[Copy]:
    """The largest group of copies whose cells are within ``SCALE_MATCH`` of
    each other (a tie: the larger cells). One mapping shows every copy at one
    scale; a copy cut by a seam of the output reads a narrower cell, its
    samples sit near its cells' edges, and each change of a bit reads there
    as a blend (SNV, 58.33 px beside 60: `disagree` at every 128th frame, on
    Spout and NDI alike)."""
    groups = [
        [c for c in copies if abs(c.cell / copy.cell - 1) <= SCALE_MATCH]
        for copy in copies
    ]
    return max(groups, key=lambda g: (len(g), g[0].cell), default=[])


def clip_copies(picture: np.ndarray) -> list[Copy]:
    """The copies a capture reads every picture at: those of ``picture``
    that read it as one picture (within ``SAME_PICTURE``) with MORE than half
    of the copies that read it at all. A copy found where none is (an
    overlay drawing a ruler over a strip) reads another index and is left
    out; with no such majority (a torn picture, two copies reading two
    pictures), none: the caller looks at another picture."""
    good = []
    for copy in same_scale(locate(picture)):
        reading = read_sums(cell_sums(picture, [copy]), [copy])
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
    a 4K recording is never held whole. The copies are looked for on every
    ``LOCATE_EVERY``-th picture from the first, and on the last, until found
    (``clip_copies``), at most ``LOCATE_TRIES`` times; the pictures since the
    look before are kept and read at the copies found (``no_strip`` if none
    were). ffmpeg is killed if the read fails."""
    src_w, src_h = av_sync_check.probe_video_size(ffmpeg, str(path))
    out_w = width or src_w
    out_h = round(src_h * out_w / src_w / 2) * 2
    size = out_w * out_h
    readings: list[Reading] = []
    copies: list[Copy] = []
    waiting: list[np.ndarray] = []
    looks = 0

    def look() -> None:
        """Look for the copies on the last waiting picture, then read every
        waiting picture at what was found."""
        nonlocal copies, looks
        looks += 1
        copies = clip_copies(waiting[-1])
        for seen in waiting:
            readings.append(read_sums(cell_sums(seen, copies), copies))
        waiting.clear()

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
                    if copies:
                        readings.append(read_sums(cell_sums(picture, copies), copies))
                        continue
                    if looks == LOCATE_TRIES:
                        readings.append(Reading("no_strip"))
                        continue
                    waiting.append(picture)
                    if (len(readings) + len(waiting) - 1) % LOCATE_EVERY == 0:
                        look()
                if waiting:
                    look()
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


def find_clip(
    grab: Callable[..., np.ndarray | None],
    region: tuple[int, int, int, int] | None,
    seconds: float,
    clock: Callable[[], float] = time.perf_counter,
    pause: Callable[[float], None] = time.sleep,
    max_rows: int | None = None,
) -> Located | None:
    """Grab pictures (``region``, or the whole output) until one shows copies
    that read as one picture (``clip_copies``), for at most ``seconds``:
    the copies and the region to capture, the part of the grabbed area that
    ``capture_region`` picks (its strip rows and columns); None if no picture
    showed them. ``grab`` takes the region by keyword, as dxcam's does; a
    grab that finds no new picture pauses 1 ms. ``max_rows`` as
    ``capture_region``'s."""
    left, top = region[:2] if region else (0, 0)
    deadline = clock() + seconds
    while clock() < deadline:
        picture = grab(region=region)
        if picture is None:
            pause(0.001)
            continue
        copies = clip_copies(picture)
        if not copies:
            continue
        shape = np.asarray(picture).shape
        (x0, y0, x1, y1), inside = capture_region(copies, shape, max_rows)
        return Located(tuple(inside), (left + x0, top + y0, left + x1, top + y1))
    return None


def capture_dxcam(
    output: int, region: tuple[int, int, int, int] | None, seconds: float
) -> tuple[list[float], list[np.ndarray], list[float], list[float], Located | None]:
    """Every new picture of desktop output ``output`` for ``seconds``
    (Windows, dxcam with ``output_color="BGRA"``: no colour conversion, so
    dxcam 0.3+ never loads OpenCV; dxcam 0.0.5 imports it whatever the
    colour), after ``find_clip`` found the copies (``LOCATE_S``): its poll's
    time (stamped before the grab), the copies' cell sums (``cell_sums``,
    decoded after the capture, so the decode never delays a grab), every
    poll's time, when each picture's acquire was over at the latest
    (``acquire_ended``), and what was located (None: no copy found, nothing
    captured). The frame was on screen between the stamp and that end — a
    grab preempted before its acquire must not move the picture early
    (review round 7). The end is stamped right after dxcam's
    ``AcquireNextFrame`` call when this dxcam exposes it, else after the
    whole grab. A poll that finds nothing sleeps 1 ms (Python 3.11+ on
    Windows sleeps on a high-resolution timer)."""
    import dxcam  # Windows only (the box's ops\probe-venv)

    camera = dxcam.create(output_idx=output, output_color="BGRA")
    return capture_camera(camera, region, seconds)


def capture_spout(
    sender: str, region: tuple[int, int, int, int] | None, seconds: float
) -> tuple[list[float], list[np.ndarray], list[float], list[float], Located | None]:
    """``capture_dxcam``'s five results from a Spout sender (Windows)."""
    source = GlSender(sender)
    try:
        return capture_camera(
            NewPictures(source), region, seconds, max_rows=SPOUT_ROWS, idle=0.0
        )
    finally:
        source.close()


def capture_camera(
    camera,
    region: tuple[int, int, int, int] | None,
    seconds: float,
    max_rows: int | None = None,
    idle: float = 0.001,
) -> tuple[list[float], list[np.ndarray], list[float], list[float], Located | None]:
    """The timed polls of ``capture_dxcam`` on anything with its ``grab``.
    ``idle``: what a poll that finds nothing sleeps; ``max_rows`` as
    ``capture_region``'s."""
    acquired = time_acquire(camera)
    times: list[float] = []
    pictures: list[np.ndarray] = []
    polls: list[float] = []
    acquire_ended: list[float] = []
    located = find_clip(camera.grab, region, LOCATE_S, max_rows=max_rows)
    if located is None:
        del camera
        return times, pictures, polls, acquire_ended, None
    copies = list(located.copies)
    deadline = time.perf_counter() + seconds
    while (now := time.perf_counter()) < deadline:
        polls.append(now)
        acquired[0] = None
        picture = camera.grab(region=located.region)
        if picture is None:
            time.sleep(idle)
            continue
        ended = acquired[0] if acquired[0] is not None else time.perf_counter()
        times.append(now)
        acquire_ended.append(ended)
        pictures.append(cell_sums(picture, copies))
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
    spout = False
    if args.video is not None:
        readings, times, located = read_video(args.video, args.width)
        if located is None:
            print(
                f"wall_frames: no copy of the clip's strip found in {args.video} "
                f"(looked at most {LOCATE_TRIES} times, every {LOCATE_EVERY}th "
                "picture from the first)",
                file=sys.stderr,
            )
        if args.locked:
            check_locked(times, args.refresh_hz)
        blind = None  # since the picture before
        span = None  # the first picture's pts to the last one's
    else:
        if args.spout is not None:
            source = f"sender {args.spout!r}"
            times, sums, polls, ended, located = capture_spout(
                args.spout, args.region, args.seconds
            )
        else:
            source = f"output {args.dxcam}"
            times, sums, polls, ended, located = capture_dxcam(
                args.dxcam, args.region, args.seconds
            )
        if located is None:
            print(
                f"wall_frames: no copy of the clip's strip found on {source} "
                f"in {LOCATE_S:g} s (region {args.region}): nothing "
                "captured",
                file=sys.stderr,
            )
            report = analyse([], args.fps, args.grid_fps, args.frames)
            report["located"] = None
            return report
        readings = [read_sums(s, list(located.copies)) for s in sums]
        blind = poll_blind(times, polls)
        late = [max(0.0, e - t) for t, e in zip(times, ended)]
        spout = args.spout is not None
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
        time_error=SPOUT_TIME_ERROR if spout else POLL_TIME_ERROR,
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
    source.add_argument("--spout", metavar="SENDER")
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
        help="area L,T,R,B to look for the clip in (default: the whole picture)",
    )
    parser.add_argument(
        "--min-boundaries",
        type=int,
        default=MIN_BOUNDARIES,
        help="boundaries a clean capture judges for judder",
    )
    args = parser.parse_args(argv)
    if args.locked and args.video is None:
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
