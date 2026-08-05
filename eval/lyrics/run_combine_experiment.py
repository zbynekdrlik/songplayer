#!/usr/bin/env python3
"""run_combine_experiment.py — orchestrates the 2026-08-05 offline experiment:
does pairing an audio-LLM's LINE content with a dedicated-ASR backend's
per-word TIMESTAMPS clear the 400ms wall-timing gate that neither backend
clears alone (`reports/2026-08-05-one-call-northstar.md`)?

Runs `combine_lines_times.combine_lines_times()` for every (line_backend,
time_backend) pair in COMBOS across every fixture in the manifest, scores
each combined output with the EXISTING `score_one_call.py` matcher
(imported, never modified) plus a stricter "conservative" parallel metric
defined here, and computes word-alignment-rate statistics (overall, and
split by whether the line's text repeats elsewhere in the same song) plus a
sample of concrete LLM-word-vs-ASR-word mismatches.

Also re-scores each backend's own untouched `lines[]` as a baseline
(`BASELINE_BACKENDS`), so the combo numbers can be read against "how did
each ingredient perform alone".

Outputs:
  - `reports/2026-08-05-combine-raw/<combo-id>_<video_id>.json` — the
    per-fixture combined lines + combine_lines_times' own stats, for every
    combo (NOT for baselines — baselines are scored straight from the
    already-committed `reports/2026-08-05-raw/` files, untouched).
  - `reports/2026-08-05-combine-scores.json` — full detail: baselines +
    combos, each with the score_one_call.py aggregate, the conservative
    aggregate, word-alignment stats, and a sample of mismatch examples.
  - stdout: compact summary.

Usage:
    python3 eval/lyrics/run_combine_experiment.py
"""

from __future__ import annotations

import argparse
import difflib
import json
import logging
import statistics
from collections import Counter
from pathlib import Path
from typing import Any

from eval.lyrics import combine_lines_times as clt
from eval.lyrics import score_one_call

logger = logging.getLogger("lyrics_eval.run_combine_experiment")

# Stricter than score_one_call.RATIO_THRESHOLD (0.6): requires the GOLD line
# to be unique in the song (eliminates repeat-confusion by construction,
# rather than disambiguating it by closest-start time) and a higher text
# similarity. See conservative_match()'s docstring.
CONSERVATIVE_RATIO_THRESHOLD = 0.75

BASELINE_BACKENDS = [
    "gemini36-flash",
    "aai-u35-translate",
    "soniox-v5",
    "qwen35-omni",
]
COMBOS: list[tuple[str, str]] = [
    ("qwen35-omni", "soniox-v5"),
    ("qwen35-omni", "aai-u35-translate"),
    ("gemini36-flash", "soniox-v5"),
]


def combo_id(line_backend: str, time_backend: str) -> str:
    return f"combo-{line_backend}-lines_{time_backend}-times"


# ---------------------------------------------------------------------------
# Conservative parallel metric (score_one_call.py's greedy_match is known to
# have no monotonicity constraint and can pick the WRONG repeat of a phrase
# on chant/chorus material — reports/2026-08-05-one-call-northstar.md. This
# metric sidesteps that entirely by only scoring gold lines whose text is
# UNIQUE in the song, so there is never more than one candidate occurrence.)
# ---------------------------------------------------------------------------


def unique_gold_lines(gold_lines: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Gold lines whose normalized text occurs exactly once in the fixture."""
    norm_counts = Counter(score_one_call.normalize_text(g["text"]) for g in gold_lines)
    return [
        g
        for g in gold_lines
        if norm_counts[score_one_call.normalize_text(g["text"])] == 1
    ]


def conservative_match(
    produced_lines: list[dict[str, Any]], gold_lines: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Pair a produced (TIMED) line to a gold line only when: (a) the gold
    line's normalized text is unique in the song, and (b) text similarity
    to the produced line is >= CONSERVATIVE_RATIO_THRESHOLD. Highest-ratio
    candidate wins; each gold line matched at most once. No closest-start
    disambiguation is needed (unlike score_one_call.greedy_match) because
    uniqueness already rules out having more than one real candidate."""
    uniq_gold = unique_gold_lines(gold_lines)
    gold_norm = [score_one_call.normalize_text(g["text"]) for g in uniq_gold]
    gold_available = list(range(len(uniq_gold)))

    timed_produced = [p for p in produced_lines if p.get("start_ms") is not None]
    timed_produced.sort(key=lambda p: p["start_ms"])

    matches: list[dict[str, Any]] = []
    for p in timed_produced:
        p_norm = score_one_call.normalize_text(p["text"])
        candidates: list[tuple[int, float]] = []
        for gi in gold_available:
            ratio = difflib.SequenceMatcher(None, p_norm, gold_norm[gi]).ratio()
            if ratio >= CONSERVATIVE_RATIO_THRESHOLD:
                candidates.append((gi, ratio))
        if not candidates:
            continue
        best_gi, best_ratio = max(candidates, key=lambda t: t[1])
        delta = abs(p["start_ms"] - uniq_gold[best_gi]["start_ms"])
        matches.append(
            {
                "gold_idx": best_gi,
                "abs_delta_ms": delta,
                "ratio": round(best_ratio, 3),
                "gold_text": uniq_gold[best_gi]["text"],
                "produced_text": p["text"],
            }
        )
        gold_available.remove(best_gi)
    return matches


def conservative_aggregate(matches: list[dict[str, Any]]) -> dict[str, Any]:
    deltas = [m["abs_delta_ms"] for m in matches]
    within_400 = sum(1 for d in deltas if d <= score_one_call.WALL_TOLERANCE_MS)
    within_1000 = sum(1 for d in deltas if d <= score_one_call.LOOSE_TOLERANCE_MS)
    return {
        "n_pairs": len(matches),
        "median_abs_delta_ms": round(statistics.median(deltas), 1) if deltas else None,
        "mean_abs_delta_ms": round(statistics.fmean(deltas), 1) if deltas else None,
        "pct_within_400ms": round(within_400 / len(deltas) * 100.0, 1)
        if deltas
        else None,
        "pct_within_1000ms": round(within_1000 / len(deltas) * 100.0, 1)
        if deltas
        else None,
    }


# ---------------------------------------------------------------------------
# Combo runner
# ---------------------------------------------------------------------------


def to_scoreable_lines(
    combined_lines: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], int]:
    """score_one_call.py's matcher sorts produced lines by start_ms and
    cannot handle None (an UNTIMED combined line) — filter those out here
    and report how many were excluded. This is a scoring-input decision,
    not a data-loss one: the full combined output (including UNTIMED
    lines) is still written to the per-fixture combine-raw JSON file."""
    scoreable = [
        {
            "text": line["text"],
            "start_ms": line["start_ms"],
            "end_ms": line["end_ms"],
            "text_sk": line["text_sk"],
        }
        for line in combined_lines
        if line["timed"]
    ]
    n_untimed = sum(1 for line in combined_lines if not line["timed"])
    return scoreable, n_untimed


def normalized_line_text_counts(lines: list[dict[str, Any]]) -> Counter:
    return Counter(
        score_one_call.normalize_text(line.get("text") or "") for line in lines
    )


def run_combo(
    *,
    line_backend: str,
    time_backend: str,
    manifest: dict[str, dict[str, Any]],
    raw_dir: Path,
    combine_raw_dir: Path,
) -> dict[str, Any]:
    cid = combo_id(line_backend, time_backend)
    fixture_scores: list[dict[str, Any]] = []
    conservative_matches: list[dict[str, Any]] = []
    align_rate_records: list[dict[str, Any]] = []
    mismatch_examples: list[dict[str, Any]] = []
    n_lines_start_past_audio_end = 0

    for video_id, fixture in manifest.items():
        try:
            line_output = clt.load_backend_output(raw_dir, line_backend, video_id)
            time_output = clt.load_backend_output(raw_dir, time_backend, video_id)
        except FileNotFoundError as exc:
            logger.warning("skipping %s for %s: %s", video_id, cid, exc)
            fixture_scores.append(
                {
                    "backend": cid,
                    "video_id": video_id,
                    "category": fixture["category"],
                    "error": str(exc),
                }
            )
            continue

        line_source_lines = line_output.get("lines") or []
        time_source_lines = time_output.get("lines") or []
        combined = clt.combine_lines_times(line_source_lines, time_source_lines)

        out_path = combine_raw_dir / f"{cid}_{video_id}.json"
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(
            json.dumps(
                {
                    "backend_id": cid,
                    "line_backend": line_backend,
                    "time_backend": time_backend,
                    "video_id": video_id,
                    "duration_ms": time_output.get("duration_ms"),
                    "lines": combined["lines"],
                    "combine_stats": combined["stats"],
                },
                indent=2,
                ensure_ascii=False,
            )
            + "\n",
            encoding="utf-8",
        )

        scoreable, n_untimed = to_scoreable_lines(combined["lines"])
        score = score_one_call.score_fixture(
            backend=cid,
            video_id=video_id,
            category=fixture["category"],
            produced={"lines": scoreable},
            gold_lines=fixture["gold_lines"],
        )
        score["n_lines_untimed_excluded"] = n_untimed
        score["n_lines_including_untimed"] = len(combined["lines"])
        score["untimed_pct"] = (
            round(n_untimed / len(combined["lines"]) * 100.0, 1)
            if combined["lines"]
            else None
        )
        fixture_scores.append(score)

        cons_matches = conservative_match(scoreable, fixture["gold_lines"])
        for m in cons_matches:
            m["video_id"] = video_id
            m["category"] = fixture["category"]
        conservative_matches.extend(cons_matches)

        # NOTE on what this metric actually proves: `time_output["duration_ms"]`
        # is each backend's OWN self-reported max(line.end_ms) (see e.g.
        # soniox_v5.py::estimate_duration_ms), not an externally-verified true
        # WAV length — no reliable true-duration ground truth is available
        # from the committed JSON alone. Since every combined start_ms is
        # drawn from an aligned word taken FROM time_output itself, this
        # count is STRUCTURALLY guaranteed to be 0 for any combo (a combined
        # line can never start later than the time source's own last word) —
        # it demonstrates that structural guarantee, it does not "verify
        # against the true audio length" the way the raw gemini36-flash/
        # qwen35-omni backends' past-end-of-file lines (a genuine LLM
        # hallucination artifact, cited from the prior sweep) would need.
        duration_ms = time_output.get("duration_ms") or 0
        if duration_ms:
            n_lines_start_past_audio_end += sum(
                1 for line in scoreable if line["start_ms"] > duration_ms
            )

        text_counts = normalized_line_text_counts(line_source_lines)
        rep_total = rep_aligned = uniq_total = uniq_aligned = 0
        for line in combined["lines"]:
            norm = score_one_call.normalize_text(line.get("text") or "")
            if text_counts[norm] > 1:
                rep_total += line["n_words_in_line"]
                rep_aligned += line["n_words_aligned"]
            else:
                uniq_total += line["n_words_in_line"]
                uniq_aligned += line["n_words_aligned"]
        align_rate_records.append(
            {
                "video_id": video_id,
                "category": fixture["category"],
                "words_total": combined["stats"]["n_words_total"],
                "words_aligned": combined["stats"]["n_words_aligned"],
                "word_align_rate": combined["stats"]["word_align_rate"],
                "repeated_words_total": rep_total,
                "repeated_words_aligned": rep_aligned,
                "unique_words_total": uniq_total,
                "unique_words_aligned": uniq_aligned,
            }
        )

        line_words = clt.flatten_line_source(line_source_lines)
        time_words = clt.flatten_time_source(time_source_lines)
        for m in clt.diagnose_replace_block_mismatches(line_words, time_words):
            mismatch_examples.append(
                {
                    "video_id": video_id,
                    "line_word": m.line_text,
                    "time_word": m.time_text,
                    "ratio": m.ratio,
                    "recovered": m.recovered,
                }
            )

    report = score_one_call.build_backend_report(cid, fixture_scores)
    categories = sorted({f["category"] for f in manifest.values()})
    report["conservative"] = {
        "aggregate": conservative_aggregate(conservative_matches),
        "by_category": {
            cat: conservative_aggregate(
                [m for m in conservative_matches if m["category"] == cat]
            )
            for cat in categories
        },
    }

    total_words = sum(r["words_total"] for r in align_rate_records)
    total_aligned = sum(r["words_aligned"] for r in align_rate_records)
    total_rep_total = sum(r["repeated_words_total"] for r in align_rate_records)
    total_rep_aligned = sum(r["repeated_words_aligned"] for r in align_rate_records)
    total_uniq_total = sum(r["unique_words_total"] for r in align_rate_records)
    total_uniq_aligned = sum(r["unique_words_aligned"] for r in align_rate_records)
    report["word_alignment"] = {
        "overall_rate": round(total_aligned / total_words, 3) if total_words else None,
        "repeated_line_words_rate": round(total_rep_aligned / total_rep_total, 3)
        if total_rep_total
        else None,
        "unique_line_words_rate": round(total_uniq_aligned / total_uniq_total, 3)
        if total_uniq_total
        else None,
        "repeated_line_words_total": total_rep_total,
        "unique_line_words_total": total_uniq_total,
        "per_fixture": align_rate_records,
    }
    report["n_lines_start_past_audio_end"] = n_lines_start_past_audio_end
    report["mismatch_examples_sample"] = mismatch_examples[:80]
    return report


def run_baseline(
    backend: str, manifest: dict[str, dict[str, Any]], raw_dir: Path
) -> dict[str, Any]:
    """Score a backend's own untouched lines[] via score_one_call.py's
    unmodified functions — the "before" picture the combos are compared
    against."""
    fixture_scores: list[dict[str, Any]] = []
    for video_id, fixture in manifest.items():
        produced = score_one_call.load_produced(raw_dir, backend, video_id)
        if produced is None:
            fixture_scores.append(
                {
                    "backend": backend,
                    "video_id": video_id,
                    "category": fixture["category"],
                    "error": "output file missing or unparseable",
                }
            )
            continue
        fixture_scores.append(
            score_one_call.score_fixture(
                backend=backend,
                video_id=video_id,
                category=fixture["category"],
                produced=produced,
                gold_lines=fixture["gold_lines"],
            )
        )
    return score_one_call.build_backend_report(backend, fixture_scores)


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
    p.add_argument(
        "--combine-raw-dir",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-combine-raw",
    )
    p.add_argument(
        "--out",
        type=Path,
        default=root / "eval" / "lyrics" / "reports" / "2026-08-05-combine-scores.json",
    )
    args = p.parse_args(argv)

    manifest = score_one_call.load_manifest(args.manifest)
    logger.info("manifest loaded: %d fixtures", len(manifest))

    results: dict[str, Any] = {"baselines": {}, "combos": {}}
    for backend in BASELINE_BACKENDS:
        results["baselines"][backend] = run_baseline(backend, manifest, args.raw_dir)

    for line_backend, time_backend in COMBOS:
        cid = combo_id(line_backend, time_backend)
        results["combos"][cid] = run_combo(
            line_backend=line_backend,
            time_backend=time_backend,
            manifest=manifest,
            raw_dir=args.raw_dir,
            combine_raw_dir=args.combine_raw_dir,
        )

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
        json.dumps(results, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logger.info("wrote %s", args.out)

    print(f"\n=== combine experiment summary ({args.out}) ===\n")
    print("--- baselines (each backend's own untouched lines) ---")
    for backend, report in results["baselines"].items():
        agg = report["aggregate"]
        print(
            f"  {backend:20s} within400={agg['pct_within_400ms']!s:>6}%  "
            f"median_delta={agg['median_abs_delta_ms']!s:>8}ms  "
            f"coverage={agg['gold_coverage_pct']!s:>6}%  "
            f"line_ratio={agg['line_count_ratio']}"
        )
    print("\n--- combos (LLM lines + ASR times) ---")
    for cid, report in results["combos"].items():
        agg = report["aggregate"]
        cons = report["conservative"]["aggregate"]
        wa = report["word_alignment"]
        print(f"  {cid}")
        print(
            f"    official     : within400={agg['pct_within_400ms']!s:>6}%  "
            f"median_delta={agg['median_abs_delta_ms']!s:>8}ms  "
            f"coverage={agg['gold_coverage_pct']!s:>6}%  "
            f"untimed_excluded={sum(s.get('n_lines_untimed_excluded', 0) for s in report['per_fixture'] if s.get('error') is None)}"
        )
        print(
            f"    conservative  : within400={cons['pct_within_400ms']!s:>6}%  "
            f"median_delta={cons['median_abs_delta_ms']!s:>8}ms  n_pairs={cons['n_pairs']}"
        )
        print(
            f"    word_align_rate={wa['overall_rate']}  "
            f"repeated_rate={wa['repeated_line_words_rate']}  "
            f"unique_rate={wa['unique_line_words_rate']}"
        )
        print(
            f"    lines_start_past_audio_end={report['n_lines_start_past_audio_end']}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
