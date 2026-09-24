#!/usr/bin/env python3
"""Post-deploy A/V sync + audio-dropout gate (#147).

Compares an OBS PROGRAM recording of a playing SongPlayer output against the
ORIGINAL cached sidecars (``<base>_audio.flac`` + ``<base>_video.mp4``) and
answers two questions about the real output the operator sees:

1. **Lipsync.** ``A/V = audio_offset - video_offset`` in ms, where each offset
   is ``orig_time - rec_time`` (positive A/V = audio AHEAD of the picture).
2. **Audio continuity.** Does the recording go silent anywhere the original
   is loud (a dropout)?

Method (automates the manual 24.9.2026 measurement, A/V +13 ms, 0 dropouts):

* **Audio offset**: both tracks decoded to 8 kHz mono float. An FFT
  cross-correlation, normalized by the original's local energy, is searched
  over the whole song. The peak correlation must be >= ``MIN_AUDIO_CORR``.
  The time of sample 0 in each file comes from ffmpeg's own decoded-frame pts
  (``ashowinfo``), so AAC priming (a -0.021 s ``start_time``) is accounted for
  whether or not the decoder skips it.
* **Video offset**: recording frames are scaled to a 64-wide gray grid and
  cropped to the letterboxed content box. The box is computed from the source
  aspect, never hardcoded: a 1920x960 source in a 1920x1080 canvas gives rows
  2:34 of 36. Original frames are cropped to exactly that area
  (``source_crop``) and scaled to the box size. Frames are
  mean-removed and unit-normalized, and every recording frame is dot-scored
  against every original frame in a window of +-``VIDEO_WINDOW_S`` around the
  audio offset. The window is clamped to the decoded span of the video.
  The offset is the candidate shift ``d`` (1 ms grid) that maximizes the MEAN
  score of all recording frames against the original frame on screen at
  ``rec_pts + d`` (sample-and-hold). The centre of the maximal plateau is
  reported.
  ``match`` (median per-frame score at that shift) must be >=
  ``MIN_VIDEO_MATCH``. ``contrast`` (peak minus the median of the curve) must
  be >= ``MIN_VIDEO_CONTRAST``: a flat curve means the clip carries no motion
  to align on.

  Why a global alignment and not the per-frame median of the manual method:
  on static or lyric-video frames every candidate scores ~1.0, so per-frame
  argmaxes scatter across the window. Those frames also score HIGHEST, so a
  "frames above the median score" filter keeps them. Their median is pulled to
  the window centre, which is the audio offset, so A/V reads ~0 and would
  false-PASS. In a global mean, static frames add a constant to every shift
  and cannot move the peak.
* **Dropouts**: the recording and the aligned original are gain-matched
  (least squares). A 10 ms block is a DROPOUT when the recording's RMS is
  < 15 % of the gain-matched original's RMS while the original is loud. The
  manual method used 50 ms blocks, but a grid-aligned 50 ms block misses a
  lost 10-50 ms NDI audio buffer. "Loud" means above
  ``min(20th percentile, 0.5 x median)``. A loud 50 ms block whose relative
  error exceeds 0.8 is reported as a GLITCH. Glitches are informational and do
  not fail the gate.

Verdict: ``cannot_measure`` (exit 2) when any validity threshold is missed or
the analysis itself errors. Otherwise ``fail`` (exit 1) when |A/V| >
``--max-av-ms`` or any dropout block exists, and ``pass`` (exit 0) if not.
Every number is always printed as JSON on stdout.

The pure functions (``audio_offset``, ``content_box``, ``source_crop``,
``video_offset``, ``dropout_blocks``, ``verdict``, ``exit_code``) take numpy arrays and are
covered by ``scripts/tests/test_av_sync_check.py``. The ffmpeg I/O layer
(``decode_audio``, ``decode_video``, ``probe_video_size``, ``measure``) is kept
thin and is deliberately NOT unit-tested. The Eval Checks CI job has no
ffmpeg, so it is exercised on the box by the post-deploy gate
(``e2e/post-deploy-av-sync.spec.ts``).

Only numpy is required at runtime (the box's ``lyrics_venv`` has it) plus an
ffmpeg binary. No ffprobe is needed; the box ships none.

Blind spot: the reference is the sidecar pair itself. An offset baked into
the sidecars at download/normalize time is invisible here.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import traceback

import numpy as np

SR = 8000
GRID_W = 64
MIN_AUDIO_CORR = 0.9
MIN_VIDEO_MATCH = 0.95
MIN_VIDEO_CONTRAST = 0.002
VIDEO_WINDOW_S = 1.0
VIDEO_STEP_S = 0.001
BLOCK_MS = 50
DROPOUT_BLOCK_MS = 10
EDGE_GUARD_MS = 100
DROPOUT_RATIO = 0.15
LOUD_PERCENTILE = 20
LOUD_MEDIAN_FRACTION = 0.5
GLITCH_REL_ERR = 0.8
DEFAULT_MAX_AV_MS = 40.0
MAX_REPORTED_TIMES = 20

EXIT_CODES = {"pass": 0, "fail": 1, "cannot_measure": 2}


# ---------------------------------------------------------------------------
# Pure analysis
# ---------------------------------------------------------------------------


def audio_offset(
    rec: np.ndarray,
    orig: np.ndarray,
    sr: int = SR,
    rec_t0: float = 0.0,
    orig_t0: float = 0.0,
) -> dict:
    """Locate ``rec`` inside ``orig`` by energy-normalized cross-correlation.

    ``rec_t0`` / ``orig_t0`` are the times of sample 0 in each file's own
    timeline. Returns ``offset_s = orig_time - rec_time`` for the same audio,
    the peak normalized correlation ``corr`` (in [-1, 1]), and the integer
    sample ``lag`` into ``orig`` where ``rec`` starts.
    """
    rec = np.asarray(rec, dtype=np.float64)
    orig = np.asarray(orig, dtype=np.float64)
    n, m = len(rec), len(orig)
    if n == 0 or m < n:
        raise ValueError(
            f"audio: recording ({n} samples) must be non-empty and no longer than the original ({m})"
        )
    rec_norm = float(np.sqrt(np.dot(rec, rec)))
    if rec_norm == 0.0:
        raise ValueError("audio: recording is digital silence")

    size = 1 << int(np.ceil(np.log2(n + m - 1)))
    xc = np.fft.irfft(np.fft.rfft(orig, size) * np.conj(np.fft.rfft(rec, size)), size)[
        : m - n + 1
    ]
    csum = np.concatenate(([0.0], np.cumsum(orig * orig)))
    energy = csum[n:] - csum[: m - n + 1]
    energy = np.maximum(energy, 0.0)
    # A small floor keeps near-silent stretches of the original from dividing
    # FFT round-off by ~0; it biases a real match by < 0.1 %.
    eps = 1e-3 * float(np.mean(energy)) + 1e-12
    corr = np.clip(xc / (rec_norm * np.sqrt(energy + eps)), -1.0, 1.0)

    lag = int(np.argmax(corr))
    frac = 0.0
    if 0 < lag < len(corr) - 1:
        y0, y1, y2 = corr[lag - 1], corr[lag], corr[lag + 1]
        denom = y0 - 2.0 * y1 + y2
        if denom < 0.0:
            frac = float(np.clip(0.5 * (y0 - y2) / denom, -0.5, 0.5))
    offset_s = (orig_t0 + (lag + frac) / sr) - rec_t0
    # Diagnostic only: the best correlation more than 0.5 s away from the peak.
    # A repeated chorus can come close. A wrong peak then shows up as a low
    # video match, so it reads cannot_measure and never a false pass.
    away = int(0.5 * sr)
    others = np.concatenate((corr[: max(0, lag - away)], corr[lag + away + 1 :]))
    second = float(others.max()) if len(others) else None
    return {
        "offset_s": float(offset_s),
        "corr": float(corr[lag]),
        "lag": lag,
        "second_corr": second,
    }


def _content_geometry(
    canvas_w: int,
    canvas_h: int,
    src_w: int,
    src_h: int,
    grid_w: int,
    grid_h: int | None,
) -> tuple[int, float, float, float, float]:
    """``(grid_h, x0f, y0f, content_w, content_h)`` of the fitted source, in grid cells."""
    if min(canvas_w, canvas_h, src_w, src_h) <= 0:
        raise ValueError("content_box: sizes must be positive")
    if grid_h is None:
        grid_h = max(1, round(grid_w * canvas_h / canvas_w))
    canvas_aspect = canvas_w / canvas_h
    src_aspect = src_w / src_h
    if src_aspect >= canvas_aspect:
        content_w, content_h = float(grid_w), grid_h * canvas_aspect / src_aspect
    else:
        content_w, content_h = grid_w * src_aspect / canvas_aspect, float(grid_h)
    return (
        grid_h,
        (grid_w - content_w) / 2.0,
        (grid_h - content_h) / 2.0,
        content_w,
        content_h,
    )


def content_box(
    canvas_w: int,
    canvas_h: int,
    src_w: int,
    src_h: int,
    grid_w: int = GRID_W,
    grid_h: int | None = None,
) -> tuple[int, int, int, int]:
    """Rows/cols ``(y0, y1, x0, x1)`` of the canvas grid covered by the source.

    The source is fit into the canvas preserving aspect (letterbox when it is
    wider, pillarbox when narrower) and centred. Only grid cells FULLY inside
    the content are kept, so no cell mixes black bar with picture.
    """
    grid_h, x0f, y0f, content_w, content_h = _content_geometry(
        canvas_w, canvas_h, src_w, src_h, grid_w, grid_h
    )
    y0, y1 = int(np.ceil(y0f - 1e-6)), int(np.floor(y0f + content_h + 1e-6))
    x0, x1 = int(np.ceil(x0f - 1e-6)), int(np.floor(x0f + content_w + 1e-6))
    if y1 <= y0 or x1 <= x0:
        raise ValueError(
            f"content_box: source {src_w}x{src_h} leaves no content in a {grid_w}x{grid_h} grid"
        )
    return y0, y1, x0, x1


def source_crop(
    canvas_w: int,
    canvas_h: int,
    src_w: int,
    src_h: int,
    grid_w: int = GRID_W,
    grid_h: int | None = None,
) -> tuple[float, float, float, float]:
    """``(x, y, w, h)`` in SOURCE pixels of exactly the area ``content_box`` keeps.

    When a letterbox edge falls inside a grid cell, ``content_box`` drops that
    partial cell. The original must be cropped by the same fraction before it
    is scaled into the box, or the two pictures differ by up to a cell of
    geometry.
    """
    y0, y1, x0, x1 = content_box(canvas_w, canvas_h, src_w, src_h, grid_w, grid_h)
    _, x0f, y0f, content_w, content_h = _content_geometry(
        canvas_w, canvas_h, src_w, src_h, grid_w, grid_h
    )
    sx, sy = src_w / content_w, src_h / content_h
    return (x0 - x0f) * sx, (y0 - y0f) * sy, (x1 - x0) * sx, (y1 - y0) * sy


def _normalize_frames(frames: np.ndarray) -> np.ndarray:
    flat = np.asarray(frames, dtype=np.float64).reshape(len(frames), -1)
    flat = flat - flat.mean(axis=1, keepdims=True)
    norms = np.linalg.norm(flat, axis=1, keepdims=True)
    # A uniform frame (black) has no structure: leave it a zero vector, it then
    # scores 0 against everything and cannot bias the alignment.
    return np.divide(flat, norms, out=np.zeros_like(flat), where=norms > 1e-9)


def video_offset(
    rec_frames: np.ndarray,
    rec_pts: np.ndarray,
    orig_frames: np.ndarray,
    orig_pts: np.ndarray,
    center_s: float,
    window_s: float = VIDEO_WINDOW_S,
    step_s: float = VIDEO_STEP_S,
) -> dict:
    """Best shift ``d`` (``orig_time = rec_time + d``) aligning the frame sequences.

    ``rec_frames`` must already be cropped to the content box and match the
    ``orig_frames`` shape. ``orig_pts`` must be increasing. An original frame is
    on screen from its pts until the next one (sample-and-hold). The candidate
    window ``center_s +- window_s`` is clamped so every recording frame maps
    inside the decoded original span.
    """
    rec_pts = np.asarray(rec_pts, dtype=np.float64)
    orig_pts = np.asarray(orig_pts, dtype=np.float64)
    if len(rec_frames) != len(rec_pts) or len(orig_frames) != len(orig_pts):
        raise ValueError("video: frames and pts lengths differ")
    if len(rec_frames) == 0 or len(orig_frames) < 2:
        raise ValueError("video: need recording frames and >= 2 original frames")
    if rec_frames.shape[1:] != orig_frames.shape[1:]:
        raise ValueError(
            f"video: frame shapes differ {rec_frames.shape[1:]} vs {orig_frames.shape[1:]}"
        )
    if np.any(np.diff(orig_pts) <= 0):
        raise ValueError("video: original pts must be strictly increasing")

    last_dur = float(orig_pts[-1] - orig_pts[-2])
    lo = max(center_s - window_s, orig_pts[0] - rec_pts.min())
    hi = min(center_s + window_s, orig_pts[-1] + last_dur - rec_pts.max() - 1e-9)
    if hi < lo:
        raise ValueError(
            f"video: no shift in [{center_s - window_s:.3f}, {center_s + window_s:.3f}] keeps the "
            f"recording inside the decoded original [{orig_pts[0]:.3f}, {orig_pts[-1] + last_dur:.3f}]"
        )
    shifts = np.arange(lo, hi + step_s / 2.0, step_s)

    scores = _normalize_frames(rec_frames) @ _normalize_frames(orig_frames).T  # (N, K)
    idx = (
        np.searchsorted(orig_pts, rec_pts[:, None] + shifts[None, :], side="right") - 1
    )
    idx = np.clip(idx, 0, len(orig_pts) - 1)
    per_frame = scores[np.arange(len(rec_pts))[:, None], idx]  # (N, D)
    curve = per_frame.mean(axis=0)

    best = int(np.argmax(curve))
    peak = curve[best]
    tol = 1e-9 + 1e-9 * abs(peak)
    left = best
    while left > 0 and curve[left - 1] >= peak - tol:
        left -= 1
    right = best
    while right < len(curve) - 1 and curve[right + 1] >= peak - tol:
        right += 1
    offset = float((shifts[left] + shifts[right]) / 2.0)
    centre_idx = (left + right) // 2
    return {
        "offset_s": offset,
        "match": float(np.median(per_frame[:, centre_idx])),
        "contrast": float(peak - np.median(curve)),
        "plateau_ms": float((shifts[right] - shifts[left]) * 1000.0),
        "frames": int(len(rec_pts)),
        "shift_range_s": [float(shifts[0]), float(shifts[-1])],
    }


def _runs(mask: np.ndarray) -> list[tuple[int, int]]:
    """``(start, length)`` of every run of consecutive True values."""
    padded = np.concatenate(([False], mask, [False])).astype(np.int8)
    edges = np.flatnonzero(np.diff(padded))
    return [(int(a), int(b - a)) for a, b in zip(edges[::2], edges[1::2])]


def _block_rms(x: np.ndarray, blk: int) -> np.ndarray:
    nb = len(x) // blk
    return np.sqrt(np.mean(x[: nb * blk].reshape(nb, blk) ** 2, axis=1))


def _loud_inner(orig_rms: np.ndarray, blk_ms: float, edge_guard_ms: int) -> np.ndarray:
    """Blocks outside the edge guard where the ORIGINAL is loud.

    Loud means an RMS above ``min(LOUD_PERCENTILE-th percentile, LOUD_MEDIAN_FRACTION
    x median)`` of the guard-free blocks. The percentile alone drops a fixed
    20 % of blocks even in a dense mix, where the quietest 10 ms are barely
    quieter than the rest. A short gap landing there would be missed one time
    in five. The median bound keeps every block at least half the typical
    level. Genuinely quiet passages (rests, fades), where an encoder may round
    the signal to near-zero, stay excluded.
    """
    nb = len(orig_rms)
    guard = int(np.ceil(edge_guard_ms / blk_ms))
    inner = np.zeros(nb, dtype=bool)
    inner[guard : nb - guard] = True
    if not inner.any():
        return inner
    ref = orig_rms[inner]
    threshold = min(
        float(np.percentile(ref, LOUD_PERCENTILE)),
        LOUD_MEDIAN_FRACTION * float(np.median(ref)),
    )
    return inner & (orig_rms > threshold)


def dropout_blocks(
    rec: np.ndarray,
    orig_aligned: np.ndarray,
    sr: int = SR,
    block_ms: int = BLOCK_MS,
    t0: float = 0.0,
    edge_guard_ms: int = EDGE_GUARD_MS,
    dropout_block_ms: int = DROPOUT_BLOCK_MS,
) -> dict:
    """Gain-matched block comparison of the recording vs the aligned original.

    ``orig_aligned[i]`` must be the original sample heard at ``rec[i]``, and
    ``t0`` is the recording time of sample 0 (used for the reported times).

    * DROPOUTS are detected at ``dropout_block_ms`` (10 ms) resolution. A
      sub-block is a dropout when the recording's RMS is < ``DROPOUT_RATIO`` of
      the gain-matched original's while the original is loud. A lost NDI audio
      buffer is 10-50 ms, and a grid-aligned 50 ms block only notices a gap
      that covers nearly all of it.
    * GLITCHES (relative error > ``GLITCH_REL_ERR`` on a loud ``block_ms``
      block that is not a dropout) are informational.

    Blocks touching the first/last ``edge_guard_ms`` are not classified. ffmpeg
    6.1 decodes an AAC mkv's encoder priming (``start_time`` -0.021 s) as
    near-silence at sample 0, which would read as a dropout on every run.
    """
    rec = np.asarray(rec, dtype=np.float64)
    orig = np.asarray(orig_aligned, dtype=np.float64)
    if len(rec) != len(orig):
        raise ValueError(f"dropouts: length mismatch {len(rec)} vs {len(orig)}")
    blk = int(sr * block_ms / 1000)
    sub = int(sr * dropout_block_ms / 1000)
    nb = len(rec) // blk
    if nb == 0:
        raise ValueError("dropouts: recording shorter than one block")
    orig_energy = float(np.dot(orig, orig))
    if orig_energy == 0.0:
        raise ValueError("dropouts: aligned original is digital silence")
    gain = float(np.dot(rec, orig) / orig_energy)

    sub_orig_rms = _block_rms(orig, sub)
    sub_loud = _loud_inner(sub_orig_rms, dropout_block_ms, edge_guard_ms)
    dropout = sub_loud & (
        _block_rms(rec, sub) < DROPOUT_RATIO * abs(gain) * sub_orig_rms
    )
    events = [
        {"start_s": round(t0 + a * sub / sr, 3), "ms": n * dropout_block_ms}
        for a, n in _runs(dropout)
    ]

    r = rec[: nb * blk].reshape(nb, blk)
    ref = gain * orig[: nb * blk].reshape(nb, blk)
    loud = _loud_inner(_block_rms(orig, blk), block_ms, edge_guard_ms)
    rel_err = np.linalg.norm(r - ref, axis=1) / np.maximum(
        np.linalg.norm(ref, axis=1), 1e-12
    )
    # A 50 ms block containing a dropout is reported as the dropout, not a glitch.
    dropout_in_block = np.zeros(nb, dtype=bool)
    per_block = blk // sub
    hits = np.flatnonzero(dropout) // per_block
    dropout_in_block[hits[hits < nb]] = True
    glitch = loud & ~dropout_in_block & (rel_err > GLITCH_REL_ERR)

    return {
        "blocks": int(nb),
        "block_ms": block_ms,
        "dropout_block_ms": dropout_block_ms,
        "edge_guard_ms": edge_guard_ms,
        "loud_blocks": int(loud.sum()),
        "dropout_blocks": int(dropout.sum()),
        "dropout_events": events[:MAX_REPORTED_TIMES],
        "glitch_blocks": int(glitch.sum()),
        "glitch_times_s": [
            round(t0 + i * blk / sr, 3)
            for i in np.flatnonzero(glitch)[:MAX_REPORTED_TIMES]
        ],
        "median_rel_err": float(np.median(rel_err[loud])) if loud.any() else None,
        "gain": gain,
    }


def verdict(
    audio_corr: float,
    video_match: float,
    video_contrast: float,
    av_ms: float,
    dropouts: int,
    max_av_ms: float = DEFAULT_MAX_AV_MS,
) -> tuple[str, list[str]]:
    """``("pass" | "fail" | "cannot_measure", reasons)`` from the measured numbers."""
    unmeasurable = []
    if not audio_corr >= MIN_AUDIO_CORR:
        unmeasurable.append(f"audio correlation {audio_corr:.3f} < {MIN_AUDIO_CORR}")
    if not video_match >= MIN_VIDEO_MATCH:
        unmeasurable.append(f"video match {video_match:.3f} < {MIN_VIDEO_MATCH}")
    if not video_contrast >= MIN_VIDEO_CONTRAST:
        unmeasurable.append(
            f"video contrast {video_contrast:.4f} < {MIN_VIDEO_CONTRAST} (no motion to align on)"
        )
    failures = []
    if not abs(av_ms) <= max_av_ms:
        failures.append(f"|A/V| {abs(av_ms):.1f} ms > {max_av_ms:g} ms")
    if dropouts > 0:
        failures.append(f"{dropouts} audio dropout block(s)")
    if unmeasurable:
        return "cannot_measure", unmeasurable + failures
    if failures:
        return "fail", failures
    return "pass", []


def exit_code(status: str) -> int:
    return EXIT_CODES[status]


# ---------------------------------------------------------------------------
# ffmpeg I/O. Thin, untested by design (no ffmpeg in Eval Checks CI).
# Exercised on the box by e2e/post-deploy-av-sync.spec.ts.
# ---------------------------------------------------------------------------

_PTS_RE = re.compile(r"pts_time:\s*(-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)")
_SIZE_RE = re.compile(r"\bs:(\d+)x(\d+)")


def _run(cmd: list[str]) -> tuple[bytes, str]:
    proc = subprocess.run(cmd, capture_output=True, check=False)
    stderr = proc.stderr.decode("utf-8", errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(stderr.strip().splitlines()[-15:])
        raise RuntimeError(f"ffmpeg exited {proc.returncode}: {' '.join(cmd)}\n{tail}")
    return proc.stdout, stderr


def _info_lines(stderr: str, filter_name: str) -> list[str]:
    tag = f"Parsed_{filter_name}_"
    return [ln for ln in stderr.splitlines() if tag in ln and "pts_time:" in ln]


def _pts(line: str, path: str) -> float:
    m = _PTS_RE.search(line)
    if m is None:  # e.g. "pts_time:NOPTS" — a frame without a timestamp
        raise RuntimeError(f"frame without a usable pts_time in {path}: {line.strip()}")
    return float(m.group(1))


def decode_audio(ffmpeg: str, path: str) -> tuple[np.ndarray, float]:
    """First audio stream as 8 kHz mono float32 + the time of its sample 0."""
    out, err = _run(
        [ffmpeg, "-hide_banner", "-nostdin", "-copyts", "-i", path, "-map", "0:a:0",
         "-af", "ashowinfo", "-ac", "1", "-ar", str(SR), "-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"]
    )  # fmt: skip
    lines = _info_lines(err, "ashowinfo")
    if not lines:
        raise RuntimeError(f"no ashowinfo frame lines decoding audio of {path}")
    t0 = _pts(lines[0], path)
    return np.frombuffer(out, dtype="<f4").astype(np.float64), t0


def probe_video_size(ffmpeg: str, path: str) -> tuple[int, int]:
    """Width/height of the first video stream (ffmpeg ``showinfo``, no ffprobe)."""
    _, err = _run(
        [ffmpeg, "-hide_banner", "-nostdin", "-i", path, "-map", "0:v:0", "-frames:v", "1",
         "-vf", "showinfo", "-f", "null", "-"]
    )  # fmt: skip
    for ln in _info_lines(err, "showinfo"):
        m = _SIZE_RE.search(ln)
        if m:
            return int(m.group(1)), int(m.group(2))
    raise RuntimeError(f"could not read the video size of {path}")


def decode_video(
    ffmpeg: str,
    path: str,
    width: int,
    height: int,
    start: float | None = None,
    duration: float | None = None,
    crop: tuple[float, float, float, float] | None = None,
) -> tuple[np.ndarray, np.ndarray]:
    """Frames (optionally cropped to source-pixel ``(x, y, w, h)`` first) scaled
    to ``width`` x ``height`` gray (uint8), plus their container pts."""
    # -ss/-t are INPUT options: ffmpeg trims before the filter graph, so showinfo
    # reports exactly the frames written (an output -t under -copyts counts from
    # 0, not from the seek point, and cuts frames showinfo already logged).
    cmd = [ffmpeg, "-hide_banner", "-nostdin", "-copyts"]
    if start is not None:
        cmd += ["-ss", f"{start:.3f}"]
    if duration is not None:
        cmd += ["-t", f"{duration:.3f}"]
    crop_f = (
        "" if crop is None else "crop={2:.3f}:{3:.3f}:{0:.3f}:{1:.3f},".format(*crop)
    )
    cmd += ["-i", path, "-map", "0:v:0",
            "-vf", f"showinfo,{crop_f}scale={width}:{height}:flags=area,format=gray",
            "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"]  # fmt: skip
    out, err = _run(cmd)
    pts = np.array([_pts(ln, path) for ln in _info_lines(err, "showinfo")])
    frame_bytes = width * height
    n = len(out) // frame_bytes
    if n == 0 or n != len(pts):
        raise RuntimeError(
            f"decoded {n} frames but showinfo reported {len(pts)} pts for {path}"
        )
    frames = np.frombuffer(out[: n * frame_bytes], dtype=np.uint8).reshape(
        n, height, width
    )
    return frames, pts


def measure(
    recording: str,
    orig_audio: str,
    orig_video: str,
    ffmpeg: str = "ffmpeg",
    max_av_ms: float = DEFAULT_MAX_AV_MS,
) -> dict:
    rec_a, rec_a_t0 = decode_audio(ffmpeg, recording)
    orig_a, orig_a_t0 = decode_audio(ffmpeg, orig_audio)
    aud = audio_offset(rec_a, orig_a, SR, rec_a_t0, orig_a_t0)
    aligned = orig_a[aud["lag"] : aud["lag"] + len(rec_a)]
    drops = dropout_blocks(rec_a, aligned, SR, BLOCK_MS, rec_a_t0)

    canvas_w, canvas_h = probe_video_size(ffmpeg, recording)
    grid_h = max(1, round(GRID_W * canvas_h / canvas_w))
    src_w, src_h = probe_video_size(ffmpeg, orig_video)
    y0, y1, x0, x1 = content_box(canvas_w, canvas_h, src_w, src_h, GRID_W, grid_h)
    crop = source_crop(canvas_w, canvas_h, src_w, src_h, GRID_W, grid_h)
    rec_f, rec_pts = decode_video(ffmpeg, recording, GRID_W, grid_h)
    rec_f = rec_f[:, y0:y1, x0:x1]
    span = float(rec_pts.max() - rec_pts.min())
    start = max(0.0, aud["offset_s"] + float(rec_pts.min()) - VIDEO_WINDOW_S - 0.5)
    orig_f, orig_pts = decode_video(
        ffmpeg,
        orig_video,
        x1 - x0,
        y1 - y0,
        start=start,
        duration=span + 2 * VIDEO_WINDOW_S + 1.0,
        crop=crop,
    )
    vid = video_offset(rec_f, rec_pts, orig_f, orig_pts, center_s=aud["offset_s"])

    av_ms = (aud["offset_s"] - vid["offset_s"]) * 1000.0
    status, reasons = verdict(
        aud["corr"],
        vid["match"],
        vid["contrast"],
        av_ms,
        drops["dropout_blocks"],
        max_av_ms,
    )
    return {
        "status": status,
        "reasons": reasons,
        "av_ms": round(av_ms, 1),
        "max_av_ms": max_av_ms,
        "recording_s": round(len(rec_a) / SR, 2),
        "audio": {
            "offset_s": round(aud["offset_s"], 4),
            "corr": round(aud["corr"], 4),
            "min_corr": MIN_AUDIO_CORR,
            "second_corr": None
            if aud["second_corr"] is None
            else round(aud["second_corr"], 4),
            "rec_t0_s": rec_a_t0,
            "orig_t0_s": orig_a_t0,
        },
        "video": {
            "offset_s": round(vid["offset_s"], 4),
            "match": round(vid["match"], 4),
            "min_match": MIN_VIDEO_MATCH,
            "contrast": round(vid["contrast"], 4),
            "min_contrast": MIN_VIDEO_CONTRAST,
            "plateau_ms": round(vid["plateau_ms"], 1),
            "frames": vid["frames"],
            "canvas": [canvas_w, canvas_h],
            "source": [src_w, src_h],
            "content_box_y0_y1_x0_x1": [y0, y1, x0, x1],
            "source_crop_x_y_w_h": [round(v, 1) for v in crop],
        },
        "dropouts": drops,
        "inputs": {
            "recording": recording,
            "orig_audio": orig_audio,
            "orig_video": orig_video,
        },
    }


def summary_line(result: dict) -> str:
    audio = result.get("audio", {})
    video = result.get("video", {})
    drops = result.get("dropouts", {})
    return (
        f"AV-SYNC status={result['status']} av_ms={result.get('av_ms')} "
        f"audio_corr={audio.get('corr')} video_match={video.get('match')} "
        f"video_contrast={video.get('contrast')} dropouts={drops.get('dropout_blocks')} "
        f"glitches={drops.get('glitch_blocks')} reasons={result['reasons']}"
    )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument(
        "--recording", required=True, help="OBS program recording (mkv/mp4)"
    )
    ap.add_argument("--orig-audio", required=True, help="original <base>_audio.flac")
    ap.add_argument("--orig-video", required=True, help="original <base>_video.mp4")
    ap.add_argument("--max-av-ms", type=float, default=DEFAULT_MAX_AV_MS)
    ap.add_argument(
        "--ffmpeg", default="ffmpeg", help="ffmpeg binary (default: on PATH)"
    )
    args = ap.parse_args(argv)
    try:
        result = measure(
            args.recording,
            args.orig_audio,
            args.orig_video,
            args.ffmpeg,
            args.max_av_ms,
        )
    except Exception as exc:  # noqa: BLE001 - reported loudly as cannot_measure, never swallowed
        traceback.print_exc(file=sys.stderr)
        result = {
            "status": "cannot_measure",
            "reasons": [f"analysis error: {type(exc).__name__}: {exc}"],
            "inputs": {
                "recording": args.recording,
                "orig_audio": args.orig_audio,
                "orig_video": args.orig_video,
            },
        }
    print(json.dumps(result, indent=2))
    print(summary_line(result), file=sys.stderr)
    return exit_code(result["status"])


if __name__ == "__main__":
    sys.exit(main())
