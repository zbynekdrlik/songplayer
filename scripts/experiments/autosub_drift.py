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
import re
import sys

_PUNCT_RE = re.compile(r"[^\w]")
_NOISE_TOKENS = {"[music]", ">>", "[applause]", "[laughter]"}


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
