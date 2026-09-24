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
* **Dropouts**: a 10 ms RMS window slides at a 1 ms hop over the recording
  and the aligned original. A window is a DROPOUT when the recording's RMS is
  < 15 % of the level-matched original's while the original is loud. Any gap
  of >= 11 ms is caught, whatever its phase. The manual method used 50 ms
  blocks, but a grid-aligned block misses a lost 10-50 ms NDI audio buffer.
  "Loud" means above ``max(0.1 x median window RMS, -45 dBFS)`` in every 2 ms.
  A loud 50 ms block whose relative error exceeds 0.8 is reported as a
  GLITCH. Glitches are informational and do not fail the gate.

Verdict, in order (see ``verdict``):
1. ``cannot_measure`` (exit 2) when the AUDIO is unmeasurable or the analysis
   itself errors.
2. ``fail`` (exit 1) on any dropout, even when the picture is unmeasurable.
3. ``cannot_measure`` when the PICTURE is unmeasurable.
4. ``fail`` when |A/V| > ``--max-av-ms``, otherwise ``pass`` (exit 0).

``unmeasurable_sides`` tells a caller whether a retake on another song makes
sense (only for ``["video"]``). Every number is always printed as JSON on
stdout.

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
DROPOUT_WINDOW_MS = 10
DROPOUT_HOP_MS = 1
EDGE_GUARD_MS = 100
DROPOUT_RATIO = 0.15
LOUD_REL_MEDIAN = 0.1  # -20 dB below the take's median window RMS
LOUD_ABS_FLOOR = 10 ** (-45 / 20)  # -45 dBFS; the sidecars are -14 LUFS
LOUD_SUB_MS = 2  # the original must be loud in every 2 ms of a window
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


def _windowed_rms(x: np.ndarray, starts: np.ndarray, win: int) -> np.ndarray:
    """RMS of ``x[s : s + win]`` for every start ``s`` (cumulative sums, O(n))."""
    csum = np.concatenate(([0.0], np.cumsum(x * x)))
    return np.sqrt(np.maximum(csum[starts + win] - csum[starts], 0.0) / win)


def _loud(
    orig_rms: np.ndarray, inner: np.ndarray, floor_rms: np.ndarray | None = None
) -> np.ndarray:
    """Windows where the ORIGINAL is loud, among the ``inner`` (unguarded) ones.

    Threshold = ``max(0.1 x median window RMS, -45 dBFS)`` over the inner
    windows: everything within 20 dB of the take's typical level and above
    near-silence.
    * A percentile gate would drop a fixed share of windows. In a dense mix
      that share is barely quieter than the rest, and in dynamic material it
      is clearly audible, and a lost buffer there must still fail.
    * The absolute floor (the sidecars are -14 LUFS) keeps rests, fades, and
      anything an encoder may round to zero out.
    ``floor_rms`` (optional, per window) is compared instead of ``orig_rms``.
    Pass the minimum short sub-window RMS, so a window is loud only when the
    original is loud THROUGHOUT it. A window that merely clips the edge of a
    hard onset is never judged against a recording that is one sample late.
    """
    if not inner.any():
        return inner
    threshold = max(LOUD_REL_MEDIAN * float(np.median(orig_rms[inner])), LOUD_ABS_FLOOR)
    level = orig_rms if floor_rms is None else floor_rms
    return inner & (level > threshold)


def dropout_blocks(
    rec: np.ndarray,
    orig_aligned: np.ndarray,
    sr: int = SR,
    block_ms: int = BLOCK_MS,
    t0: float = 0.0,
    edge_guard_ms: int = EDGE_GUARD_MS,
    window_ms: int = DROPOUT_WINDOW_MS,
    hop_ms: int = DROPOUT_HOP_MS,
) -> dict:
    """Compare the recording with the aligned original for dropouts and glitches.

    ``orig_aligned[i]`` must be the original sample heard at ``rec[i]``, and
    ``t0`` is the recording time of sample 0 (used for the reported times).

    * DROPOUTS: a ``window_ms`` (10 ms) RMS window slides at ``hop_ms`` (1 ms).
      A window is a dropout when the recording's RMS is < ``DROPOUT_RATIO`` of
      the level-matched original's while the original is loud. Overlapping
      dropout windows merge into events. Any gap of >= window + hop (11 ms)
      contains a whole window whatever its phase, so a lost 10-50 ms NDI audio
      buffer is caught. A fixed block grid only notices a gap that covers
      nearly all of one block.
    * LEVEL for the dropout test = ``max(|LS gain|, median rec/orig RMS ratio
      over loud windows)``. The RMS ratio is immune to a fractional-sample lag
      that shrinks the phase-coherent LS gain and, with it, the threshold.
    * GLITCHES: relative error > ``GLITCH_REL_ERR`` on a loud ``block_ms``
      block that holds no dropout. Informational only.

    Nothing touching the first/last ``edge_guard_ms`` is classified. ffmpeg 6.1
    decodes an AAC mkv's encoder priming (``start_time`` -0.021 s) as
    near-silence at sample 0, which would read as a dropout on every run.
    """
    rec = np.asarray(rec, dtype=np.float64)
    orig = np.asarray(orig_aligned, dtype=np.float64)
    n = len(rec)
    if n != len(orig):
        raise ValueError(f"dropouts: length mismatch {n} vs {len(orig)}")
    blk = int(sr * block_ms / 1000)
    win = int(sr * window_ms / 1000)
    hop = max(1, int(sr * hop_ms / 1000))
    guard = int(sr * edge_guard_ms / 1000)
    nb = n // blk
    if nb == 0:
        raise ValueError("dropouts: recording shorter than one block")
    orig_energy = float(np.dot(orig, orig))
    if orig_energy == 0.0:
        raise ValueError("dropouts: aligned original is digital silence")
    gain = float(np.dot(rec, orig) / orig_energy)

    starts = np.arange(0, n - win + 1, hop)
    w_orig = _windowed_rms(orig, starts, win)
    w_rec = _windowed_rms(rec, starts, win)
    sub = max(1, int(sr * LOUD_SUB_MS / 1000))
    sub_rms = _windowed_rms(orig, np.arange(0, n - sub + 1, hop), sub)
    per_win = (win - sub) // hop + 1  # sub-windows starting inside each window
    min_sub = np.lib.stride_tricks.sliding_window_view(sub_rms, per_win).min(axis=1)
    if win % hop or sub % hop or len(min_sub) < len(starts):
        raise ValueError(
            "dropouts: the hop must divide the window and sub-window lengths"
        )
    w_loud = _loud(
        w_orig, (starts >= guard) & (starts + win <= n - guard), min_sub[: len(starts)]
    )
    ratio = w_rec[w_loud] / w_orig[w_loud]
    level = max(abs(gain), float(np.median(ratio)) if len(ratio) else 0.0)
    w_drop = w_loud & (w_rec < DROPOUT_RATIO * level * w_orig)

    # Sample spans of the dropout runs. Runs closer than one block merge into
    # one event: a long gap crossing a quiet (not loud) stretch of the
    # original is one loss, not several.
    spans: list[list[int]] = []
    for a, k in _runs(w_drop):
        s0, s1 = int(starts[a]), int(starts[a + k - 1]) + win
        if spans and s0 - spans[-1][1] < blk:
            spans[-1][1] = s1
        else:
            spans.append([s0, s1])
    in_dropout = np.zeros(n, dtype=bool)
    for s0, s1 in spans:
        in_dropout[s0:s1] = True
    events = [
        {"start_s": round(t0 + s0 / sr, 3), "ms": round((s1 - s0) * 1000.0 / sr, 1)}
        for s0, s1 in spans
    ]
    total_ms = sum(e["ms"] for e in events)

    b_starts = np.arange(nb) * blk
    b_loud = _loud(
        _windowed_rms(orig, b_starts, blk),
        (b_starts >= guard) & (b_starts + blk <= n - guard),
    )
    r = rec[: nb * blk].reshape(nb, blk)
    ref = gain * orig[: nb * blk].reshape(nb, blk)
    rel_err = np.linalg.norm(r - ref, axis=1) / np.maximum(
        np.linalg.norm(ref, axis=1), 1e-12
    )
    b_has_dropout = in_dropout[: nb * blk].reshape(nb, blk).any(axis=1)
    glitch = b_loud & ~b_has_dropout & (rel_err > GLITCH_REL_ERR)

    return {
        "dropout_count": len(events),
        "dropout_ms": round(total_ms, 1),
        "dropout_events": events[:MAX_REPORTED_TIMES],
        "window_ms": window_ms,
        "hop_ms": hop_ms,
        "edge_guard_ms": edge_guard_ms,
        "loud_windows": int(w_loud.sum()),
        "level": level,
        "gain": gain,
        "blocks": int(nb),
        "block_ms": block_ms,
        "glitch_blocks": int(glitch.sum()),
        "glitch_times_s": [
            round(t0 + i * blk / sr, 3)
            for i in np.flatnonzero(glitch)[:MAX_REPORTED_TIMES]
        ],
        "median_rel_err": float(np.median(rel_err[b_loud])) if b_loud.any() else None,
    }


def verdict(
    audio_corr: float,
    video_match: float,
    video_contrast: float,
    av_ms: float,
    dropouts: int,
    max_av_ms: float = DEFAULT_MAX_AV_MS,
    video_error: str | None = None,
) -> tuple[str, list[str], list[str]]:
    """``(status, reasons, unmeasurable_sides)`` from the measured numbers.

    Order matters. Each result is trusted only as far as its own side was
    measurable:
    1. AUDIO unmeasurable (corr < 0.9) -> ``cannot_measure``. The alignment,
       the dropouts and A/V all depend on it.
    2. Dropouts (audio-only evidence) -> ``fail``, even when the PICTURE is
       unmeasurable. A still or overlaid video must never hide a lost buffer.
    3. PICTURE unmeasurable (match < 0.95 or contrast < 0.002) ->
       ``cannot_measure``, and A/V is not judged.
    4. |A/V| > ``max_av_ms`` -> ``fail``, else ``pass``.
    ``unmeasurable_sides`` (``"audio"`` / ``"video"`` / ``"video_error"``)
    lets the caller retake ONLY a ``["video"]`` cannot-measure (a still or
    overlaid picture that another song may not have). A ``video_error`` (the
    picture step raised: a probe, decode or crop bug) sits at step 3 too, but
    is deterministic and never retaken. Its reason is the error alone, not
    NaN thresholds.
    """
    audio_bad = (
        []
        if audio_corr >= MIN_AUDIO_CORR
        else [f"audio correlation {audio_corr:.3f} < {MIN_AUDIO_CORR}"]
    )
    video_bad = []
    video_side = "video"
    if video_error:
        video_bad.append(video_error)
        video_side = "video_error"
    elif not video_match >= MIN_VIDEO_MATCH:
        video_bad.append(f"video match {video_match:.3f} < {MIN_VIDEO_MATCH}")
    if not video_error and not video_contrast >= MIN_VIDEO_CONTRAST:
        video_bad.append(
            f"video contrast {video_contrast:.4f} < {MIN_VIDEO_CONTRAST} (no motion to align on)"
        )
    drop_fail = [f"{dropouts} audio dropout(s)"] if dropouts > 0 else []
    if audio_bad:
        sides = ["audio"] + ([video_side] if video_bad else [])
        # Dropouts are reported too (heavy ones lower the correlation), but an
        # unaligned recording's dropout count is not a verdict on its own.
        return "cannot_measure", audio_bad + video_bad + drop_fail, sides
    if drop_fail:
        return "fail", drop_fail + video_bad, []
    if video_bad:
        return "cannot_measure", video_bad, [video_side]
    if not abs(av_ms) <= max_av_ms:
        return "fail", [f"|A/V| {abs(av_ms):.1f} ms > {max_av_ms:g} ms"], []
    return "pass", [], []


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
    crop_f = ""
    if crop is not None:
        # Whole pixels, exact=1: no silent rounding to the 4:2:0 chroma grid.
        x, y, w, h = (round(v) for v in crop)
        crop_f = f"crop={w}:{h}:{x}:{y}:exact=1,"
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


def _measure_video(
    ffmpeg: str, recording: str, orig_video: str, center_s: float
) -> dict:
    """Decode both pictures around ``center_s`` (the audio offset) and align them."""
    canvas_w, canvas_h = probe_video_size(ffmpeg, recording)
    grid_h = max(1, round(GRID_W * canvas_h / canvas_w))
    src_w, src_h = probe_video_size(ffmpeg, orig_video)
    y0, y1, x0, x1 = content_box(canvas_w, canvas_h, src_w, src_h, GRID_W, grid_h)
    crop: tuple[float, float, float, float] | None = source_crop(
        canvas_w, canvas_h, src_w, src_h, GRID_W, grid_h
    )
    if [round(v) for v in crop] == [0, 0, src_w, src_h]:
        crop = None  # the whole source is kept: no crop filter
    rec_f, rec_pts = decode_video(ffmpeg, recording, GRID_W, grid_h)
    rec_f = rec_f[:, y0:y1, x0:x1]
    span = float(rec_pts.max() - rec_pts.min())
    start = max(0.0, center_s + float(rec_pts.min()) - VIDEO_WINDOW_S - 0.5)
    orig_f, orig_pts = decode_video(
        ffmpeg,
        orig_video,
        x1 - x0,
        y1 - y0,
        start=start,
        duration=span + 2 * VIDEO_WINDOW_S + 1.0,
        crop=crop,
    )
    vid = video_offset(rec_f, rec_pts, orig_f, orig_pts, center_s=center_s)
    return {
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
        "source_crop_x_y_w_h": None if crop is None else [round(v, 1) for v in crop],
        "_offset_exact": vid["offset_s"],
        "_match_exact": vid["match"],
        "_contrast_exact": vid["contrast"],
    }


def measure(
    recording: str,
    orig_audio: str,
    orig_video: str,
    ffmpeg: str = "ffmpeg",
    max_av_ms: float = DEFAULT_MAX_AV_MS,
) -> dict:
    """Full analysis of one recording. An AUDIO-step error propagates (the
    caller reports cannot_measure). A PICTURE-step error becomes a video-side
    cannot-measure, so dropouts already found still fail the run."""
    rec_a, rec_a_t0 = decode_audio(ffmpeg, recording)
    orig_a, orig_a_t0 = decode_audio(ffmpeg, orig_audio)
    aud = audio_offset(rec_a, orig_a, SR, rec_a_t0, orig_a_t0)
    aligned = orig_a[aud["lag"] : aud["lag"] + len(rec_a)]
    drops = dropout_blocks(rec_a, aligned, SR, BLOCK_MS, rec_a_t0)

    video_error = None
    try:
        video = _measure_video(ffmpeg, recording, orig_video, aud["offset_s"])
        match, contrast = video.pop("_match_exact"), video.pop("_contrast_exact")
        av_ms: float | None = (aud["offset_s"] - video.pop("_offset_exact")) * 1000.0
    except Exception as exc:  # noqa: BLE001 - reported loudly as a video-side cannot_measure
        traceback.print_exc(file=sys.stderr)
        video_error = f"video analysis error: {type(exc).__name__}: {exc}"
        video = {"error": video_error}
        match = contrast = float("nan")
        av_ms = None

    status, reasons, sides = verdict(
        aud["corr"],
        match,
        contrast,
        float("nan") if av_ms is None else av_ms,
        drops["dropout_count"],
        max_av_ms,
        video_error,
    )
    return {
        "status": status,
        "reasons": reasons,
        "unmeasurable_sides": sides,
        "av_ms": None if av_ms is None else round(av_ms, 1),
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
        "video": video,
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
        f"video_contrast={video.get('contrast')} dropouts={drops.get('dropout_count')} "
        f"dropout_ms={drops.get('dropout_ms')} "
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
            "unmeasurable_sides": ["error"],
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
