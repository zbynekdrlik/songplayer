"""window_merge.py — pure window math for the #144 one-call `win60` arm.

The `win60` arm sends the SAME one-call prompt to Gemini on 60 s slices of the
vocal track (5 s overlap) instead of the whole song — the only mitigation for
the whole-song timing drift measured on 2026-08-05 (timestamps jumped at
silences, 13.6 % of lines landed past the audio end). This module holds the
arithmetic only; slicing the WAV (ffmpeg `atrim`) and calling the model live in
`backends/gemini38_flash.py`.

Invariants (the project rule: never synthesized / evenly distributed timing):

- A model-produced timestamp is only ever SHIFTED by its window's offset —
  never re-estimated, interpolated, clamped or repaired.
- A line that starts at or after the end of the audio the call received is
  physically impossible: it is REMOVED and COUNTED (`n_past_end`), never kept
  and never moved back inside the audio. The count is reported by the backend
  (`metadata.n_lines_past_audio_end`) and by the scorer, because the #144
  decision rule requires 0 such lines for an arm to win.
- A line that starts inside the audio but whose `end_ms` runs past it is kept
  UNCHANGED and counted separately (`n_end_overrun`) — the scored field is the
  start, and the end is reported as-is rather than clamped.
- A line of window k (k >= 1) whose start falls inside the overlap it shares
  with window k-1 is dropped as a DUPLICATE only when window k-1 already
  produced a near-identical line (normalized text ratio >= DUP_TEXT_RATIO)
  that reaches into that overlap. A repeated chorus elsewhere in the song is a
  real second occurrence and is never dropped. The earlier window's copy wins.
"""

from __future__ import annotations

import difflib
import re
from dataclasses import dataclass, field
from typing import Any

DEFAULT_WIN_MS = 60_000
DEFAULT_OVERLAP_MS = 5_000

# Normalized-text similarity at/above which two lines in a shared overlap are
# the same sung line heard by both windows (design record 5801233778: >= 0.8).
DUP_TEXT_RATIO = 0.8

# An earlier-window line is a duplicate candidate only if it reaches into the
# shared overlap (its end at or after `overlap_start - DUP_REACH_SLACK_MS`) —
# a line sung just before the boundary may end a little early in the model's
# timing, but a line ending tens of seconds earlier is a chorus repeat.
DUP_REACH_SLACK_MS = 1_000

_PUNCT_RE = re.compile(r"[^\w\s]", re.UNICODE)
_WS_RE = re.compile(r"\s+")


def normalize_text(s: str) -> str:
    """Same normalization as `score_one_call.normalize_text` (lowercase,
    punctuation stripped, whitespace collapsed) — duplicated rather than
    imported so this module stays dependency-free and pure."""
    s = _PUNCT_RE.sub("", s.lower())
    return _WS_RE.sub(" ", s).strip()


def text_similarity(a: str, b: str) -> float:
    return difflib.SequenceMatcher(None, normalize_text(a), normalize_text(b)).ratio()


def split_windows(
    duration_ms: int,
    win_ms: int = DEFAULT_WIN_MS,
    overlap_ms: int = DEFAULT_OVERLAP_MS,
) -> list[tuple[int, int]]:
    """Cover [0, duration_ms) with windows of `win_ms` that each overlap the
    previous one by `overlap_ms`. The last window ends exactly at
    `duration_ms` and may be shorter; a song no longer than one window is a
    single window. Never emits a zero-length window."""
    if duration_ms <= 0:
        raise ValueError(f"duration_ms must be > 0, got {duration_ms}")
    if overlap_ms < 0:
        raise ValueError(f"overlap_ms must be >= 0, got {overlap_ms}")
    if win_ms <= overlap_ms:
        raise ValueError(
            f"win_ms ({win_ms}) must be greater than overlap_ms ({overlap_ms})"
        )
    step = win_ms - overlap_ms
    windows: list[tuple[int, int]] = []
    start = 0
    while True:
        end = min(start + win_ms, duration_ms)
        windows.append((start, end))
        if end >= duration_ms:
            return windows
        start += step


@dataclass
class ClipResult:
    lines: list[dict[str, Any]]
    n_past_end: int
    n_end_overrun: int


def clip_past_end(lines: list[dict[str, Any]], end_ms: int) -> ClipResult:
    """Remove (and count) every line starting at or after `end_ms`; count,
    but keep unchanged, lines that start inside and end past it."""
    kept = [ln for ln in lines if ln["start_ms"] < end_ms]
    return ClipResult(
        lines=kept,
        n_past_end=len(lines) - len(kept),
        n_end_overrun=sum(1 for ln in kept if ln["end_ms"] > end_ms),
    )


@dataclass
class MergeResult:
    lines: list[dict[str, Any]]
    n_duplicates_dropped: int = 0
    n_past_end: int = 0
    n_end_overrun: int = 0
    duplicates: list[dict[str, Any]] = field(default_factory=list)


def merge_window_lines(
    per_window: list[tuple[int, list[dict[str, Any]]]],
    *,
    audio_end_ms: int,
    overlap_ms: int = DEFAULT_OVERLAP_MS,
) -> MergeResult:
    """Merge per-window line lists (window-relative ms) into one song.

    `per_window` is `[(offset_ms, lines), ...]` in window order. Each line's
    `start_ms`/`end_ms` gets its window's offset added (copies — the input is
    never mutated); overlap duplicates are dropped per the module docstring;
    the result is sorted by start (stable) and every line starting at or after
    `audio_end_ms` is removed and counted."""
    merged: list[tuple[int, dict[str, Any]]] = []  # (window index, line)
    duplicates: list[dict[str, Any]] = []

    for k, (offset, lines) in enumerate(per_window):
        shifted = [
            {**ln, "start_ms": ln["start_ms"] + offset, "end_ms": ln["end_ms"] + offset}
            for ln in lines
        ]
        if k > 0:
            overlap_end = offset + overlap_ms
            prev = [
                ln
                for w, ln in merged
                if w == k - 1 and ln["end_ms"] >= offset - DUP_REACH_SLACK_MS
            ]
            kept: list[dict[str, Any]] = []
            for ln in shifted:
                in_overlap = offset <= ln["start_ms"] < overlap_end
                if in_overlap and any(
                    text_similarity(ln["text"], p["text"]) >= DUP_TEXT_RATIO
                    for p in prev
                ):
                    duplicates.append(ln)
                    continue
                kept.append(ln)
            shifted = kept
        merged.extend((k, ln) for ln in shifted)

    ordered = sorted((ln for _, ln in merged), key=lambda ln: ln["start_ms"])
    clip = clip_past_end(ordered, audio_end_ms)
    return MergeResult(
        lines=clip.lines,
        n_duplicates_dropped=len(duplicates),
        n_past_end=clip.n_past_end,
        n_end_overrun=clip.n_end_overrun,
        duplicates=duplicates,
    )
