"""Shared fakes for the wall reader's tests (#272): the clip's own frames,
the program's byte blend, the pacer's readings and a simulated dxcam
capture."""

from __future__ import annotations

import dataclasses
import functools
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


# Review round 3, the hand mutants: what each threshold and guard decides.


def two_boundaries(gap_refreshes: float, unread: int = 0):
    """Two boundaries, ``gap_refreshes`` apart at 60 Hz (times), with
    ``unread`` undecoded pictures between them."""
    prev = wa.Boundary(0, 0, wa.Reading("ok", 10, None, 0, 8))
    run = wa.Boundary(unread + 1, unread + 1, wa.Reading("ok", 12, None, 0, 8))
    times = [0.0] + [None] * unread + [gap_refreshes / 60]
    return prev, run, times


def dxcam_capture(
    late: tuple[float, float] | None = None, after: float = 0.0, grab: float = 0.001
):
    """What ``capture_dxcam`` returns for the 30 fps clip on a 60 Hz output
    (two presents per boundary, frames 100..129): a poll every millisecond
    gets the newest present since the last one. ``late``: no poll in that
    span; ``after``: polls run on that long after the last present;
    ``grab``: what a poll that got a picture costs."""
    presents = [(n / 60 + 0.0005, 100 + n // 2) for n in range(60)]
    end = presents[-1][0] + 0.002 + after
    times, sampled, polls = [], [], []
    seen = -1
    t = 0.0
    while t < end:
        polls.append(t)
        newest = max((k for k, (at, _) in enumerate(presents) if at <= t), default=-1)
        if newest > seen:
            seen = newest
            times.append(t)
            sampled.append(wf.sample_columns(frame(presents[newest][1], 30), (0, 1)))
            t += grab - 0.001
        t += 0.001
        if late and late[0] <= t < late[1]:
            t = late[1]
    return times, sampled, polls
