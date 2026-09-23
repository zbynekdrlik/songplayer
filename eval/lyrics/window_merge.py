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
- Overlap de-duplication (`merge_window_lines`): a line of window k (k >= 1)
  starting inside the overlap it shares with window k-1 is the SAME sung line
  as an earlier-window line reaching into that overlap when (one-to-one, the
  closest start wins):
    * `full` — normalized text ratio >= DUP_TEXT_RATIO and the starts within
      DUP_MAX_START_DELTA_MS -> the later copy is dropped;
    * `later_longer` — the earlier copy was cut at its window's END (its text
      is a word-prefix of the later one) and the starts are close -> the
      earlier, incomplete copy is dropped and the complete later one kept;
    * `later_fragment` — the later copy was cut at its window's START (its
      text is a word-substring of the earlier one) and it starts inside the
      earlier line's span -> the fragment is dropped.
  A same-text line further away in time is a real repetition (worship choruses
  repeat within seconds) and is kept.
"""

from __future__ import annotations

import difflib
from dataclasses import dataclass, field
from typing import Any

# One normalization for de-dup and scoring (lowercase, punctuation stripped,
# whitespace collapsed) — `score_one_call` is stdlib-only, so importing it
# keeps this module pure.
from eval.lyrics.score_one_call import normalize_text

DEFAULT_WIN_MS = 60_000
DEFAULT_OVERLAP_MS = 5_000

# Normalized-text similarity at/above which two lines in a shared overlap are
# the same sung line heard by both windows (design record 5801233778: >= 0.8).
DUP_TEXT_RATIO = 0.8

# Two windows hearing the same sung line put its start within this distance of
# each other; the same text further apart is a real repetition (a chant
# "Holy holy" repeats every ~2-4 s).
DUP_MAX_START_DELTA_MS = 1_500

# An earlier-window line is a duplicate candidate only if it reaches into the
# shared overlap (its end at or after `overlap_start - DUP_REACH_SLACK_MS`) —
# a line sung just before the boundary may end a little early in the model's
# timing, but a line ending tens of seconds earlier is a chorus repeat.
DUP_REACH_SLACK_MS = 1_000


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
    single window. A tail that would add LESS new audio than `overlap_ms` is
    folded into the previous window (at most `win_ms + overlap_ms` long)
    instead of a sliver call on a clip lying almost entirely in the overlap."""
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
        if duration_ms - end < overlap_ms:
            end = duration_ms
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


def same_line_kind(later: dict[str, Any], earlier: dict[str, Any]) -> str | None:
    """How `later` (window k) duplicates `earlier` (window k-1), or None.
    See the module docstring for the three kinds."""
    a = normalize_text(later["text"])
    b = normalize_text(earlier["text"])
    if not a or not b:
        return None
    close = abs(later["start_ms"] - earlier["start_ms"]) <= DUP_MAX_START_DELTA_MS
    if close and difflib.SequenceMatcher(None, a, b).ratio() >= DUP_TEXT_RATIO:
        return "full"
    if close and len(a) > len(b) and (a + " ").startswith(b + " "):
        return "later_longer"
    inside = (
        earlier["start_ms"]
        <= later["start_ms"]
        <= earlier["end_ms"] + DUP_REACH_SLACK_MS
    )
    if inside and a != b and f" {a} " in f" {b} ":
        return "later_fragment"
    return None


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
    never mutated); overlap duplicates are resolved per the module docstring
    (every dropped copy is returned in `duplicates`, with its `kind`); the
    result is sorted by start (stable) and every line starting at or after
    `audio_end_ms` is removed and counted."""
    merged: list[tuple[int, dict[str, Any]]] = []  # (window index, line)
    duplicates: list[dict[str, Any]] = []

    for k, (offset, lines) in enumerate(per_window):
        shifted = sorted(
            (
                {
                    **ln,
                    "start_ms": ln["start_ms"] + offset,
                    "end_ms": ln["end_ms"] + offset,
                }
                for ln in lines
            ),
            key=lambda ln: ln["start_ms"],
        )
        if k == 0:
            merged.extend((k, ln) for ln in shifted)
            continue
        overlap_end = offset + overlap_ms
        prev_idx = [
            i
            for i, (w, ln) in enumerate(merged)
            if w == k - 1 and ln["end_ms"] >= offset - DUP_REACH_SLACK_MS
        ]
        consumed: set[int] = set()
        for ln in shifted:
            match: tuple[int, str] | None = None
            if offset <= ln["start_ms"] < overlap_end:
                candidates = [
                    (abs(ln["start_ms"] - merged[i][1]["start_ms"]), i, kind)
                    for i in prev_idx
                    if i not in consumed
                    and (kind := same_line_kind(ln, merged[i][1])) is not None
                ]
                if candidates:
                    _, i, kind = min(candidates)
                    match = (i, kind)
            if match is None:
                merged.append((k, ln))
                continue
            i, kind = match
            consumed.add(i)
            if kind == "later_longer":
                duplicates.append({**merged[i][1], "kind": kind})
                merged[i] = (k, ln)
            else:
                duplicates.append({**ln, "kind": kind})

    ordered = sorted((ln for _, ln in merged), key=lambda ln: ln["start_ms"])
    clip = clip_past_end(ordered, audio_end_ms)
    return MergeResult(
        lines=clip.lines,
        n_duplicates_dropped=len(duplicates),
        n_past_end=clip.n_past_end,
        n_end_overrun=clip.n_end_overrun,
        duplicates=duplicates,
    )
