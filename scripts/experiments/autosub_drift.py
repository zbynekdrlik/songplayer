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
import re
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to local copy of songplayer.db")
    parser.add_argument("--out", required=True, help="Markdown report output path")
    parser.parse_args()
    print("not implemented yet", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
