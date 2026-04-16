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
import os
import re
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

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
    last_bin = len(buckets) - 2
    for d in drifts_ms:
        for i in range(len(buckets) - 1):
            lo = buckets[i]
            hi = buckets[i + 1]
            # Last bin is closed on both ends so a drift equal to the
            # max bucket edge is counted, not silently dropped. All
            # other bins remain half-open [lo, hi).
            if i == last_bin:
                if lo <= d <= hi:
                    counts[i] += 1
                    break
            else:
                if lo <= d < hi:
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
    attempted = r.match.matched + r.match.skipped
    body = [
        header,
        "",
        f"- URL: {url}",
        f"- Match rate: **{r.match.matched}/{attempted}** Qwen3 words"
        f" attempted ({r.match.matched / max(attempted, 1):.1%}),"
        f" {r.match.skipped} skipped (no auto-sub counterpart in window)",
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

    parts.append("## Raw data references")
    parts.append("")
    parts.append(
        "Auto-sub json3 files are pulled into a per-run tmp dir created by"
        " `tempfile.mkdtemp(prefix=\"autosub_drift_\")` and are NOT committed"
        " to the repo. Re-run the script (see header docstring) to regenerate"
        " them. The Qwen3 reference word timings are pulled from a read-only"
        " SCP snapshot of the production `songplayer.db`; that snapshot is"
        " also not committed."
    )
    parts.append("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(parts), encoding="utf-8")


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
    success, or None if the video simply has no auto-subs (yt-dlp
    completed cleanly but produced no `<id>.en.json3` file).

    A real yt-dlp failure (network error, banned video, malformed args)
    raises subprocess.CalledProcessError unmodified — `main()` lets it
    propagate so the script exits non-zero per the spec failure table.
    """
    out_template = tmp_dir / f"{video_id}.%(ext)s"
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
    candidate = tmp_dir / f"{video_id}.en.json3"
    if candidate.exists():
        return candidate
    # When a video has BOTH manual and auto-generated English captions,
    # yt-dlp writes the auto-sub variant as <id>.en-orig.json3 to keep
    # it distinct. Our test corpus is selected for having manual subs,
    # so this fallback is the common case, not the edge.
    orig = tmp_dir / f"{video_id}.en-orig.json3"
    return orig if orig.exists() else None


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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to local copy of songplayer.db")
    parser.add_argument("--out", required=True, help="Markdown report output path")
    parser.parse_args()
    print("not implemented yet", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
