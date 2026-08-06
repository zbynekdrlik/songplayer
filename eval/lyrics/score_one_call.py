#!/usr/bin/env python3
"""score_one_call.py — mechanical scorer for the one-call north-star sweep.

Scores one-call backend outputs (`gemini36-flash`, `aai-u35-translate`, both
producing `{lines: [{text, start_ms, end_ms, text_sk, words}]}` per
`eval/lyrics/backends/gemini36_flash.py` / `aai_u35_translate.py`) against
the pinned gold line timings in `manifest.json`.

This is a PURE ALGORITHMIC comparison — no LLM judge involved — and is
deliberately separate from the Claude-judge-based harness the rest of
`eval/lyrics/` uses (`judge_prompt.md`, `schemas/report.schema.json`,
`schemas/judgment.schema.json`). Those score 0-10 via a Claude judge
reading transcripts; this script computes objective timing/segmentation/
translation metrics directly from gold vs produced JSON, for the 2026-08-05
one-call north-star spike (gemini36-flash / aai-u35-translate). It does not
write anything conforming to report.schema.json and is not consumed by
that schema's tests.

Method limits (also called out in the generated report — restated here so
the number is never read without its caveat): gold_lines comes from
lrclib/spotify/yt_subs LINE-level sync and is itself imperfect (see
manifest.json's `gold_source` per fixture). Where the produced TEXT
diverges from gold TEXT (different wording, ASR errors, a skipped/added
line), the text-similarity gate below may fail to match a line at all, or
may match it to the wrong nearby gold line — so `n_matched` UNDER-COUNTS
correct timing on fixtures where the transcript itself is shaky, and the
resulting timing-delta stats are only as trustworthy as the underlying
text match. Coverage (gold matched / gold total) is reported alongside the
timing deltas for exactly this reason — read them together, never the
timing number alone.

Matching algorithm (per fixture): produced lines are walked in start_ms
order; for each, every NOT-YET-matched gold line is scored by
`difflib.SequenceMatcher.ratio()` on normalized text (lowercased,
punctuation/apostrophes stripped, whitespace collapsed). Any gold line
scoring >= RATIO_THRESHOLD (0.6) is an eligible candidate; among eligible
candidates the CLOSEST-START one is picked (not the highest-ratio one) —
this disambiguates repeated/chant lyrics ("Burn, burn, burn" x4 in
chant_repetition fixtures) by time proximity instead of arbitrarily
picking whichever repeat has a marginally higher text ratio. A matched
gold line is removed from the pool so no gold line is double-matched.

That matcher has NO monotonicity constraint, so a produced line can bind to
a gold line far earlier in the song than one already consumed. `monotonic_match`
is the same matcher with that one constraint added and is reported as a THIRD
view beside the official and conservative ones — `greedy_match` itself is never
changed, so every historical number in `reports/` stays comparable.

TWO DENOMINATORS, always reported together: `pct_within_400ms` is CONDITIONAL
on a line having been matched AND timed, so its denominator is whatever subset
that backend handled; `pct_gold_within_400ms` divides the same numerator by the
gold-line count, which is identical for every backend and is therefore the
comparable figure. `*_all_fixtures` variants additionally count the gold lines
of fixtures the backend produced no output for at all, so a silent crash never
outscores an honest all-untimed output.

Usage:
    python3 eval/lyrics/score_one_call.py \\
        --manifest eval/lyrics/manifest.json \\
        --raw-dir eval/lyrics/reports/2026-08-05-raw \\
        --backends gemini36-flash aai-u35-translate \\
        --out eval/lyrics/reports/2026-08-05-scores.json

Prints a compact aggregate summary to stdout (mean/median/p90 start delta,
%<=400ms, %<=1000ms, line-count ratio, >32-char %, sk_ok_pct — overall and
per category, per backend) and writes the full per-fixture + aggregate
detail to --out as JSON.
"""

from __future__ import annotations

import argparse
import difflib
import json
import logging
import math
import re
import statistics
from pathlib import Path
from typing import Any

logger = logging.getLogger("lyrics_eval.score_one_call")

RATIO_THRESHOLD = 0.6
WALL_TOLERANCE_MS = 400
LOOSE_TOLERANCE_MS = 1000
WALL_LINE_WIDTH_CHARS = 32

_PUNCT_RE = re.compile(r"[^\w\s]", re.UNICODE)
_WS_RE = re.compile(r"\s+")
_SK_DIACRITIC_RE = re.compile(r"[áäčďéíĺľňóôŕšťúýž]", re.IGNORECASE)

DEFAULT_BACKENDS = ["gemini36-flash", "aai-u35-translate"]


def normalize_text(s: str) -> str:
    s = s.lower()
    s = _PUNCT_RE.sub("", s)
    s = _WS_RE.sub(" ", s).strip()
    return s


def percentile(values: list[float], pct: float) -> float | None:
    """Linear-interpolation percentile (numpy's default method), stdlib-only."""
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * pct
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return s[int(k)]
    d0 = s[int(f)] * (c - k)
    d1 = s[int(c)] * (k - f)
    return d0 + d1


def load_manifest(path: Path) -> dict[str, dict[str, Any]]:
    """Returns {video_id: fixture_dict} keyed lookup."""
    data = json.loads(path.read_text(encoding="utf-8"))
    return {f["video_id"]: f for f in data["fixtures"]}


def load_produced(raw_dir: Path, backend: str, video_id: str) -> dict[str, Any] | None:
    path = raw_dir / f"{backend}_{video_id}.json"
    if not path.exists():
        logger.warning(
            "no output file for backend=%s video_id=%s (%s)", backend, video_id, path
        )
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        logger.warning(
            "failed to parse output for backend=%s video_id=%s: %s",
            backend,
            video_id,
            exc,
        )
        return None


def _match(
    produced_lines: list[dict[str, Any]],
    gold_lines: list[dict[str, Any]],
    *,
    monotonic: bool,
) -> list[dict[str, Any]]:
    """Shared body of `greedy_match` (monotonic=False — the OFFICIAL view,
    byte-for-byte the historical behaviour) and `monotonic_match`
    (monotonic=True — the third, order-respecting view)."""
    gold_available = list(range(len(gold_lines)))
    gold_norm = [normalize_text(g["text"]) for g in gold_lines]
    matches: list[dict[str, Any]] = []

    produced_order = sorted(
        range(len(produced_lines)), key=lambda i: produced_lines[i]["start_ms"]
    )
    last_gi = -1
    for pi in produced_order:
        p = produced_lines[pi]
        p_norm = normalize_text(p["text"])
        candidates: list[tuple[int, float]] = []
        for gi in gold_available:
            if monotonic and gi < last_gi:
                continue
            ratio = difflib.SequenceMatcher(None, p_norm, gold_norm[gi]).ratio()
            if ratio >= RATIO_THRESHOLD:
                candidates.append((gi, ratio))
        if not candidates:
            continue
        best_gi, best_ratio = min(
            candidates, key=lambda t: abs(p["start_ms"] - gold_lines[t[0]]["start_ms"])
        )
        delta = abs(p["start_ms"] - gold_lines[best_gi]["start_ms"])
        matches.append(
            {
                "produced_idx": pi,
                "gold_idx": best_gi,
                "abs_delta_ms": delta,
                "ratio": best_ratio,
            }
        )
        gold_available.remove(best_gi)
        last_gi = best_gi
    return matches


def greedy_match(
    produced_lines: list[dict[str, Any]], gold_lines: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Greedy, gold-line-consuming match. Returns a list of match records:
    {produced_idx, gold_idx, abs_delta_ms, ratio}. A gold line is matched
    to at most one produced line.

    UNCHANGED — this is the OFFICIAL view every historical number in
    `reports/` was computed with, and it stays comparable across sessions.
    Its known limitation (no ordering constraint, so a produced line can bind
    to a gold line far earlier in the song) is measured by the parallel
    `monotonic_match` view rather than silently patched here."""
    return _match(produced_lines, gold_lines, monotonic=False)


def monotonic_match(
    produced_lines: list[dict[str, Any]], gold_lines: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """THIRD scoring view — same matcher as `greedy_match` plus the one
    constraint it lacks: a produced line may only bind to a gold line at or
    after the highest gold index already consumed. Gold lines are
    chronologically ordered in every manifest fixture, so a backwards pair is
    a genuine mis-pairing, not legitimate reordering; ~27-28% of the official
    view's pairs are out of gold order and the resulting delta distortion
    differs per backend by several points, which is larger than the margins
    the shootout reports.

    Report this ALONGSIDE the official view, never instead of it: dropping an
    out-of-order pair removes a (usually large) delta from the numerator AND
    the denominator, so the conditional percentage rises for everyone — only
    the relative ordering and the gold-normalized twin are meaningful."""
    return _match(produced_lines, gold_lines, monotonic=True)


def score_fixture(
    *,
    backend: str,
    video_id: str,
    category: str,
    produced: dict[str, Any],
    gold_lines: list[dict[str, Any]],
) -> dict[str, Any]:
    produced_lines = produced.get("lines") or []
    n_produced = len(produced_lines)
    n_gold = len(gold_lines)

    matches = greedy_match(produced_lines, gold_lines)
    n_matched = len(matches)
    deltas = [m["abs_delta_ms"] for m in matches]
    ratios = [m["ratio"] for m in matches]

    within_400 = sum(1 for d in deltas if d <= WALL_TOLERANCE_MS)
    within_1000 = sum(1 for d in deltas if d <= LOOSE_TOLERANCE_MS)

    gt32_en = sum(
        1
        for line in produced_lines
        if len(line.get("text") or "") > WALL_LINE_WIDTH_CHARS
    )
    gt32_sk = sum(
        1
        for line in produced_lines
        if len(line.get("text_sk") or "") > WALL_LINE_WIDTH_CHARS
    )

    sk_ok = 0
    for line in produced_lines:
        text = line.get("text") or ""
        text_sk = line.get("text_sk") or ""
        if not text_sk:
            continue
        has_diacritic = bool(_SK_DIACRITIC_RE.search(text_sk))
        not_dup = (
            difflib.SequenceMatcher(None, text_sk.lower(), text.lower()).ratio() < 0.8
        )
        if has_diacritic and not_dup:
            sk_ok += 1

    words_present = [line for line in produced_lines if line.get("words")]
    has_word_timings = len(words_present) > 0
    word_count = (
        sum(len(line["words"]) for line in words_present) if has_word_timings else 0
    )

    return {
        "backend": backend,
        "video_id": video_id,
        "category": category,
        "n_produced": n_produced,
        "n_gold": n_gold,
        "line_count_ratio": (n_produced / n_gold) if n_gold else None,
        "n_matched": n_matched,
        "gold_coverage_pct": (n_matched / n_gold * 100.0) if n_gold else None,
        "produced_precision_pct": (n_matched / n_produced * 100.0)
        if n_produced
        else None,
        "mean_abs_delta_ms": round(statistics.fmean(deltas), 1) if deltas else None,
        "median_abs_delta_ms": round(statistics.median(deltas), 1) if deltas else None,
        "p90_abs_delta_ms": round(percentile(deltas, 0.9), 1) if deltas else None,
        "pct_within_400ms": round(within_400 / n_matched * 100.0, 1)
        if n_matched
        else None,
        "pct_within_1000ms": round(within_1000 / n_matched * 100.0, 1)
        if n_matched
        else None,
        "mean_match_text_ratio": round(statistics.fmean(ratios), 3) if ratios else None,
        "pct_gt32_chars_en": round(gt32_en / n_produced * 100.0, 1)
        if n_produced
        else None,
        "pct_gt32_chars_sk": round(gt32_sk / n_produced * 100.0, 1)
        if n_produced
        else None,
        "sk_ok_pct": round(sk_ok / n_produced * 100.0, 1) if n_produced else None,
        "has_word_timings": has_word_timings,
        "word_count": word_count if has_word_timings else None,
        "raw_deltas_ms": deltas,
        "error": None,
    }


def pooled_aggregate(fixture_scores: list[dict[str, Any]]) -> dict[str, Any]:
    """Pools raw matched-line deltas across ALL given fixtures (not a mean
    of per-fixture means) — statistically sounder when fixtures have very
    different matched-line counts."""
    ok_scores = [s for s in fixture_scores if s.get("error") is None]
    all_deltas: list[float] = []
    for s in ok_scores:
        all_deltas.extend(s["raw_deltas_ms"])

    total_produced = sum(s["n_produced"] for s in ok_scores)
    total_gold = sum(s["n_gold"] for s in ok_scores)
    total_matched = sum(s["n_matched"] for s in ok_scores)
    # ...and the HONEST denominator, which also counts the gold lines of
    # fixtures this backend produced no output for. Without it a backend that
    # crashes with no file scores strictly BETTER than one that honestly
    # writes an all-untimed output — the two conventions are both in use in
    # this harness (ctc writes a stub, mtl/elevenlabs die with no file), so
    # the two must be read side by side. Requires errored fixture records to
    # carry `n_gold`; a legacy record without it contributes 0, which is the
    # pre-fix behaviour rather than a crash.
    total_gold_all = sum(s.get("n_gold") or 0 for s in fixture_scores)
    total_sk_ok = sum(
        round((s["sk_ok_pct"] or 0.0) / 100.0 * s["n_produced"])
        for s in ok_scores
        if s["n_produced"]
    )
    total_gt32_en = sum(
        round((s["pct_gt32_chars_en"] or 0.0) / 100.0 * s["n_produced"])
        for s in ok_scores
        if s["n_produced"]
    )
    total_gt32_sk = sum(
        round((s["pct_gt32_chars_sk"] or 0.0) / 100.0 * s["n_produced"])
        for s in ok_scores
        if s["n_produced"]
    )
    n_with_words = sum(1 for s in ok_scores if s["has_word_timings"])

    within_400 = sum(1 for d in all_deltas if d <= WALL_TOLERANCE_MS)
    within_1000 = sum(1 for d in all_deltas if d <= LOOSE_TOLERANCE_MS)

    return {
        "n_fixtures": len(ok_scores),
        "n_fixtures_errored": len(fixture_scores) - len(ok_scores),
        "total_produced_lines": total_produced,
        "total_gold_lines": total_gold,
        "total_gold_lines_all_fixtures": total_gold_all,
        "total_matched_lines": total_matched,
        "line_count_ratio": round(total_produced / total_gold, 3)
        if total_gold
        else None,
        "gold_coverage_pct": round(total_matched / total_gold * 100.0, 1)
        if total_gold
        else None,
        "gold_coverage_pct_all_fixtures": round(
            total_matched / total_gold_all * 100.0, 1
        )
        if total_gold_all
        else None,
        "mean_abs_delta_ms": round(statistics.fmean(all_deltas), 1)
        if all_deltas
        else None,
        "median_abs_delta_ms": round(statistics.median(all_deltas), 1)
        if all_deltas
        else None,
        "p90_abs_delta_ms": round(percentile(all_deltas, 0.9), 1)
        if all_deltas
        else None,
        "pct_within_400ms": round(within_400 / len(all_deltas) * 100.0, 1)
        if all_deltas
        else None,
        "pct_within_1000ms": round(within_1000 / len(all_deltas) * 100.0, 1)
        if all_deltas
        else None,
        # Same numerator, GOLD denominator. `pct_within_400ms` above is
        # CONDITIONAL on a line having been both text-matched and timed, so
        # each backend is graded on the subset it happened to handle and the
        # denominator moves between backends (measured 1138 -> 1243 across the
        # 2026-08-05 shootout) — the worst backend gets the smallest
        # denominator. These two normalize every backend onto the same gold
        # line count and are the comparable figure; always print both, and
        # never quote the conditional one without saying what it is conditional
        # on.
        "pct_gold_within_400ms": round(within_400 / total_gold * 100.0, 1)
        if total_gold
        else None,
        "pct_gold_within_400ms_all_fixtures": round(
            within_400 / total_gold_all * 100.0, 1
        )
        if total_gold_all
        else None,
        "pct_gt32_chars_en": round(total_gt32_en / total_produced * 100.0, 1)
        if total_produced
        else None,
        "pct_gt32_chars_sk": round(total_gt32_sk / total_produced * 100.0, 1)
        if total_produced
        else None,
        "sk_ok_pct": round(total_sk_ok / total_produced * 100.0, 1)
        if total_produced
        else None,
        "fixtures_with_word_timings": n_with_words,
    }


def delta_aggregate(
    matches: list[dict[str, Any]], *, total_gold: int | None = None
) -> dict[str, Any]:
    """Pool a FLAT list of match records (anything carrying `abs_delta_ms`)
    into the standard delta stats. Used by the parallel scoring views
    (`monotonic_match`, `run_combine_experiment.conservative_match`) so every
    view reports the same fields as `pooled_aggregate`'s timing block.

    `total_gold` is the gold-line count the view was computed over; pass it so
    the gold-normalized %<=400ms is available for the view too (a view that
    drops pairs — as both parallel views do — otherwise reports a percentage
    over a denominator only it uses)."""
    deltas = [m["abs_delta_ms"] for m in matches]
    within_400 = sum(1 for d in deltas if d <= WALL_TOLERANCE_MS)
    within_1000 = sum(1 for d in deltas if d <= LOOSE_TOLERANCE_MS)
    return {
        "n_pairs": len(matches),
        "total_gold_lines": total_gold,
        "mean_abs_delta_ms": round(statistics.fmean(deltas), 1) if deltas else None,
        "median_abs_delta_ms": round(statistics.median(deltas), 1) if deltas else None,
        "p90_abs_delta_ms": round(percentile(deltas, 0.9), 1) if deltas else None,
        "pct_within_400ms": round(within_400 / len(deltas) * 100.0, 1)
        if deltas
        else None,
        "pct_within_1000ms": round(within_1000 / len(deltas) * 100.0, 1)
        if deltas
        else None,
        "pct_gold_within_400ms": round(within_400 / total_gold * 100.0, 1)
        if total_gold
        else None,
    }


def _rank_key(s: dict[str, Any]) -> tuple[float, float]:
    """Higher pct_within_400ms first, then lower median_abs_delta_ms — used
    to pick 'best'/'worst' fixtures. Fixtures with zero matched lines sort
    last regardless of direction (there's nothing to rank)."""
    pct = s.get("pct_within_400ms")
    median = s.get("median_abs_delta_ms")
    if pct is None or median is None:
        return (-1.0, float("inf"))
    return (pct, -median)


def rank_fixtures(
    fixture_scores: list[dict[str, Any]], n: int = 3
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Returns (best_n, worst_n) among fixtures that have at least one
    matched line (error=None and n_matched > 0) — ranked by pct_within_400ms
    desc, then median_abs_delta_ms asc."""
    rankable = [
        s for s in fixture_scores if s.get("error") is None and s.get("n_matched")
    ]
    ranked = sorted(rankable, key=_rank_key, reverse=True)
    best = ranked[:n]
    worst = list(reversed(ranked[-n:])) if len(ranked) >= n else list(reversed(ranked))
    return best, worst


def build_backend_report(
    backend: str, fixture_scores: list[dict[str, Any]]
) -> dict[str, Any]:
    by_category: dict[str, list[dict[str, Any]]] = {}
    for s in fixture_scores:
        by_category.setdefault(s["category"], []).append(s)

    best, worst = rank_fixtures(fixture_scores, n=3)

    return {
        "backend": backend,
        "aggregate": pooled_aggregate(fixture_scores),
        "by_category": {
            cat: pooled_aggregate(scores) for cat, scores in sorted(by_category.items())
        },
        "best_fixtures": [
            {
                "video_id": s["video_id"],
                "category": s["category"],
                "pct_within_400ms": s["pct_within_400ms"],
                "median_abs_delta_ms": s["median_abs_delta_ms"],
                "gold_coverage_pct": s["gold_coverage_pct"],
            }
            for s in best
        ],
        "worst_fixtures": [
            {
                "video_id": s["video_id"],
                "category": s["category"],
                "pct_within_400ms": s["pct_within_400ms"],
                "median_abs_delta_ms": s["median_abs_delta_ms"],
                "gold_coverage_pct": s["gold_coverage_pct"],
            }
            for s in worst
        ],
        "per_fixture": fixture_scores,
    }


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(levelname)s %(name)s: %(message)s"
    )
    root = Path(__file__).resolve().parents[2]

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--manifest", type=Path, default=root / "eval" / "lyrics" / "manifest.json"
    )
    p.add_argument(
        "--raw-dir",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-raw",
    )
    p.add_argument("--backends", nargs="+", default=DEFAULT_BACKENDS)
    p.add_argument(
        "--out",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-scores.json",
    )
    args = p.parse_args(argv)

    fixtures_by_id = load_manifest(args.manifest)
    logger.info("manifest loaded: %d fixtures", len(fixtures_by_id))

    reports: dict[str, Any] = {}
    for backend in args.backends:
        fixture_scores: list[dict[str, Any]] = []
        for video_id, fixture in fixtures_by_id.items():
            produced = load_produced(args.raw_dir, backend, video_id)
            if produced is None:
                fixture_scores.append(
                    {
                        "backend": backend,
                        "video_id": video_id,
                        "category": fixture["category"],
                        # kept so the fixture still counts toward the honest
                        # gold denominator (pooled_aggregate's *_all_fixtures)
                        "n_gold": len(fixture["gold_lines"]),
                        "error": "output file missing or unparseable",
                    }
                )
                continue
            fixture_scores.append(
                score_fixture(
                    backend=backend,
                    video_id=video_id,
                    category=fixture["category"],
                    produced=produced,
                    gold_lines=fixture["gold_lines"],
                )
            )
        reports[backend] = build_backend_report(backend, fixture_scores)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(reports, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logger.info("wrote %s", args.out)

    # Compact stdout summary — this is what the orchestrating agent reads,
    # so it never needs to open the (much larger) JSON to see the numbers.
    print(f"\n=== score_one_call summary ({args.out}) ===\n")
    for backend, report in reports.items():
        agg = report["aggregate"]
        print(f"--- {backend} ---")
        print(
            f"  fixtures: {agg['n_fixtures']} ok, {agg['n_fixtures_errored']} errored "
            f"(no output)"
        )
        print(
            f"  matched lines: {agg['total_matched_lines']}/{agg['total_gold_lines']} gold "
            f"({agg['gold_coverage_pct']}% coverage; "
            f"{agg['gold_coverage_pct_all_fixtures']}% over all "
            f"{agg['total_gold_lines_all_fixtures']} gold lines incl. errored fixtures)"
        )
        print(
            f"  start delta (ms): mean={agg['mean_abs_delta_ms']} "
            f"median={agg['median_abs_delta_ms']} p90={agg['p90_abs_delta_ms']}"
        )
        print(
            f"  within 400ms: {agg['pct_within_400ms']}% of MATCHED+TIMED lines   "
            f"within 1000ms: {agg['pct_within_1000ms']}%"
        )
        print(
            f"  within 400ms (gold-normalized, the comparable figure): "
            f"{agg['pct_gold_within_400ms']}% of scored gold lines   "
            f"{agg['pct_gold_within_400ms_all_fixtures']}% of ALL gold lines"
        )
        print(f"  line_count_ratio (produced/gold): {agg['line_count_ratio']}")
        print(
            f"  >32 chars: en={agg['pct_gt32_chars_en']}%  sk={agg['pct_gt32_chars_sk']}%"
        )
        print(f"  sk_ok_pct: {agg['sk_ok_pct']}%")
        print(
            f"  fixtures with word timings: {agg['fixtures_with_word_timings']}/{agg['n_fixtures']}"
        )
        print("  by category:")
        for cat, cagg in report["by_category"].items():
            print(
                f"    {cat:22s} n={cagg['n_fixtures']:>2} coverage={cagg['gold_coverage_pct']!s:>6}% "
                f"median_delta={cagg['median_abs_delta_ms']!s:>7}ms within400={cagg['pct_within_400ms']!s:>6}% "
                f"sk_ok={cagg['sk_ok_pct']!s:>6}%"
            )
        print("  best 3 fixtures (by within-400ms rate, then median delta):")
        for s in report["best_fixtures"]:
            print(
                f"    {s['video_id']} ({s['category']}): within400={s['pct_within_400ms']}% "
                f"median_delta={s['median_abs_delta_ms']}ms coverage={s['gold_coverage_pct']}%"
            )
        print("  worst 3 fixtures:")
        for s in report["worst_fixtures"]:
            print(
                f"    {s['video_id']} ({s['category']}): within400={s['pct_within_400ms']}% "
                f"median_delta={s['median_abs_delta_ms']}ms coverage={s['gold_coverage_pct']}%"
            )
        print()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
