"""Shared fakes for the wall reader's tests (#272): the clip's own frames,
what a wall shows of them (any zoom and offset, scaled the way a renderer
scales: bilinear, and blurred), the program's byte blend, the pacer's
readings, the wall's refreshes as a camera samples them, and a simulated
dxcam capture."""

from __future__ import annotations

import dataclasses
import functools
import math
from fractions import Fraction

import numpy as np

import wall_clip as clip
import wall_analysis as wa
import wall_frames as wf


@functools.lru_cache(maxsize=None)
def _frame(index: int, fps: int, width: int, height: int) -> np.ndarray:
    picture = clip.draw_frame(index, fps, clip.layout(width, height))
    picture.setflags(write=False)
    return picture


def frame(index: int, fps: int = 25, width: int = 960, height: int = 540):
    return _frame(index, fps, width, height)


def blend(a: np.ndarray, b: np.ndarray, weight: float) -> np.ndarray:
    """``b`` over ``a`` at ``weight``, rounded to 8 bits like the program."""
    mixed = a.astype(np.float64) * (1 - weight) + b.astype(np.float64) * weight
    return np.clip(np.round(mixed), 0, 255).astype(np.uint8)


def wall_view(
    picture: np.ndarray,
    zoom: float,
    left: float,
    top: float,
    width: int,
    height: int,
    zoom_y: float | None = None,
) -> np.ndarray:
    """What a ``width`` x ``height`` output shows of ``picture`` scaled by
    ``zoom`` across (``zoom_y`` down, default the same) from its pixel
    ``(left, top)``: output pixel ``(x, y)`` shows ``(left + (x + 0.5) /
    zoom - 0.5, ...)``, bilinear (Arena's scaler), edges clamped, 8 bits."""
    zoom_y = zoom if zoom_y is None else zoom_y
    rows, cols = picture.shape
    xs = np.clip(left + (np.arange(width) + 0.5) / zoom - 0.5, 0, cols - 1)
    ys = np.clip(top + (np.arange(height) + 0.5) / zoom_y - 0.5, 0, rows - 1)
    x0, y0 = np.floor(xs).astype(int), np.floor(ys).astype(int)
    x1, y1 = np.minimum(x0 + 1, cols - 1), np.minimum(y0 + 1, rows - 1)
    fx, fy = (xs - x0)[None, :], (ys - y0)[:, None]
    p = picture.astype(np.float64)
    upper = p[y0][:, x0] * (1 - fx) + p[y0][:, x1] * fx
    lower = p[y1][:, x0] * (1 - fx) + p[y1][:, x1] * fx
    return np.clip(np.round(upper * (1 - fy) + lower * fy), 0, 255).astype(np.uint8)


def blurred(picture: np.ndarray, radius: int) -> np.ndarray:
    """A box blur of ``2 * radius + 1`` px each way (edges clamped)."""
    out = picture.astype(np.float64)
    size = 2 * radius + 1
    for axis in (0, 1):
        pad = [(0, 0), (0, 0)]
        pad[axis] = (radius, radius)
        padded = np.pad(out, pad, mode="edge")
        sums = np.cumsum(padded, axis=axis)
        sums = np.insert(sums, 0, 0, axis=axis)
        out = (
            np.take(sums, range(size, sums.shape[axis]), axis=axis)
            - np.take(sums, range(0, sums.shape[axis] - size), axis=axis)
        ) / size
    return np.clip(np.round(out), 0, 255).astype(np.uint8)


def noisy(picture: np.ndarray, sigma: float, seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    out = picture.astype(np.float64) + rng.normal(0, sigma, picture.shape)
    return np.clip(np.round(out), 0, 255).astype(np.uint8)


def flips_up(index: int) -> bool:
    """Whether the cell that changes from ``index`` to ``index + 1`` goes
    black -> white."""
    return clip.gray(index + 1) > clip.gray(index)


def program_level(shown: int, later: int, q8: int) -> float:
    """A strip cell's level after the program's byte blend (``nv12_mix.rs``:
    ``(f·(256 − w) + t·w + 128) >> 8``) of black / white luma."""
    f = clip.Y_WHITE if shown else clip.Y_BLACK
    t = clip.Y_WHITE if later else clip.Y_BLACK
    y = (f * (256 - q8) + t * q8 + 128) >> 8
    return (y - clip.Y_BLACK) / (clip.Y_WHITE - clip.Y_BLACK)


def reading_at(position: Fraction, fps: int, frames: int) -> wa.Reading:
    """What the reader reads of the pacer's boundary at clip ``position``:
    the strip blended by the program's own 8-bit arithmetic at the pacer's
    weight, read by the reader's level rule."""
    index = int(position)
    q8 = wa.blend_q8(position, fps)
    if index + 1 >= frames:
        q8 = 0  # the song's end: no next frame to blend
    shown = clip.code_bits(index)
    later = clip.code_bits(index + 1) if q8 else shown
    levels = np.array([program_level(a, b, q8) for a, b in zip(shown, later)])
    return dataclasses.replace(wf.decode_levels(levels), strips=8)


def pacer_readings(
    fps: int, start: Fraction, boundaries: int, frames: int, refreshes: int = 2
) -> list[wa.Reading]:
    """What a capture of the pacer's output reads: one reading per refresh,
    ``refreshes`` per boundary, from clip position ``start``. The clip loops
    after ``frames`` frames; the loop's new play starts on a boundary."""
    out = []
    position = start
    step = Fraction(fps, wa.GRID_FPS)
    for _ in range(boundaries):
        out.extend([reading_at(position, fps, frames)] * refreshes)
        position += step
        if position >= frames:
            position = Fraction(0)
    return out


def at_60_hz(readings: list[wa.Reading]) -> list[float]:
    return [n / 60 for n in range(len(readings))]


def wall_refreshes(
    counts: list[int], fps: int, start: Fraction = Fraction(100), frames: int = 1500
) -> list[wa.Reading]:
    """The wall's refreshes, one reading each: the pacer's boundary ``k``
    (from clip position ``start``) on screen ``counts[k]`` refreshes."""
    step = Fraction(fps, wa.GRID_FPS)
    out: list[wa.Reading] = []
    for k, count in enumerate(counts):
        out.extend([reading_at(start + k * step, fps, frames)] * count)
    return out


def camera(
    refreshes: list[wa.Reading], wall_hz: float, cam_fps: float, offset: float
) -> tuple[list[wa.Reading], list[float]]:
    """A camera (or recorder) sampling the wall at ``cam_fps`` from
    ``offset`` s: what the refresh on screen at each sample instant shows,
    and the instants."""
    readings, times = [], []
    j = 0
    while (r := math.floor((t := offset + j / cam_fps) * wall_hz)) < len(refreshes):
        readings.append(refreshes[r])
        times.append(t)
        j += 1
    return readings, times


def judder(n: int, every: int = 20) -> list[int]:
    """Refresh counts at 60 Hz on the 30 fps grid: 2 each, but every
    ``every``-th pair 1 + 3."""
    counts = [2] * n
    for k in range(0, n - 1, every):
        counts[k], counts[k + 1] = 1, 3
    return counts


def dxcam_capture(
    late: tuple[float, float] | None = None,
    after: float = 0.0,
    grab: float = 0.001,
    held: tuple[float, float] | None = None,
):
    """What ``capture_dxcam`` returns for the 30 fps clip on a 60 Hz output
    (two presents per boundary, frames 100..129): a poll every millisecond
    gets the newest present since the last one, acquired at its stamp.
    ``late``: no poll in that span; ``after``: polls run on that long after
    the last present; ``grab``: what a poll that got a picture costs;
    ``held`` ``(at, delay)``: the first poll from ``at`` acquires ``delay``
    after its stamp (preempted between the two). The copies are located
    once, on the clip's first frame, as ``capture_dxcam`` does."""
    presents = [(n / 60 + 0.0005, 100 + n // 2) for n in range(60)]
    end = presents[-1][0] + 0.002 + after
    times, sampled, polls, ended = [], [], [], []
    copies = wf.clip_copies(frame(100, 30))
    seen = -1
    t = 0.0
    while t < end:
        polls.append(t)
        taken = t
        if held and held[0] <= t:
            taken, held = t + held[1], None
        newest = max(
            (k for k, (at, _) in enumerate(presents) if at <= taken), default=-1
        )
        if newest > seen:
            seen = newest
            times.append(t)
            ended.append(taken)
            sampled.append(wf.sample_copies(frame(presents[newest][1], 30), copies))
            t = max(t + grab - 0.001, taken)
        t += 0.001
        if late and late[0] <= t < late[1]:
            t = late[1]
    return times, sampled, polls, ended, wf.Located(tuple(copies), None)
