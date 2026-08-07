#!/usr/bin/env python3
"""score_aligner.py — scorer for forced-aligner backend outputs (the
2026-08-05 aligner shootout: `ctc-forced-aligner`, `lyrics-alignment-mtl`).

A forced aligner receives FIXED reference text (an audio-LLM's already-
produced `lines[].text`/`text_sk`) plus the isolated-vocal WAV, and returns
per-word timestamps for that exact text — it does not transcribe or
reinterpret. Its raw output is committed in the SAME per-fixture shape every
other backend in this harness uses (`{backend_id, backend_revision, wav_path,
duration_ms, lines: [{text, start_ms, end_ms, text_sk, words}], metadata}`,
see `eval/lyrics/backends/soniox_v5.py` for the established convention) —
the one difference is that a line the aligner could not time at all
(zero words matched) carries `start_ms/end_ms: null` rather than a guessed
value, exactly like `combine_lines_times.py`'s UNTIMED convention.

This module is deliberately thin glue over the TWO existing scorers rather
than a third metric:
  - `score_one_call.score_fixture` / `build_backend_report` (imported,
    NEVER modified) — the OFFICIAL view: greedy closest-start text match,
    ratio >= 0.6, gold-line-consuming.
  - `run_combine_experiment.conservative_match` / `conservative_aggregate`
    (imported, never modified) — the CONSERVATIVE view: a produced line
    only pairs to a gold line whose normalized text is UNIQUE in the song
    (ratio >= 0.75) — no repeat-disambiguation needed or used, since there
    is only ever one candidate.

Both views need every UNTIMED line filtered out first (score_one_call's
matcher sorts by `start_ms` and cannot handle `None`) — `to_scoreable_lines`
below does exactly what `run_combine_experiment.to_scoreable_lines` does for
combiner output, applied here to raw aligner output instead.

**The poisoned fixture** (`Xvm4_fWkXe8` — qwen35-omni hallucinated a
degenerate 161-copy repetition loop on this song, see project docs) is
ALWAYS excluded from the pooled/per-category aggregates so one fixture's
pathology cannot dominate a 22-fixture picture, and is ALWAYS scored and
reported on its own under `poisoned_fixture` in the output — visible, never
silently dropped.

Usage:
    python3 eval/lyrics/score_aligner.py \\
        --manifest eval/lyrics/manifest.json \\
        --raw-dir eval/lyrics/reports/2026-08-05-aligner-raw \\
        --backends ctc-forced-aligner ctc-forced-aligner-star lyrics-alignment-mtl \\
        --out eval/lyrics/reports/2026-08-05-aligner-scores.json
"""

from __future__ import annotations

import argparse
import json
import logging
import statistics
from pathlib import Path
from typing import Any

from eval.lyrics import score_one_call
from eval.lyrics.run_combine_experiment import (
    conservative_aggregate,
    conservative_match,
    total_gold_of,
)
from eval.lyrics.score_one_call import POISONED_FIXTURE_VIDEO_ID

logger = logging.getLogger("lyrics_eval.score_aligner")

# A backend that never recorded `metadata.device` (e.g. elevenlabs-fa, the
# ctc-* rows) must not collapse into the string "None" — that reads as a
# real device to any consumer doing `by_device["cuda"]` or rendering a chart
# legend. An explicit sentinel says plainly that the value was never
# recorded, as opposed to genuinely being device=None.
UNRECORDED_DEVICE = "unrecorded"


def _device_bucket(score: dict[str, Any]) -> str:
    device = score.get("device")
    return device if device is not None else UNRECORDED_DEVICE


def to_scoreable_lines(
    produced_lines: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], int]:
    """Filter out UNTIMED lines (start_ms is None — the aligner found zero
    alignable words for that line). Mirrors
    `run_combine_experiment.to_scoreable_lines`, applied to a raw aligner
    backend's own `lines[]` instead of a combiner's output. Returns
    (scoreable_lines, n_untimed)."""
    scoreable = [line for line in produced_lines if line.get("start_ms") is not None]
    n_untimed = len(produced_lines) - len(scoreable)
    return scoreable, n_untimed


def score_one_fixture(
    *,
    backend: str,
    video_id: str,
    category: str,
    produced: dict[str, Any] | None,
    gold_lines: list[dict[str, Any]],
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Returns (official_score_dict, conservative_match_list) for one
    fixture. official_score_dict has the same shape as
    `score_one_call.score_fixture` output, plus untimed-line bookkeeping."""
    if produced is None:
        return (
            {
                "backend": backend,
                "video_id": video_id,
                "category": category,
                # An errored fixture MUST still carry its gold-line count.
                # Without it, `pooled_aggregate`'s denominator silently shrinks
                # for whichever backend crashed, so a backend that dies with no
                # output file scores strictly BETTER than one that honestly
                # writes an all-untimed output — and this shootout has both
                # conventions in play (ctc writes a stub; mtl/elevenlabs write
                # nothing). Measured counterfactual: had ctc crashed silently on
                # p74PDWAFk0A instead of writing its honest all-untimed file,
                # its coverage would read 72.8% instead of 66.9% — above MTL.
                "n_gold": len(gold_lines),
                "error": "output file missing or unparseable",
            },
            [],
        )

    produced_lines = produced.get("lines") or []
    scoreable, n_untimed = to_scoreable_lines(produced_lines)

    score = score_one_call.score_fixture(
        backend=backend,
        video_id=video_id,
        category=category,
        produced={"lines": scoreable},
        gold_lines=gold_lines,
    )
    score["n_lines_untimed"] = n_untimed
    score["n_lines_total"] = len(produced_lines)
    score["untimed_pct"] = (
        round(n_untimed / len(produced_lines) * 100.0, 1) if produced_lines else None
    )
    metadata = produced.get("metadata") or {}
    score["runtime_sec"] = metadata.get("runtime_sec")
    # A fixture that OOM'd on the GPU and was retried on CPU costs ~5x the
    # wall-clock of a GPU run; pooling both into one mean attributes a
    # CPU-fallback penalty to the MODEL. Carry the provenance through so the
    # runtime rollup can split by device (lyrics_alignment_mtl/run.py records
    # both fields; backends that don't simply report None).
    score["device"] = metadata.get("device")
    score["cuda_oom_retried"] = metadata.get("cuda_oom_retried")

    cons_matches = conservative_match(scoreable, gold_lines)
    for m in cons_matches:
        m["video_id"] = video_id
        m["category"] = category

    return score, cons_matches


def score_backend(
    backend: str,
    manifest: dict[str, dict[str, Any]],
    raw_dir: Path,
) -> dict[str, Any]:
    fixture_scores: list[dict[str, Any]] = []
    conservative_matches: list[dict[str, Any]] = []
    monotonic_matches: list[dict[str, Any]] = []
    poisoned_score: dict[str, Any] | None = None
    poisoned_conservative: list[dict[str, Any]] = []

    for video_id, fixture in manifest.items():
        produced = score_one_call.load_produced(raw_dir, backend, video_id)
        score, cons = score_one_fixture(
            backend=backend,
            video_id=video_id,
            category=fixture["category"],
            produced=produced,
            gold_lines=fixture["gold_lines"],
        )
        # THIRD view — same matcher as the official one plus the monotonicity
        # constraint it lacks (see score_one_call.monotonic_match). Computed
        # here rather than inside score_one_fixture so that function's return
        # signature (and every caller/test of it) stays unchanged.
        scoreable, _ = to_scoreable_lines((produced or {}).get("lines") or [])
        mono = score_one_call.monotonic_match(scoreable, fixture["gold_lines"])
        for m in mono:
            m["video_id"] = video_id
            m["category"] = fixture["category"]

        if video_id == POISONED_FIXTURE_VIDEO_ID:
            poisoned_score = score
            poisoned_conservative = cons
            continue  # excluded from every pooled/per-category aggregate
        fixture_scores.append(score)
        conservative_matches.extend(cons)
        monotonic_matches.extend(mono)

    report = score_one_call.build_backend_report(backend, fixture_scores)
    # matches pooled_aggregate's total_gold_lines
    total_gold = total_gold_of(fixture_scores)
    categories = sorted({f["category"] for f in manifest.values()})
    # Per-category gold total — WITHOUT this, every per-category cell below
    # ships `pct_gold_within_400ms: null` even though the official view's
    # per-category cells (via pooled_aggregate) carry real values, and a
    # report author quoting a per-category number has no gold-normalized
    # figure available at all.
    gold_by_category = {
        cat: total_gold_of([s for s in fixture_scores if s["category"] == cat])
        for cat in categories
    }
    report["conservative"] = {
        "aggregate": conservative_aggregate(conservative_matches, total_gold),
        "by_category": {
            cat: conservative_aggregate(
                [m for m in conservative_matches if m["category"] == cat],
                gold_by_category[cat],
            )
            for cat in categories
        },
    }
    report["monotonic"] = {
        "aggregate": score_one_call.delta_aggregate(
            monotonic_matches, total_gold=total_gold
        ),
        "by_category": {
            cat: score_one_call.delta_aggregate(
                [m for m in monotonic_matches if m["category"] == cat],
                total_gold=gold_by_category[cat],
            )
            for cat in categories
        },
    }

    # Untimed-line rollup — pooled across the (non-poisoned) fixtures this
    # backend actually produced output for.
    ok_scores = [s for s in fixture_scores if s.get("error") is None]
    total_lines = sum(s["n_lines_total"] for s in ok_scores)
    total_untimed = sum(s["n_lines_untimed"] for s in ok_scores)
    report["untimed"] = {
        "total_lines": total_lines,
        "total_untimed": total_untimed,
        "untimed_pct": round(total_untimed / total_lines * 100.0, 1)
        if total_lines
        else None,
    }

    timed_scores = [s for s in ok_scores if s.get("runtime_sec") is not None]
    runtimes = [s["runtime_sec"] for s in timed_scores]
    # Split by device: a fixture that OOM'd on the GPU and was retried on CPU
    # runs ~5x slower, so blending it into one mean attributes a CPU-fallback
    # penalty to the model. `mean_runtime_sec` below keeps the blended figure
    # for continuity with earlier reports — quote the per-device one.
    by_device: dict[str, dict[str, Any]] = {}
    for device in sorted({_device_bucket(s) for s in timed_scores}):
        vals = [s["runtime_sec"] for s in timed_scores if _device_bucket(s) == device]
        by_device[device] = {
            "n_fixtures": len(vals),
            "mean_runtime_sec": round(statistics.fmean(vals), 1),
            "median_runtime_sec": round(statistics.median(vals), 1),
            "total_runtime_sec": round(sum(vals), 1),
        }
    report["runtime"] = {
        "n_fixtures_with_runtime": len(runtimes),
        "mean_runtime_sec": round(statistics.fmean(runtimes), 1) if runtimes else None,
        "median_runtime_sec": round(statistics.median(runtimes), 1)
        if runtimes
        else None,
        "total_runtime_sec": round(sum(runtimes), 1) if runtimes else None,
        "n_cuda_oom_retried": sum(1 for s in ok_scores if s.get("cuda_oom_retried")),
        "by_device": by_device,
    }

    if poisoned_score is not None:
        poisoned_untimed = poisoned_score.get("n_lines_untimed")
        poisoned_total = poisoned_score.get("n_lines_total")
        report["poisoned_fixture"] = {
            "video_id": POISONED_FIXTURE_VIDEO_ID,
            "official": poisoned_score,
            # Gold-normalize against the poisoned fixture's OWN gold count.
            # Its `official` and `monotonic` siblings already carry a real
            # `pct_gold_within_400ms`; leaving only this view null invited
            # exactly the bare-conditional quote the rule forbids.
            "conservative": conservative_aggregate(
                poisoned_conservative, poisoned_score.get("n_gold")
            ),
            "untimed_pct": round(poisoned_untimed / poisoned_total * 100.0, 1)
            if poisoned_total
            else None,
        }
    else:
        report["poisoned_fixture"] = {
            "video_id": POISONED_FIXTURE_VIDEO_ID,
            "error": "no output found for poisoned fixture",
        }

    return report


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
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-aligner-raw",
    )
    p.add_argument("--backends", nargs="+", required=True)
    p.add_argument(
        "--out",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-aligner-scores.json",
    )
    args = p.parse_args(argv)

    manifest = score_one_call.load_manifest(args.manifest)
    logger.info("manifest loaded: %d fixtures", len(manifest))

    results: dict[str, Any] = {}
    for backend in args.backends:
        results[backend] = score_backend(backend, manifest, args.raw_dir)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(results, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logger.info("wrote %s", args.out)

    print(f"\n=== aligner scoring summary ({args.out}) ===\n")
    for backend, report in results.items():
        agg = report["aggregate"]
        cons = report["conservative"]["aggregate"]
        mono = report["monotonic"]["aggregate"]
        untimed = report["untimed"]
        runtime = report["runtime"]
        print(
            f"--- {backend} ({agg['n_fixtures']} fixtures scored, "
            f"{agg['n_fixtures_errored']} errored, poisoned fixture excluded) ---"
        )
        print(
            f"  gold lines   : {agg['total_gold_lines']} in scored fixtures / "
            f"{agg['total_gold_lines_all_fixtures']} incl. errored fixtures"
        )
        print(
            f"  official     : within400={agg['pct_within_400ms']!s:>6}% of matched  "
            f"gold_within400={agg['pct_gold_within_400ms']!s:>6}%  "
            f"median_delta={agg['median_abs_delta_ms']!s:>8}ms  "
            f"p90={agg['p90_abs_delta_ms']!s:>8}ms  "
            f"coverage={agg['gold_coverage_pct']!s:>6}% "
            f"({agg['gold_coverage_pct_all_fixtures']}% of all gold)"
        )
        print(
            f"  conservative : within400={cons['pct_within_400ms']!s:>6}%  "
            f"gold_within400={cons['pct_gold_within_400ms']!s:>6}%  "
            f"median_delta={cons['median_abs_delta_ms']!s:>8}ms  n_pairs={cons['n_pairs']}"
        )
        print(
            f"  monotonic    : within400={mono['pct_within_400ms']!s:>6}%  "
            f"gold_within400={mono['pct_gold_within_400ms']!s:>6}%  "
            f"median_delta={mono['median_abs_delta_ms']!s:>8}ms  n_pairs={mono['n_pairs']}"
        )
        print(
            f"  untimed      : {untimed['total_untimed']}/{untimed['total_lines']} "
            f"({untimed['untimed_pct']}%)"
        )
        print(
            f"  runtime      : mean={runtime['mean_runtime_sec']}s "
            f"median={runtime['median_runtime_sec']}s "
            f"(n={runtime['n_fixtures_with_runtime']}, "
            f"cuda_oom_retried={runtime['n_cuda_oom_retried']})"
        )
        for device, stats in runtime["by_device"].items():
            print(
                f"    device {device:8s}: n={stats['n_fixtures']:>2} "
                f"mean={stats['mean_runtime_sec']}s "
                f"median={stats['median_runtime_sec']}s"
            )
        print("  by category (official):")
        for cat, cagg in report["by_category"].items():
            print(
                f"    {cat:22s} n={cagg['n_fixtures']:>2} coverage={cagg['gold_coverage_pct']!s:>6}% "
                f"median_delta={cagg['median_abs_delta_ms']!s:>7}ms within400={cagg['pct_within_400ms']!s:>6}%"
            )
        pf = report["poisoned_fixture"]
        if "error" in pf:
            print(f"  poisoned fixture ({POISONED_FIXTURE_VIDEO_ID}): {pf['error']}")
        else:
            print(
                f"  poisoned fixture ({POISONED_FIXTURE_VIDEO_ID}): "
                f"untimed={pf['untimed_pct']}%  "
                f"official_within400={pf['official'].get('pct_within_400ms')}%  "
                f"conservative_within400={pf['conservative'].get('pct_within_400ms')}%"
            )
        print()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
