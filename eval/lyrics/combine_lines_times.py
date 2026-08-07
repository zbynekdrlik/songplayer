#!/usr/bin/env python3
"""combine_lines_times.py — recombine a LINE source's text with a TIME
source's real per-word ASR timestamps, via monotonic word-sequence
alignment.

Motivation (2026-08-05 offline experiment): the one-call north-star sweep
(`score_one_call.py`, `reports/2026-08-05-one-call-northstar.md`) showed
audio-LLM backends (`gemini36-flash`, `qwen35-omni`) write good line CONTENT
but estimate line TIMING badly (as low as 7.7% of lines land within the
400ms wall-timing gate), while dedicated ASR backends with real acoustic
alignment (`soniox-v5`, `aai-u35-translate`) time individual WORDS
precisely but have no concept of a lyric LINE — `soniox-v5`'s naive
silence-gap line grouping over-segments sung melisma into ~2x the gold
line count. This module tests the hybrid: keep the LLM's line content
(text/text_sk), replace its timestamps with the span of the SAME words
found in a dedicated ASR backend's word-level timestamp stream.

Method — word-sequence alignment (per `reports/2026-08-05-combine-experiment.md`):

1. Flatten the TIME source's `lines[].words[]` into one ordered,
   chronological word stream (`flatten_time_source`). Both `soniox-v5` and
   `aai-u35-translate` already emit whole-word (not sub-word-token) `words[]`
   with real integer `start_ms`/`end_ms` per word — see their
   `backends/*.py` — so no sub-word merging is needed here; a time-source
   line lacking `words` contributes nothing and is logged, not guessed.
2. Flatten the LINE source's `lines[].text` into an ordered word stream too
   (`flatten_line_source`, one entry per whitespace-split token, tagging
   which line it came from). The line source's OWN per-word timestamps (if
   any, e.g. `gemini36-flash`'s LLM-guessed `words[]`) are never read here —
   only word IDENTITY matters; timing always comes from the time source.
3. Align the two normalized-token sequences with
   `difflib.SequenceMatcher(autojunk=False)` (`align_word_streams`). This is
   a MONOTONIC alignment by construction — Ratcliff/Obershelp never emits an
   out-of-order match — which is exactly what makes it robust to a repeated
   chorus: the Nth occurrence of a repeated phrase in the line source can
   only align to the Nth-or-later occurrence in the time source, never back
   onto an earlier one already consumed by an earlier line. `autojunk=False`
   is deliberate: with autojunk ON, a token appearing very often (a chant
   word repeated dozens of times) is treated as "popular" noise and excluded
   from matching — precisely the repeated-content bias this experiment must
   NOT have.
4. Within an `equal` opcode, every token pair is an exact normalized-text
   match. Within a same-length `replace` opcode, each position is ALSO
   probed with a light fuzzy check (`SequenceMatcher.ratio() >=
   FUZZY_MATCH_RATIO_THRESHOLD`) to recover trivial spelling/contraction
   drift ("cause" vs "caus") while genuinely different word choices ("'cos"
   vs "because", ratio 0.4) are correctly left unaligned rather than
   force-matched. Unequal-length `replace` blocks, `insert`, and `delete`
   blocks are left entirely unaligned — no fuzzy recovery is attempted there
   because there is no reliable position-to-position pairing.
5. For each LINE-source line: `start_ms` = min `start_ms` among its aligned
   words, `end_ms` = max `end_ms` (equivalently first/last in word order,
   since the alignment is monotonic and the time source is chronological).
   A line with ZERO aligned words is UNTIMED — `start_ms`/`end_ms` are
   `None`. Never interpolated, never guessed.
6. `_enforce_monotonic` clamps any TIMED line's `start_ms` up to the
   previous TIMED line's `end_ms` (a floor that only advances on TIMED
   lines — UNTIMED lines are skipped and do not move it), matching the
   `floor_start_ms` sanitizer pattern already used in the production lyrics
   pipeline (`crates/sp-server/src/lyrics/merge.rs`, pipeline v10 history in
   the project `CLAUDE.md`).

Usage (single fixture):
    python3 eval/lyrics/combine_lines_times.py \\
        --raw-dir eval/lyrics/reports/2026-08-05-raw \\
        --line-backend qwen35-omni --time-backend soniox-v5 \\
        --video-id 5JW87KKDTcU \\
        --out /tmp/combined.json

`combine_lines_times()` / `combine_backend_outputs()` are the importable
entry points used by `run_combine_experiment.py` to batch this across every
fixture and every (line, time) combination.
"""

from __future__ import annotations

import argparse
import difflib
import json
import logging
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

logger = logging.getLogger("lyrics_eval.combine_lines_times")

_WORD_PUNCT_RE = re.compile(r"[^\w]", re.UNICODE)

# Positional fuzzy-match ratio applied only within equal-LENGTH "replace"
# blocks of the monotonic alignment. Chosen empirically (see module
# docstring / reports/2026-08-05-combine-experiment.md "mismatch examples"):
# 0.5 recovers spelling/contraction drift ("cause" vs "caus" -> 0.89,
# "gonna" vs "going" -> 0.6) while rejecting genuine word substitutions
# ("'cos" vs "because" -> 0.4).
FUZZY_MATCH_RATIO_THRESHOLD = 0.5

DEFAULT_RAW_DIR = Path(__file__).resolve().parent / "reports" / "2026-08-05-raw"


def normalize_word(word: str) -> str:
    """Lowercase + strip every non-word character (punctuation, apostrophes,
    trailing comma/period). Returns "" for a token that was pure
    punctuation — callers must skip empty results before matching."""
    return _WORD_PUNCT_RE.sub("", word).lower()


@dataclass(frozen=True)
class LineWord:
    """One word token from the LINE source, tagged with its owning line."""

    text: str
    line_idx: int


@dataclass(frozen=True)
class TimedWord:
    """One word token from the TIME source, carrying real ASR timestamps."""

    text: str
    start_ms: int
    end_ms: int
    source_line_idx: int


@dataclass(frozen=True)
class WordAlignment:
    line_word_idx: int
    time_word_idx: int
    kind: str  # "exact" | "fuzzy"
    ratio: float


@dataclass(frozen=True)
class MismatchExample:
    """One position pair inside an equal-length 'replace' opcode block,
    regardless of whether it cleared FUZZY_MATCH_RATIO_THRESHOLD. This is
    the concrete "LLM word vs ASR word at the same position" diagnostic
    view — `recovered=True` means the pair was close enough to align
    anyway (spelling/contraction drift); `recovered=False` means it is a
    real word-choice difference that correctly stayed unaligned."""

    line_word_idx: int
    time_word_idx: int
    line_text: str
    time_text: str
    ratio: float
    recovered: bool


def tokenize_line_text(text: str) -> list[str]:
    """Whitespace-split a line's text into words, in order. Punctuation
    stays attached to the token (normalize_word strips it later, at match
    time) so the raw word is preserved for diagnostics/mismatch reporting."""
    return text.split()


def flatten_line_source(lines: list[dict[str, Any]]) -> list[LineWord]:
    """Flatten a LINE-source backend's `lines[]` into an ordered word stream,
    tagging each word with the index of the line it came from. Only `text`
    is read — a line source's own timestamps (line-level or per-word) are
    never consulted here; this combiner replaces them entirely."""
    words: list[LineWord] = []
    for line_idx, line in enumerate(lines):
        text = line.get("text") or ""
        for tok in tokenize_line_text(text):
            words.append(LineWord(text=tok, line_idx=line_idx))
    return words


def flatten_time_source(lines: list[dict[str, Any]]) -> list[TimedWord]:
    """Flatten a TIME-source backend's `lines[].words[]` into one ordered,
    chronological word stream. Trusts the input line order as already
    chronological (both soniox-v5 and aai-u35-translate build lines by a
    sequential scan over a time-ordered word stream) rather than
    re-sorting, which could reorder ties incorrectly; a genuine
    out-of-order anomaly is logged, not silently fixed. A line with no
    `words` (or an empty list) contributes nothing and is logged — this is
    a real backend defect to notice, not something to paper over with a
    guessed timestamp."""
    words: list[TimedWord] = []
    prev_start_ms: int | None = None
    for line_idx, line in enumerate(lines):
        raw_words = line.get("words") or []
        if not raw_words:
            logger.warning(
                "time-source line %d has no word-level timings (text=%r) — "
                "contributes zero alignable words",
                line_idx,
                (line.get("text") or "")[:60],
            )
            continue
        for w in raw_words:
            text = w.get("text") or ""
            if not text.strip():
                continue
            start_ms = w.get("start_ms")
            end_ms = w.get("end_ms")
            if start_ms is None or end_ms is None:
                logger.warning("time-source word missing start_ms/end_ms: %r", w)
                continue
            start_ms = int(start_ms)
            end_ms = int(end_ms)
            if prev_start_ms is not None and start_ms < prev_start_ms:
                logger.warning(
                    "time-source word stream is not chronological: "
                    "start_ms=%d after previous start_ms=%d (line %d, text=%r)",
                    start_ms,
                    prev_start_ms,
                    line_idx,
                    text,
                )
            prev_start_ms = start_ms
            words.append(
                TimedWord(
                    text=text,
                    start_ms=start_ms,
                    end_ms=end_ms,
                    source_line_idx=line_idx,
                )
            )
    return words


def _align_and_diagnose(
    line_words: list[LineWord], time_words: list[TimedWord]
) -> tuple[list[WordAlignment], list[MismatchExample]]:
    """Single difflib pass producing both the accepted alignments and every
    equal-length 'replace'-block position pair (accepted or not) — shared
    by align_word_streams() and diagnose_replace_block_mismatches() so the
    matcher only runs once per call."""
    line_norm = [normalize_word(w.text) for w in line_words]
    time_norm = [normalize_word(w.text) for w in time_words]

    matcher = difflib.SequenceMatcher(None, line_norm, time_norm, autojunk=False)
    alignments: list[WordAlignment] = []
    mismatches: list[MismatchExample] = []
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            for k in range(i2 - i1):
                li_idx, ti_idx = i1 + k, j1 + k
                if line_norm[li_idx] and time_norm[ti_idx]:
                    alignments.append(WordAlignment(li_idx, ti_idx, "exact", 1.0))
        elif tag == "replace" and (i2 - i1) == (j2 - j1):
            for k in range(i2 - i1):
                li_idx, ti_idx = i1 + k, j1 + k
                a, b = line_norm[li_idx], time_norm[ti_idx]
                if not a or not b:
                    continue
                ratio = round(difflib.SequenceMatcher(None, a, b).ratio(), 3)
                recovered = ratio >= FUZZY_MATCH_RATIO_THRESHOLD
                if recovered:
                    alignments.append(WordAlignment(li_idx, ti_idx, "fuzzy", ratio))
                mismatches.append(
                    MismatchExample(
                        line_word_idx=li_idx,
                        time_word_idx=ti_idx,
                        line_text=line_words[li_idx].text,
                        time_text=time_words[ti_idx].text,
                        ratio=ratio,
                        recovered=recovered,
                    )
                )
        # tag in {"insert", "delete"}, or an unequal-length "replace": left
        # unaligned. There is no reliable position-to-position pairing to
        # probe there, so no fuzzy recovery/diagnostic is attempted.
    alignments.sort(key=lambda a: a.line_word_idx)
    return alignments, mismatches


def align_word_streams(
    line_words: list[LineWord], time_words: list[TimedWord]
) -> list[WordAlignment]:
    """Monotonic word-sequence alignment — see module docstring for the full
    method. Returns one WordAlignment per matched pair, sorted by
    line_word_idx. A line word or time word can appear in at most one
    alignment (SequenceMatcher opcodes partition both sequences)."""
    alignments, _mismatches = _align_and_diagnose(line_words, time_words)
    return alignments


def diagnose_replace_block_mismatches(
    line_words: list[LineWord], time_words: list[TimedWord]
) -> list[MismatchExample]:
    """Every equal-length 'replace'-block position pair from the SAME
    alignment align_word_streams() would compute, whether or not it was
    accepted — the concrete "what did the two backends actually disagree
    on" view used for the mismatch-analysis section of
    reports/2026-08-05-combine-experiment.md."""
    _alignments, mismatches = _align_and_diagnose(line_words, time_words)
    return mismatches


def _enforce_monotonic(
    lines: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], int]:
    """Clamp every TIMED line's start_ms up to the previous TIMED line's
    end_ms (a floor that only advances on TIMED lines; UNTIMED lines are
    skipped and never move it) and end_ms up to at least start_ms. Mirrors
    the floor_start_ms sanitizer already used in the production lyrics
    pipeline (see project CLAUDE.md, pipeline v10). Returns
    (lines, violation_count)."""
    floor_ms = 0
    violations = 0
    out: list[dict[str, Any]] = []
    for line in lines:
        if not line["timed"]:
            out.append(line)
            continue
        start_ms, end_ms = line["start_ms"], line["end_ms"]
        if start_ms < floor_ms:
            logger.warning(
                "monotonic violation: line start_ms=%d < floor=%d (text=%r) — clamping",
                start_ms,
                floor_ms,
                (line.get("text") or "")[:60],
            )
            start_ms = floor_ms
            violations += 1
        if end_ms < start_ms:
            end_ms = start_ms
        line = {**line, "start_ms": start_ms, "end_ms": end_ms}
        floor_ms = end_ms
        out.append(line)
    return out, violations


def combine_lines_times(
    line_source_lines: list[dict[str, Any]],
    time_source_lines: list[dict[str, Any]],
) -> dict[str, Any]:
    """Core entry point. `line_source_lines` supplies text/text_sk content
    (an audio-LLM backend's `lines[]`); `time_source_lines` supplies real
    per-word ASR timestamps (a dedicated-ASR backend's `lines[]`, MUST carry
    `words[]` with start_ms/end_ms). Returns
    `{"lines": [...], "stats": {...}}`.

    Each output line is `{text, start_ms, end_ms, text_sk, words: None,
    timed, n_words_in_line, n_words_aligned}` — start_ms/end_ms are None
    (and timed=False) when zero words in that line aligned to the time
    source. `words` is always None on the combined output: this experiment
    times LINES, not individual words, so no per-word timing is fabricated
    for the combined line even though the alignment technically knows one
    per-word time per aligned word — see reports/2026-08-05-combine-experiment.md
    for why (the project's own CLAUDE.md v18 already dropped synthesized
    per-word timings from production for exactly this reason: partial,
    alignment-derived per-word coverage would look line-shaped but isn't
    trustworthy at word granularity)."""
    line_words = flatten_line_source(line_source_lines)
    time_words = flatten_time_source(time_source_lines)
    alignments = align_word_streams(line_words, time_words)

    aligned_by_line_word_idx: dict[int, TimedWord] = {
        a.line_word_idx: time_words[a.time_word_idx] for a in alignments
    }

    words_per_line: dict[int, list[int]] = {}
    for word_idx, lw in enumerate(line_words):
        words_per_line.setdefault(lw.line_idx, []).append(word_idx)

    out_lines: list[dict[str, Any]] = []
    n_words_total = 0
    n_words_aligned = 0
    for line_idx, line in enumerate(line_source_lines):
        word_indices = words_per_line.get(line_idx, [])
        n_words_total += len(word_indices)
        aligned_here = [
            aligned_by_line_word_idx[wi]
            for wi in word_indices
            if wi in aligned_by_line_word_idx
        ]
        n_words_aligned += len(aligned_here)
        if aligned_here:
            start_ms = min(tw.start_ms for tw in aligned_here)
            end_ms = max(tw.end_ms for tw in aligned_here)
            timed = True
        else:
            start_ms = None
            end_ms = None
            timed = False
        out_lines.append(
            {
                "text": line.get("text"),
                "start_ms": start_ms,
                "end_ms": end_ms,
                "text_sk": line.get("text_sk"),
                "words": None,
                "timed": timed,
                "n_words_in_line": len(word_indices),
                "n_words_aligned": len(aligned_here),
            }
        )

    out_lines, n_violations = _enforce_monotonic(out_lines)

    stats = {
        "n_lines": len(out_lines),
        "n_lines_timed": sum(1 for line in out_lines if line["timed"]),
        "n_lines_untimed": sum(1 for line in out_lines if not line["timed"]),
        "n_words_total": n_words_total,
        "n_words_aligned": n_words_aligned,
        "word_align_rate": (n_words_aligned / n_words_total) if n_words_total else None,
        "n_monotonic_violations_clamped": n_violations,
    }
    return {"lines": out_lines, "stats": stats}


def load_backend_output(raw_dir: Path, backend: str, video_id: str) -> dict[str, Any]:
    path = raw_dir / f"{backend}_{video_id}.json"
    if not path.exists():
        raise FileNotFoundError(
            f"no output file for backend={backend} video_id={video_id} ({path})"
        )
    return json.loads(path.read_text(encoding="utf-8"))


def combine_backend_outputs(
    raw_dir: Path, line_backend: str, time_backend: str, video_id: str
) -> dict[str, Any]:
    """Load both backends' raw output JSON from raw_dir and combine them.
    Raises FileNotFoundError if either is missing — never silently skips."""
    line_output = load_backend_output(raw_dir, line_backend, video_id)
    time_output = load_backend_output(raw_dir, time_backend, video_id)
    result = combine_lines_times(
        line_output.get("lines") or [], time_output.get("lines") or []
    )
    result["backend_id"] = f"combo:{line_backend}+{time_backend}"
    result["line_backend"] = line_backend
    result["time_backend"] = time_backend
    result["video_id"] = video_id
    result["wav_path"] = line_output.get("wav_path")
    result["duration_ms"] = time_output.get("duration_ms") or line_output.get(
        "duration_ms"
    )
    return result


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(levelname)s %(name)s: %(message)s"
    )
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--raw-dir", type=Path, default=DEFAULT_RAW_DIR)
    p.add_argument("--line-backend", required=True, help="e.g. qwen35-omni")
    p.add_argument("--time-backend", required=True, help="e.g. soniox-v5")
    p.add_argument("--video-id", required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)

    result = combine_backend_outputs(
        args.raw_dir, args.line_backend, args.time_backend, args.video_id
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    stats = result["stats"]
    print(
        f"combined {args.line_backend}+{args.time_backend} video={args.video_id}: "
        f"{stats['n_lines_timed']}/{stats['n_lines']} lines timed, "
        f"word_align_rate={stats['word_align_rate']}"
    )
    logger.info("wrote %s", args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
