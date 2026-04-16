#!/usr/bin/env python3
"""
autosub_drift.py — one-shot Phase 2 validation experiment.

Pulls YouTube auto-subtitles + Qwen3 reference word timings for the
three songs from issue #29, computes per-song drift statistics, and
writes a markdown report at docs/experiments/2026-04-16-autosub-drift.md.

Usage:
    python scripts/experiments/autosub_drift.py
        --db /tmp/songplayer.db
        --out docs/experiments/2026-04-16-autosub-drift.md

Spec: docs/superpowers/specs/2026-04-16-phase2-autosub-drift-experiment-design.md
"""

import argparse
import json
import math
import re
import statistics
import sys
from dataclasses import dataclass
from typing import List

_PUNCT_RE = re.compile(r"[^\w]")
_NOISE_TOKENS = {"[music]", ">>", "[applause]", "[laughter]"}


@dataclass(frozen=True)
class Word:
    text: str
    start_ms: int


def parse_json3(json_text: str) -> List[Word]:
    """Parse yt-dlp's json3 caption format into a flat word stream.

    Each json3 event has a tStartMs and zero or more segs. A seg has a
    utf8 fragment and an optional tOffsetMs (relative to the event).
    Word-level captions emit one seg per word with its own offset.
    Sentence-level captions emit one seg containing the full text with
    no offset — we split that on whitespace and assign every word the
    event's tStartMs (lower-confidence fallback, noted in the report).
    """
    doc = json.loads(json_text)
    out: List[Word] = []
    for event in doc.get("events", []):
        segs = event.get("segs")
        if not segs:
            continue
        event_start = int(event.get("tStartMs", 0))

        if any("tOffsetMs" in seg for seg in segs):
            for seg in segs:
                fragment = seg.get("utf8", "")
                if not fragment.strip():
                    continue
                offset = int(seg.get("tOffsetMs", 0))
                out.append(Word(text=fragment.strip(), start_ms=event_start + offset))
        else:
            text = "".join(seg.get("utf8", "") for seg in segs).strip()
            for word in text.split():
                out.append(Word(text=word, start_ms=event_start))
    return out


def normalize_word(text: str) -> str:
    """Lowercase, strip punctuation, drop noise tokens.

    Returns an empty string for noise tokens, empty input, or
    whitespace-only input. Used to compare auto-sub words against
    Qwen3 words on equal footing.
    """
    s = text.strip().lower()
    if s in _NOISE_TOKENS or not s:
        return ""
    return _PUNCT_RE.sub("", s)


@dataclass(frozen=True)
class MatchResult:
    matched: int
    skipped: int
    drifts_ms: List[int]
    total_qwen_words: int
    total_autosub_words: int


def match_word_streams(
    qwen_words: List[Word],
    autosub_words: List[Word],
    window_n: int = 10,
) -> MatchResult:
    """Sequentially walk Qwen3 words; for each, search up to window_n
    auto-sub words ahead for the first exact-text match (after
    normalization). On match, record drift and advance the auto-sub
    pointer past the matched word. On miss, skip and leave the auto-sub
    pointer untouched. Strict forward walk; no backtracking.
    """
    drifts: List[int] = []
    matched = 0
    skipped = 0
    auto_idx = 0

    for q in qwen_words:
        q_norm = normalize_word(q.text)
        if not q_norm:
            skipped += 1
            continue

        found = -1
        for offset in range(window_n):
            cand_idx = auto_idx + offset
            if cand_idx >= len(autosub_words):
                break
            if normalize_word(autosub_words[cand_idx].text) == q_norm:
                found = cand_idx
                break

        if found >= 0:
            drifts.append(autosub_words[found].start_ms - q.start_ms)
            matched += 1
            auto_idx = found + 1
        else:
            skipped += 1

    return MatchResult(
        matched=matched,
        skipped=skipped,
        drifts_ms=drifts,
        total_qwen_words=len(qwen_words),
        total_autosub_words=len(autosub_words),
    )


@dataclass(frozen=True)
class DriftStats:
    count: int
    mean_ms: int
    median_ms: int
    rms_ms: float
    min_ms: int
    max_ms: int
    p05_ms: int
    p95_ms: int


def _nearest_rank_percentile(sorted_values: List[int], pct: float) -> int:
    if not sorted_values:
        return 0
    rank = max(1, math.ceil(pct / 100.0 * len(sorted_values)))
    return sorted_values[rank - 1]


def compute_stats(drifts_ms: List[int]) -> DriftStats:
    """Per-song drift summary. Returns zeros for empty input."""
    if not drifts_ms:
        return DriftStats(0, 0, 0, 0.0, 0, 0, 0, 0)
    s = sorted(drifts_ms)
    rms = math.sqrt(sum(d * d for d in drifts_ms) / len(drifts_ms))
    return DriftStats(
        count=len(drifts_ms),
        mean_ms=int(round(statistics.mean(drifts_ms))),
        median_ms=int(round(statistics.median(drifts_ms))),
        rms_ms=rms,
        min_ms=s[0],
        max_ms=s[-1],
        p05_ms=_nearest_rank_percentile(s, 5),
        p95_ms=_nearest_rank_percentile(s, 95),
    )


def make_histogram(drifts_ms: List[int], buckets: List[int]) -> str:
    """ASCII histogram. Buckets are bin EDGES (length N gives N-1 bins).

    Each bin label is `[lo, hi)`. Bar width is one `#` per drift. Returns
    a 'no data' string for empty input.
    """
    if not drifts_ms:
        return "no data"
    counts = [0] * (len(buckets) - 1)
    for d in drifts_ms:
        for i in range(len(buckets) - 1):
            if buckets[i] <= d < buckets[i + 1]:
                counts[i] += 1
                break
    lines = []
    label_width = max(
        len(f"[{buckets[i]}, {buckets[i + 1]})") for i in range(len(buckets) - 1)
    )
    for i, c in enumerate(counts):
        label = f"[{buckets[i]}, {buckets[i + 1]})".ljust(label_width)
        lines.append(f"{label} {'#' * c} ({c})")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to local copy of songplayer.db")
    parser.add_argument("--out", required=True, help="Markdown report output path")
    parser.parse_args()
    print("not implemented yet", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
