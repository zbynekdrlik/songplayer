# Phase 2 Auto-Sub Drift Experiment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a one-shot Python script that pulls YouTube auto-subtitles + Qwen3 reference word-timings for three test songs, computes drift statistics, and writes a committed markdown report deciding whether Phase 2 (skip-Qwen3 pipeline) is worth building.

**Architecture:** Single Python file `scripts/experiments/autosub_drift.py` containing pure functions (normalize, parse_json3, match_word_streams, compute_stats, make_histogram, write_report) plus I/O wrappers (yt-dlp subprocess, SCP from win-resolume, sqlite3 query). Pure functions get unit tests (pytest). I/O wrappers and the main wiring are verified by running the script end-to-end. The deliverable is the report at `docs/experiments/2026-04-16-autosub-drift.md`, not the script itself.

**Tech Stack:** Python 3.10+, yt-dlp, sqlite3 (stdlib), pytest. No new dependencies. No Rust changes. No CI changes.

**Spec:** `docs/superpowers/specs/2026-04-16-phase2-autosub-drift-experiment-design.md`

---

## File Structure

| Path                                                    | Purpose                                                                    |
| ------------------------------------------------------- | -------------------------------------------------------------------------- |
| `scripts/experiments/autosub_drift.py`                  | Single-file script: pure functions + I/O wrappers + `__main__`             |
| `scripts/experiments/test_autosub_drift.py`             | pytest unit tests for the pure functions                                   |
| `docs/experiments/2026-04-16-autosub-drift.md`          | Durable report — produced by running the script, committed by hand         |

No `__init__.py`. No package layout. Flat, throwaway, single-purpose.

---

## Task 1: Scaffold the script and tests

**Files:**
- Create: `scripts/experiments/autosub_drift.py`
- Create: `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Create the script with module docstring and stub `main()`**

```python
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
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to local copy of songplayer.db")
    parser.add_argument("--out", required=True, help="Markdown report output path")
    parser.parse_args()
    print("not implemented yet", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Create the test file with a placeholder test**

```python
"""Unit tests for autosub_drift.py pure functions."""

def test_placeholder():
    assert True
```

- [ ] **Step 3: Verify test runs**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `1 passed`

- [ ] **Step 4: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): scaffold autosub_drift.py + test file (#29)"
```

---

## Task 2: Implement `normalize_word`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
from autosub_drift import normalize_word


def test_normalize_lowercases():
    assert normalize_word("Love") == "love"


def test_normalize_strips_punctuation():
    assert normalize_word("love!") == "love"
    assert normalize_word("you're") == "youre"
    assert normalize_word("...don't?") == "dont"


def test_normalize_returns_empty_for_noise():
    assert normalize_word("[music]") == ""
    assert normalize_word(">>") == ""
    assert normalize_word("") == ""
    assert normalize_word("   ") == ""


def test_normalize_keeps_alphanumeric():
    assert normalize_word("123abc") == "123abc"
```

- [ ] **Step 2: Run tests, expect import error / failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError` (normalize_word not defined)

- [ ] **Step 3: Implement `normalize_word`**

Add to `autosub_drift.py` above `main()`:

```python
import re

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
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 5 passed

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add normalize_word with unit tests"
```

---

## Task 3: Implement `parse_json3`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
import json
from autosub_drift import parse_json3, Word


def test_parse_json3_extracts_word_level_starts():
    """yt-dlp json3 events contain segs with offset (relative to event tStartMs)."""
    raw = {
        "events": [
            {
                "tStartMs": 1000,
                "dDurationMs": 2000,
                "segs": [
                    {"utf8": "hello", "tOffsetMs": 0},
                    {"utf8": " ", "tOffsetMs": 200},
                    {"utf8": "world", "tOffsetMs": 250},
                ],
            }
        ]
    }
    words = parse_json3(json.dumps(raw))
    assert words == [
        Word(text="hello", start_ms=1000),
        Word(text="world", start_ms=1250),
    ]


def test_parse_json3_skips_whitespace_only_segs():
    raw = {
        "events": [
            {
                "tStartMs": 5000,
                "segs": [
                    {"utf8": "\n", "tOffsetMs": 0},
                    {"utf8": "ok", "tOffsetMs": 100},
                ],
            }
        ]
    }
    assert parse_json3(json.dumps(raw)) == [Word(text="ok", start_ms=5100)]


def test_parse_json3_falls_back_to_event_start_when_no_segs():
    """Sentence-level event without per-word offsets uses event tStartMs for the whole text."""
    raw = {
        "events": [
            {"tStartMs": 9000, "dDurationMs": 1000, "segs": [{"utf8": "whole sentence"}]}
        ]
    }
    # Single seg with no tOffsetMs -> treated as starting at the event boundary,
    # split by whitespace, all words get the same start_ms.
    assert parse_json3(json.dumps(raw)) == [
        Word(text="whole", start_ms=9000),
        Word(text="sentence", start_ms=9000),
    ]


def test_parse_json3_ignores_events_without_segs():
    raw = {"events": [{"tStartMs": 0}, {"tStartMs": 1000, "segs": [{"utf8": "yo"}]}]}
    assert parse_json3(json.dumps(raw)) == [Word(text="yo", start_ms=1000)]
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError: cannot import name 'parse_json3'`

- [ ] **Step 3: Implement `parse_json3`**

Add to `autosub_drift.py`:

```python
import json
from dataclasses import dataclass
from typing import List


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
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 9 passed (5 from Task 2 + 4 new)

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add parse_json3 with unit tests"
```

---

## Task 4: Implement `match_word_streams`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
from autosub_drift import match_word_streams, MatchResult


def test_match_perfect_alignment():
    qwen = [Word("hello", 1000), Word("world", 2000)]
    auto = [Word("hello", 1050), Word("world", 1980)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 0
    assert result.drifts_ms == [50, -20]


def test_match_skips_qwen_word_with_no_autosub_counterpart():
    qwen = [Word("hello", 1000), Word("ghost", 1500), Word("world", 2000)]
    auto = [Word("hello", 1000), Word("world", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 1
    assert result.drifts_ms == [0, 0]


def test_match_advances_only_on_match():
    """An unmatched qwen word does NOT advance the autosub pointer."""
    qwen = [Word("missing", 500), Word("hello", 1000), Word("world", 2000)]
    auto = [Word("hello", 1000), Word("world", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 1


def test_match_window_boundary_no_match_beyond_n():
    """If the matching autosub word is beyond window_n, it's a skip."""
    qwen = [Word("hello", 1000), Word("target", 2000)]
    # 11 noise words then "target"
    auto = [Word(f"noise{i}", 1000 + i * 10) for i in range(11)] + [Word("target", 2000)]
    qwen_only_target = [Word("target", 2000)]
    result = match_word_streams(qwen_only_target, auto, window_n=10)
    assert result.matched == 0
    assert result.skipped == 1


def test_match_returns_zero_match_rate_when_disjoint():
    qwen = [Word("apple", 1000), Word("banana", 2000)]
    auto = [Word("orange", 1000), Word("grape", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 0
    assert result.skipped == 2
    assert result.drifts_ms == []


def test_match_normalizes_before_compare():
    qwen = [Word("Hello!", 1000), Word("world", 2000)]
    auto = [Word("hello", 1100), Word("World.", 2050)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.drifts_ms == [100, 50]
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError: cannot import name 'match_word_streams'`

- [ ] **Step 3: Implement `match_word_streams`**

Add to `autosub_drift.py`:

```python
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
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 15 passed

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add match_word_streams (Option A matcher) with unit tests"
```

---

## Task 5: Implement `compute_stats` and `make_histogram`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
import math
from autosub_drift import compute_stats, make_histogram, DriftStats


def test_compute_stats_basic():
    drifts = [-100, 0, 100, 200]
    s = compute_stats(drifts)
    assert s.count == 4
    assert s.mean_ms == 50
    assert s.median_ms == 50
    assert s.min_ms == -100
    assert s.max_ms == 200
    assert math.isclose(s.rms_ms, math.sqrt((10000 + 0 + 10000 + 40000) / 4))


def test_compute_stats_empty_returns_zeros():
    s = compute_stats([])
    assert s.count == 0
    assert s.mean_ms == 0
    assert s.rms_ms == 0.0
    assert s.median_ms == 0
    assert s.min_ms == 0
    assert s.max_ms == 0
    assert s.p05_ms == 0
    assert s.p95_ms == 0


def test_compute_stats_percentiles_with_100_values():
    drifts = list(range(100))  # 0..99
    s = compute_stats(drifts)
    # nearest-rank percentile: p95 = value at index ceil(0.95 * 100) - 1 = 94
    assert s.p95_ms == 94
    assert s.p05_ms == 4


def test_make_histogram_buckets_and_renders_ascii():
    drifts = [-1500, -400, -50, 50, 400, 1500]
    buckets = [-2000, -1000, -500, -300, -100, 0, 100, 300, 500, 1000, 2000]
    text = make_histogram(drifts, buckets)
    assert "[-2000, -1000)" in text
    assert "[1000, 2000)" in text
    # one drift in each of the 6 represented buckets, others empty
    assert text.count("#") == 6


def test_make_histogram_handles_empty_drifts():
    text = make_histogram([], [-1000, 0, 1000])
    assert "no data" in text.lower()
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError`

- [ ] **Step 3: Implement `compute_stats` and `make_histogram`**

Add to `autosub_drift.py`:

```python
import math
import statistics


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
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 20 passed

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add compute_stats + make_histogram with unit tests"
```

---

## Task 6: Implement `classify_bucket` and `recommendation_from_results`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
from autosub_drift import classify_bucket, recommendation_from_buckets


def test_classify_bucket_green():
    assert classify_bucket(0) == "green"
    assert classify_bucket(299) == "green"


def test_classify_bucket_amber():
    assert classify_bucket(300) == "amber"
    assert classify_bucket(700) == "amber"


def test_classify_bucket_red():
    assert classify_bucket(701) == "red"
    assert classify_bucket(5000) == "red"


def test_recommendation_red_kills_project():
    assert recommendation_from_buckets(["green", "amber", "red"]) == "kill"


def test_recommendation_amber_downgrades_to_refine():
    assert recommendation_from_buckets(["green", "amber", "green"]) == "refine"


def test_recommendation_all_green_greenlights():
    assert recommendation_from_buckets(["green", "green", "green"]) == "greenlight"


def test_recommendation_empty_input_kills():
    """No data is not a positive signal."""
    assert recommendation_from_buckets([]) == "kill"
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError`

- [ ] **Step 3: Implement classification helpers**

Add to `autosub_drift.py`:

```python
def classify_bucket(rms_ms: float) -> str:
    """Map an RMS drift in ms to one of the spec's three decision buckets."""
    if rms_ms < 300:
        return "green"
    if rms_ms <= 700:
        return "amber"
    return "red"


def recommendation_from_buckets(buckets: List[str]) -> str:
    """Worst per-song bucket determines the project recommendation.

    One red song kills the project. One amber song downgrades to refine.
    All green greenlights. Empty input is treated as a kill (no signal
    is not a positive signal).
    """
    if not buckets:
        return "kill"
    if "red" in buckets:
        return "kill"
    if "amber" in buckets:
        return "refine"
    return "greenlight"
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 27 passed

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add bucket classification + project recommendation"
```

---

## Task 7: Implement `write_report`

**Files:** `scripts/experiments/autosub_drift.py`, `scripts/experiments/test_autosub_drift.py`

- [ ] **Step 1: Add failing tests**

Append to `test_autosub_drift.py`:

```python
from autosub_drift import write_report, SongResult


def test_write_report_includes_required_sections(tmp_path):
    results = [
        SongResult(
            video_id="abc123",
            title="Get This Party Started",
            artist="Planetshakers",
            error=None,
            match=MatchResult(
                matched=200, skipped=50,
                drifts_ms=[-100, 0, 100, 200, 300],
                total_qwen_words=250, total_autosub_words=270,
            ),
            stats=DriftStats(5, 100, 100, 173.2, -100, 300, -100, 300),
            histogram="bucket: ###",
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)

    text = out.read_text()
    assert "# Phase 2 Auto-Sub Drift Experiment" in text
    assert "## Methodology" in text
    assert "## Per-song results" in text
    assert "Get This Party Started" in text
    assert "Planetshakers" in text
    assert "abc123" in text
    assert "## Conclusion" in text
    assert "## Recommendation" in text
    assert "200/250" in text  # match rate
    assert "bucket: ###" in text  # histogram


def test_write_report_handles_missing_data_song(tmp_path):
    results = [
        SongResult(
            video_id="ghi789",
            title="?",
            artist="?",
            error="no auto-subs available",
            match=None,
            stats=None,
            histogram=None,
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "no auto-subs available" in text
    assert "ghi789" in text


def test_write_report_recommendation_section_cites_worst_bucket(tmp_path):
    results = [
        SongResult("a", "T1", "A1", None,
                   MatchResult(10, 0, [0] * 10, 10, 10),
                   DriftStats(10, 0, 0, 100.0, 0, 0, 0, 0), "h"),
        SongResult("b", "T2", "A2", None,
                   MatchResult(10, 0, [800] * 10, 10, 10),
                   DriftStats(10, 800, 800, 800.0, 800, 800, 800, 800), "h"),
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "kill" in text.lower()
    assert "T2" in text  # the red song must be cited
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: `ImportError`

- [ ] **Step 3: Implement `write_report`**

Add to `autosub_drift.py`:

```python
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class SongResult:
    video_id: str
    title: str
    artist: str
    error: Optional[str]
    match: Optional[MatchResult]
    stats: Optional[DriftStats]
    histogram: Optional[str]


def _render_song_section(r: SongResult) -> str:
    header = f"### {r.title} — {r.artist} (`{r.video_id}`)"
    url = f"https://www.youtube.com/watch?v={r.video_id}"
    if r.error:
        return f"{header}\n\n- URL: {url}\n- **No data: {r.error}**\n"
    assert r.match and r.stats and r.histogram is not None
    bucket = classify_bucket(r.stats.rms_ms)
    body = [
        header,
        "",
        f"- URL: {url}",
        f"- Match rate: **{r.match.matched}/{r.match.total_qwen_words}**"
        f" Qwen3 words matched ({r.match.matched / max(r.match.total_qwen_words, 1):.1%}),"
        f" {r.match.skipped} skipped",
        f"- Auto-sub stream: {r.match.total_autosub_words} words",
        f"- Drift: RMS **{r.stats.rms_ms:.0f} ms**, mean {r.stats.mean_ms} ms,"
        f" median {r.stats.median_ms} ms, min {r.stats.min_ms} ms, max {r.stats.max_ms} ms,"
        f" p05 {r.stats.p05_ms} ms, p95 {r.stats.p95_ms} ms",
        f"- Bucket: **{bucket}**",
        "",
        "Histogram (drift in ms, `#` = one Qwen3 word):",
        "",
        "```",
        r.histogram,
        "```",
        "",
    ]
    return "\n".join(body)


def write_report(results: List[SongResult], out_path: Path) -> None:
    """Render the durable markdown report to out_path."""
    valid = [r for r in results if r.match and r.stats]
    bucket_per_song = [classify_bucket(r.stats.rms_ms) for r in valid]
    rec = recommendation_from_buckets(bucket_per_song)

    parts: List[str] = []
    parts.append("# Phase 2 Auto-Sub Drift Experiment")
    parts.append("")
    parts.append(
        "Validation experiment for issue #29. Decides whether YouTube"
        " auto-subtitles carry word-level timestamps accurate enough on"
        " sung vocals to skip the Qwen3-ForcedAligner timing stage."
    )
    parts.append("")

    parts.append("## Methodology")
    parts.append("")
    parts.append(
        "- Auto-subs pulled with"
        " `yt-dlp --write-auto-subs --sub-format json3 --sub-langs en --skip-download`."
    )
    parts.append(
        "- Qwen3 reference word timings copied from win-resolume's"
        " production `songplayer.db` (read-only SCP, no remote write)."
    )
    parts.append(
        "- Matcher (Option A): sequential forward walk; for each Qwen3"
        " word, search up to 10 auto-sub words ahead for an exact text"
        " match after lowercasing + punctuation stripping. No backtrack."
        " Skipped words are reported separately and do NOT pollute the"
        " drift distribution."
    )
    parts.append(
        "- Decision rule: per-song RMS drift `< 300 ms` → green,"
        " `300–700 ms` → amber, `> 700 ms` → red. Worst per-song bucket"
        " sets the project recommendation (one red kills, one amber"
        " refines, all green greenlights)."
    )
    parts.append("")

    parts.append("## Per-song results")
    parts.append("")
    for r in results:
        parts.append(_render_song_section(r))

    parts.append("## Conclusion")
    parts.append("")
    parts.append("| Song | RMS drift | Bucket |")
    parts.append("| --- | --- | --- |")
    for r in valid:
        parts.append(
            f"| {r.title} — {r.artist} | {r.stats.rms_ms:.0f} ms |"
            f" {classify_bucket(r.stats.rms_ms)} |"
        )
    for r in results:
        if r.error:
            parts.append(f"| {r.title} — {r.artist} | n/a | no data ({r.error}) |")
    parts.append("")

    parts.append("## Recommendation")
    parts.append("")
    if rec == "kill":
        worst = next(
            (r for r in valid if classify_bucket(r.stats.rms_ms) == "red"),
            None,
        )
        cite = f" Worst-case song: **{worst.title}** at {worst.stats.rms_ms:.0f} ms RMS." if worst else ""
        parts.append(
            f"**KILL** — auto-sub timing is not accurate enough on sung"
            f" worship vocals to skip Qwen3.{cite} Close issue #29."
        )
    elif rec == "refine":
        worst = next(
            (r for r in valid if classify_bucket(r.stats.rms_ms) == "amber"),
            None,
        )
        cite = f" Worst-case song: **{worst.title}** at {worst.stats.rms_ms:.0f} ms RMS." if worst else ""
        parts.append(
            f"**REFINE** — auto-sub timing is workable but needs a"
            f" correction pass before it can replace Qwen3.{cite}"
            f" Phase 2 design must include a refinement stage."
        )
    else:
        parts.append(
            "**GREENLIGHT** — auto-sub timing is accurate enough across"
            " the test corpus. Phase 2 can use auto-sub timestamps"
            " directly with no refinement pass. Open the Phase 2 design"
            " brainstorm."
        )
    parts.append("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(parts), encoding="utf-8")
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cd scripts/experiments && python -m pytest test_autosub_drift.py -v`
Expected: 30 passed

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py scripts/experiments/test_autosub_drift.py
git commit -m "feat(experiment): add write_report markdown formatter with unit tests"
```

---

## Task 8: Add I/O wrappers (yt-dlp, sqlite, SCP)

**Files:** `scripts/experiments/autosub_drift.py`

These functions touch external systems and are verified by running the script end-to-end (Task 10). No unit tests.

- [ ] **Step 1: Add `fetch_autosubs`, `fetch_qwen_reference`, `pull_db_from_winresolume`**

Add to `autosub_drift.py`:

```python
import os
import shutil
import sqlite3
import subprocess
import tempfile


def pull_db_from_winresolume(local_path: Path) -> None:
    """SCP the production songplayer.db from win-resolume to local_path.

    Read-only — copies a snapshot, never writes back. Requires SSH
    config to win-resolume already in place (the dev machine has it).
    """
    remote = "win-resolume:/c/ProgramData/SongPlayer/songplayer.db"
    subprocess.run(
        ["scp", "-q", remote, str(local_path)],
        check=True,
    )


def fetch_autosubs(video_id: str, tmp_dir: Path) -> Optional[Path]:
    """Download English auto-subs as json3. Returns the json3 path on
    success, None if no auto-subs are available for this video."""
    out_template = tmp_dir / f"{video_id}.%(ext)s"
    try:
        subprocess.run(
            [
                "yt-dlp",
                "--write-auto-subs",
                "--sub-format", "json3",
                "--sub-langs", "en",
                "--skip-download",
                "--no-warnings",
                "-o", str(out_template),
                f"https://www.youtube.com/watch?v={video_id}",
            ],
            check=True,
            capture_output=True,
        )
    except subprocess.CalledProcessError as e:
        sys.stderr.write(f"yt-dlp failed for {video_id}: {e.stderr.decode(errors='replace')}\n")
        return None
    candidate = tmp_dir / f"{video_id}.en.json3"
    return candidate if candidate.exists() else None


def fetch_qwen_reference(db_path: Path, video_id: str) -> Optional[List[Word]]:
    """Pull Qwen3 word-level alignment for a video from the local DB
    snapshot. Returns None if no reference exists for this video.

    The schema is inspected at execution time before this function is
    written for real — see Task 9. The query below is a placeholder
    that Task 9 will replace with the actual table + column names.
    """
    conn = sqlite3.connect(str(db_path))
    try:
        # The Task 9 implementer must replace this query after running
        # `.schema` against the real DB. Until then, this raises so the
        # script fails loudly rather than silently returning None.
        raise NotImplementedError(
            "Replace this query after inspecting the real schema in Task 9."
        )
    finally:
        conn.close()
```

- [ ] **Step 2: Commit (no test for I/O wrappers)**

```bash
git add scripts/experiments/autosub_drift.py
git commit -m "feat(experiment): add I/O wrappers for yt-dlp, sqlite, SCP"
```

---

## Task 9: Inspect win-resolume DB schema and finalize `fetch_qwen_reference`

**Files:** `scripts/experiments/autosub_drift.py`

The Phase 1 schema for word-level alignment lives in the production DB. We must read the live schema before writing the query, per the spec's Risk 3.

- [ ] **Step 1: Pull a snapshot of the live DB**

Run on dev machine:
```bash
mkdir -p /tmp/autosub-drift-experiment
scp -q win-resolume:/c/ProgramData/SongPlayer/songplayer.db /tmp/autosub-drift-experiment/songplayer.db
```
Expected: file exists at `/tmp/autosub-drift-experiment/songplayer.db`

- [ ] **Step 2: Inspect the schema**

Run:
```bash
sqlite3 /tmp/autosub-drift-experiment/songplayer.db ".schema"
```

Look for tables holding word-level alignments — likely `lyrics_words` or similar based on Phase 1 work. Identify:
- The table name
- The column for video ID (`video_youtube_id`? `youtube_id`?)
- The column for word text (`text`? `word`?)
- The column for word start time in milliseconds (`start_ms`? `start`?)
- The order — is there a sequence column or is row order = stream order?

Write down findings before editing the script.

- [ ] **Step 3: Replace the placeholder query**

Edit `fetch_qwen_reference` in `autosub_drift.py`. Replace the `NotImplementedError` body with the actual query. Example shape (the table/column names depend on what Step 2 found — the example below uses placeholder names, the implementer MUST substitute the real ones):

```python
def fetch_qwen_reference(db_path: Path, video_id: str) -> Optional[List[Word]]:
    """Pull Qwen3 word-level alignment for a video from the local DB
    snapshot. Returns None if the video has no aligned word rows."""
    conn = sqlite3.connect(str(db_path))
    try:
        # SUBSTITUTE table + column names with what `.schema` revealed in Step 2.
        # Order MUST be the natural reading order of the song.
        cursor = conn.execute(
            "SELECT text, start_ms FROM lyrics_words "
            "WHERE video_youtube_id = ? "
            "ORDER BY line_index, word_offset",
            (video_id,),
        )
        rows = cursor.fetchall()
        if not rows:
            return None
        return [Word(text=text, start_ms=int(start_ms)) for text, start_ms in rows]
    finally:
        conn.close()
```

- [ ] **Step 4: Smoke-test the function in a Python REPL**

Run:
```bash
python -c "
from pathlib import Path
import sys
sys.path.insert(0, 'scripts/experiments')
from autosub_drift import fetch_qwen_reference
words = fetch_qwen_reference(Path('/tmp/autosub-drift-experiment/songplayer.db'), '<video_id_for_148>')
print(f'got {len(words)} words; first 5:')
for w in words[:5]:
    print(w)
"
```
Expected: prints a non-zero number of words with text + ms timestamps.

If output is `None` or zero rows, the schema query is wrong — go back to Step 2 and investigate.

- [ ] **Step 5: Commit**

```bash
git add scripts/experiments/autosub_drift.py
git commit -m "feat(experiment): finalize fetch_qwen_reference against live schema"
```

---

## Task 10: Wire up `main()` and run the experiment

**Files:** `scripts/experiments/autosub_drift.py`, `docs/experiments/2026-04-16-autosub-drift.md`

- [ ] **Step 1: Replace the stub `main()` with the full pipeline**

Edit `main()` in `autosub_drift.py`:

```python
# The three test songs from issue #29. Video IDs are the YouTube IDs
# stored in the production songplayer.db. Adjust at execution time if
# any have been re-encoded or replaced — record substitutions in the
# report's methodology section.
TEST_SONGS = [
    # (video_id, display title, display artist) — the title/artist are
    # used only for the report. Real values come from the DB if needed.
    ("REPLACE_WITH_148_VIDEO_ID", "Get This Party Started", "Planetshakers"),
    ("REPLACE_WITH_181_VIDEO_ID", "Song #181", "planetboom"),
    ("REPLACE_WITH_73_VIDEO_ID", "Song #73", "Elevation Worship"),
]

HISTOGRAM_BUCKETS = [-2000, -1000, -500, -300, -100, 0, 100, 300, 500, 1000, 2000]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to local copy of songplayer.db")
    parser.add_argument("--out", required=True, help="Markdown report output path")
    args = parser.parse_args()

    db_path = Path(args.db)
    out_path = Path(args.out)

    if not db_path.exists():
        sys.stderr.write(f"DB not found at {db_path}. Run pull_db_from_winresolume first.\n")
        return 2

    tmp_dir = Path(tempfile.mkdtemp(prefix="autosub_drift_"))
    print(f"working dir: {tmp_dir}", file=sys.stderr)

    results: List[SongResult] = []
    for video_id, title, artist in TEST_SONGS:
        print(f"[{video_id}] {title} — {artist}", file=sys.stderr)
        try:
            qwen = fetch_qwen_reference(db_path, video_id)
        except Exception as e:
            results.append(SongResult(video_id, title, artist, f"DB query failed: {e}", None, None, None))
            continue
        if not qwen:
            results.append(SongResult(video_id, title, artist, "no Qwen3 reference in DB", None, None, None))
            continue

        json3 = fetch_autosubs(video_id, tmp_dir)
        if not json3:
            results.append(SongResult(video_id, title, artist, "no auto-subs available", None, None, None))
            continue

        autosub = parse_json3(json3.read_text(encoding="utf-8"))
        match = match_word_streams(qwen, autosub, window_n=10)
        stats = compute_stats(match.drifts_ms)
        hist = make_histogram(match.drifts_ms, HISTOGRAM_BUCKETS)
        results.append(SongResult(video_id, title, artist, None, match, stats, hist))

    write_report(results, out_path)
    print(f"report written to {out_path}", file=sys.stderr)
    return 0
```

- [ ] **Step 2: Pull the live DB and identify the three video IDs**

Run on dev machine:
```bash
scp -q win-resolume:/c/ProgramData/SongPlayer/songplayer.db /tmp/autosub-drift-experiment/songplayer.db
sqlite3 /tmp/autosub-drift-experiment/songplayer.db \
  "SELECT youtube_id, song, artist FROM videos ORDER BY id;"
```

Find the rows for #148 (Planetshakers — Get This Party Started), #181 (planetboom), #73 (Elevation Worship). Substitute their `youtube_id` values into the `TEST_SONGS` list in `main()`.

If a song from the issue is not in the DB, pick the closest substitute and note it in Step 4's methodology section.

- [ ] **Step 3: Run the experiment**

Run:
```bash
python scripts/experiments/autosub_drift.py \
  --db /tmp/autosub-drift-experiment/songplayer.db \
  --out docs/experiments/2026-04-16-autosub-drift.md
```
Expected: `report written to docs/experiments/2026-04-16-autosub-drift.md`. Exit code 0.

If the script crashes on a real song, fix the bug, re-run. Do not commit a half-written report.

- [ ] **Step 4: Read the report end-to-end**

Open `docs/experiments/2026-04-16-autosub-drift.md` and verify:
- Methodology section is present
- Each test song has a results section (or an explicit "no data" block)
- Conclusion table has a row per song with bucket assignment
- Recommendation paragraph cites the worst-bucket song by name

If any song was substituted in Step 2, append a note in the Methodology section explaining the substitution.

- [ ] **Step 5: Commit the script changes + the report**

```bash
git add scripts/experiments/autosub_drift.py docs/experiments/2026-04-16-autosub-drift.md
git commit -m "feat(experiment): run drift experiment, commit Phase 2 report (#29)"
```

---

## Task 11: Push, monitor CI, open PR

**Files:** none (process steps)

- [ ] **Step 1: Verify dev is ahead of main**

Run:
```bash
git fetch origin
git log --oneline origin/main..HEAD
```
Expected: a list of the 10 commits from Tasks 1–10 above the merge base.

- [ ] **Step 2: Run `cargo fmt --all --check` to confirm we did not touch Rust**

Run:
```bash
cargo fmt --all --check
```
Expected: clean exit (no Rust changed).

- [ ] **Step 3: Push**

```bash
git push origin dev
```

- [ ] **Step 4: Monitor CI to terminal state**

Run:
```bash
gh run list --branch dev --limit 3
```

Pick the latest run id. Then:
```bash
sleep 600 && gh run view <run-id> --json status,conclusion,jobs
```
Expected: all jobs `success` (no Rust changes means no test regressions; no Python script touched in src means no lint changes).

If any job fails: `gh run view <run-id> --log-failed`, fix in one commit, push once, re-monitor.

- [ ] **Step 5: Open PR from dev to main**

Run:
```bash
gh pr create \
  --base main --head dev \
  --title "experiment(#29): Phase 2 auto-sub drift validation report" \
  --body "$(cat <<'EOF'
## Summary

Validation experiment for issue #29 (Phase 2 lyrics pipeline). Pulls
YT auto-subs + the production Qwen3 word timings for three songs,
computes per-song RMS drift, and writes a committed report deciding
whether Phase 2 (skip-Qwen3) is worth building.

Spec: docs/superpowers/specs/2026-04-16-phase2-autosub-drift-experiment-design.md
Plan: docs/superpowers/plans/2026-04-16-phase2-autosub-drift-experiment.md

## Recommendation

(Paste the recommendation paragraph from
docs/experiments/2026-04-16-autosub-drift.md here so reviewers see the
go/no-go without opening the file.)

## Test plan

- [ ] CI green
- [ ] Reviewer reads the report and agrees with bucket assignments
- [ ] If recommendation = greenlight: open Phase 2 design brainstorm
- [ ] If recommendation = refine: open Phase 2 design brainstorm with
      refinement pass scoped in
- [ ] If recommendation = kill: close issue #29 with the report linked

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 6: Verify PR is mergeable**

```bash
gh pr view --json mergeable,mergeStateStatus
```
Expected: `mergeable: true`, `mergeStateStatus: "CLEAN"`.

- [ ] **Step 7: Provide the PR URL to the user and wait for explicit merge instruction**

Per `pr-merge-policy`: never merge without the user explicitly saying so.

---

## Verification

After all tasks complete:

1. `python -m pytest scripts/experiments/test_autosub_drift.py -v` — 30 passed
2. `docs/experiments/2026-04-16-autosub-drift.md` exists with all required sections
3. `cargo fmt --all --check` — clean
4. CI green on the PR
5. PR is mergeable + clean
6. The recommendation paragraph in the PR body matches the report

The deliverable is the **report**. The script is the means; reviewers verify the script worked by reading the numbers it produced.
