#!/usr/bin/env python3
"""confidence_correlation.py — correlate ElevenLabs' per-line `mean_word_loss`
against greedy-match timing error, to check whether the aligner's own
confidence signal predicts when it is wrong.

`mean_word_loss` is the per-line average of ElevenLabs' per-word `loss`
field ("The average alignment loss/confidence score for this word,
calculated from its constituent characters" — LOWER is more confident; see
`elevenlabs_fa.py`'s module docstring for the full doc quote). This module
answers: does a HIGH loss (low confidence) line tend to have a LARGE timing
delta versus gold?

Deliberately thin glue, same pattern as `score_aligner.py`: imports
`score_one_call.greedy_match` (never modified) to get the SAME
produced-line <-> gold-line pairing already used for the official scoring
view, then joins each match's `abs_delta_ms` back to that produced line's
`mean_word_loss` — a join `score_fixture` itself does not expose (it only
returns the aggregate `raw_deltas_ms` list, not the underlying
`produced_idx`).

The poisoned fixture (`Xvm4_fWkXe8`) is EXCLUDED from the pooled
correlation for the same reason `score_aligner.py` excludes it from pooled
aggregates — its 395 hallucinated near-duplicate lines would dominate a
21-fixture correlation with one fixture's pathology. It is scored on its
own and reported separately, never silently dropped.

Usage:
    python3 eval/lyrics/aligners_11l/confidence_correlation.py \\
        --manifest eval/lyrics/manifest.json \\
        --raw-dir eval/lyrics/aligners_11l/raw \\
        --backend elevenlabs-fa \\
        --out eval/lyrics/aligners_11l/confidence_correlation.json
"""

from __future__ import annotations

import argparse
import json
import logging
import statistics
from pathlib import Path
from typing import Any

from eval.lyrics import score_one_call
from eval.lyrics.score_aligner import POISONED_FIXTURE_VIDEO_ID, to_scoreable_lines

logger = logging.getLogger("lyrics_eval.confidence_correlation")


def pearson_r(xs: list[float], ys: list[float]) -> float | None:
    """Pearson correlation coefficient. Returns None if fewer than 2 points
    or either series has zero variance (undefined correlation) — never
    fabricates a coefficient in that case."""
    n = len(xs)
    if n < 2 or len(ys) != n:
        return None
    mean_x = statistics.fmean(xs)
    mean_y = statistics.fmean(ys)
    cov = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys, strict=True))
    var_x = sum((x - mean_x) ** 2 for x in xs)
    var_y = sum((y - mean_y) ** 2 for y in ys)
    if var_x == 0 or var_y == 0:
        return None
    return cov / ((var_x**0.5) * (var_y**0.5))


def collect_loss_delta_pairs(
    manifest: dict[str, dict[str, Any]],
    raw_dir: Path,
    backend: str,
) -> tuple[list[tuple[float, float]], list[tuple[float, float]]]:
    """Returns (pooled_pairs, poisoned_pairs) — each a list of
    (mean_word_loss, abs_delta_ms) for every matched line. Pooled excludes
    the poisoned fixture; poisoned_pairs holds ONLY that fixture's matches."""
    pooled: list[tuple[float, float]] = []
    poisoned: list[tuple[float, float]] = []

    for video_id, fixture in manifest.items():
        produced = score_one_call.load_produced(raw_dir, backend, video_id)
        if produced is None:
            continue
        produced_lines = produced.get("lines") or []
        scoreable, _n_untimed = to_scoreable_lines(produced_lines)
        matches = score_one_call.greedy_match(scoreable, fixture["gold_lines"])
        target = poisoned if video_id == POISONED_FIXTURE_VIDEO_ID else pooled
        for m in matches:
            line = scoreable[m["produced_idx"]]
            loss = line.get("mean_word_loss")
            if loss is None:
                continue
            target.append((float(loss), float(m["abs_delta_ms"])))

    return pooled, poisoned


def bucket_by_quartile(pairs: list[tuple[float, float]]) -> list[dict[str, Any]]:
    """Splits matched lines into 4 equal-count buckets by `mean_word_loss`
    (ascending = most confident first) and reports mean/median timing error
    per bucket — the plain-language way to see whether confidence predicts
    accuracy without over-trusting a single correlation coefficient on
    noisy, non-linear real-world data."""
    if not pairs:
        return []
    ordered = sorted(pairs, key=lambda p: p[0])
    n = len(ordered)
    buckets: list[dict[str, Any]] = []
    q = 4
    for i in range(q):
        lo = (n * i) // q
        hi = (n * (i + 1)) // q
        chunk = ordered[lo:hi]
        if not chunk:
            continue
        losses = [c[0] for c in chunk]
        deltas = [c[1] for c in chunk]
        within_400 = sum(1 for d in deltas if d <= 400)
        buckets.append(
            {
                "quartile": i + 1,
                "n": len(chunk),
                "loss_range": [round(min(losses), 4), round(max(losses), 4)],
                "mean_loss": round(statistics.fmean(losses), 4),
                "mean_abs_delta_ms": round(statistics.fmean(deltas), 1),
                "median_abs_delta_ms": round(statistics.median(deltas), 1),
                "pct_within_400ms": round(within_400 / len(chunk) * 100.0, 1),
            }
        )
    return buckets


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(levelname)s %(name)s: %(message)s"
    )
    root = Path(__file__).resolve().parents[3]

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--manifest", type=Path, default=root / "eval" / "lyrics" / "manifest.json"
    )
    p.add_argument(
        "--raw-dir",
        type=Path,
        default=root / "eval" / "lyrics" / "aligners_11l" / "raw",
    )
    p.add_argument("--backend", default="elevenlabs-fa")
    p.add_argument(
        "--out",
        type=Path,
        default=root
        / "eval"
        / "lyrics"
        / "aligners_11l"
        / "confidence_correlation.json",
    )
    args = p.parse_args(argv)

    manifest = score_one_call.load_manifest(args.manifest)
    pooled, poisoned = collect_loss_delta_pairs(manifest, args.raw_dir, args.backend)

    result = {
        "backend": args.backend,
        "pooled": {
            "n_pairs": len(pooled),
            "pearson_r_loss_vs_abs_delta_ms": (
                round(r, 4)
                if (r := pearson_r([p[0] for p in pooled], [p[1] for p in pooled]))
                is not None
                else None
            ),
            "quartile_buckets": bucket_by_quartile(pooled),
        },
        "poisoned_fixture": {
            "video_id": POISONED_FIXTURE_VIDEO_ID,
            "n_pairs": len(poisoned),
            "pearson_r_loss_vs_abs_delta_ms": (
                round(r, 4)
                if (r := pearson_r([p[0] for p in poisoned], [p[1] for p in poisoned]))
                is not None
                else None
            ),
            "quartile_buckets": bucket_by_quartile(poisoned),
        },
    }

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    logger.info("wrote %s", args.out)

    print(f"\n=== confidence-vs-error correlation ({args.backend}) ===\n")
    print(
        f"pooled (21 fixtures, poisoned excluded): n_pairs={result['pooled']['n_pairs']} "
        f"pearson_r={result['pooled']['pearson_r_loss_vs_abs_delta_ms']}"
    )
    for b in result["pooled"]["quartile_buckets"]:
        print(
            f"  Q{b['quartile']} (n={b['n']:>3}, loss {b['loss_range'][0]}-{b['loss_range'][1]}): "
            f"median_delta={b['median_abs_delta_ms']:>7}ms  within400={b['pct_within_400ms']:>5}%"
        )
    print(
        f"\npoisoned fixture: n_pairs={result['poisoned_fixture']['n_pairs']} "
        f"pearson_r={result['poisoned_fixture']['pearson_r_loss_vs_abs_delta_ms']}"
    )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
