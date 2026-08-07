"""Tests for run_combine_experiment.py — the combo/baseline scorer whose
`combo-qwen35-omni-lines_aai-u35-translate-times` row is the BASELINE every
forced-aligner in the 2026-08-05 shootout is compared against.

The poisoned-fixture regression below is the load-bearing one: `score_aligner.py`
has always excluded `Xvm4_fWkXe8` from its pooled aggregates, so a baseline that
pooled it was being compared against aligner rows that did not — a 22-fixture
number under a "21 fixtures" header.
"""

from __future__ import annotations

import json
from pathlib import Path

from eval.lyrics import run_combine_experiment as rce


def _line(text: str, start_ms: int | None, end_ms: int | None) -> dict:
    return {
        "text": text,
        "start_ms": start_ms,
        "end_ms": end_ms,
        "text_sk": None,
        "words": None
        if start_ms is None
        else [
            {"text": w, "start_ms": start_ms, "end_ms": end_ms} for w in text.split()
        ],
    }


def _manifest_with_poisoned() -> dict[str, dict]:
    return {
        "normalvid": {
            "video_id": "normalvid",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "hello world today", "start_ms": 0, "end_ms": 1000},
                {"text": "second gold line here", "start_ms": 2000, "end_ms": 3000},
            ],
        },
        rce.POISONED_FIXTURE_VIDEO_ID: {
            "video_id": rce.POISONED_FIXTURE_VIDEO_ID,
            "category": "clean_pop",
            "gold_lines": [
                {"text": "keep on doing it", "start_ms": 0, "end_ms": 1000},
            ],
        },
    }


def test_run_baseline_excludes_poisoned_fixture_from_aggregate(
    tmp_path: Path,
) -> None:
    """REGRESSION (synthesis #1): the baseline row pooled 22 fixtures while
    every aligner row pooled 21."""
    raw_dir = tmp_path
    backend = "qwen35-omni"
    manifest = _manifest_with_poisoned()

    (raw_dir / f"{backend}_normalvid.json").write_text(
        json.dumps(
            {
                "lines": [
                    _line("hello world today", 50, 1000),
                    _line("second gold line here", 2100, 3000),
                ]
            }
        )
    )
    # The poisoned fixture: a degenerate repetition loop with wild timings.
    (raw_dir / f"{backend}_{rce.POISONED_FIXTURE_VIDEO_ID}.json").write_text(
        json.dumps({"lines": [_line("keep on doing it", 900_000, 901_000)]})
    )

    report = rce.run_baseline(backend, manifest, raw_dir)

    agg = report["aggregate"]
    assert agg["n_fixtures"] == 1, "poisoned fixture must not be pooled"
    assert agg["total_gold_lines"] == 2
    # its 900s delta must not be in the pooled deltas
    assert agg["pct_within_400ms"] == 100.0

    # excluded, but never silently dropped
    pf = report["poisoned_fixture"]
    assert pf["video_id"] == rce.POISONED_FIXTURE_VIDEO_ID
    assert pf["official"]["n_matched"] == 1
    assert {s["video_id"] for s in report["per_fixture"]} == {"normalvid"}


def test_run_combo_excludes_poisoned_fixture_from_every_view(
    tmp_path: Path,
) -> None:
    """REGRESSION (synthesis #1): the combo aggregate, the conservative view
    and the word-alignment rollup must all be over the same non-poisoned set."""
    raw_dir = tmp_path / "raw"
    raw_dir.mkdir()
    combine_raw_dir = tmp_path / "combine-raw"
    manifest = _manifest_with_poisoned()

    def _timed(text: str, start_ms: int) -> dict:
        words = text.split()
        return {
            "text": text,
            "start_ms": start_ms,
            "end_ms": start_ms + 100 * len(words),
            "text_sk": None,
            "words": [
                {
                    "text": w,
                    "start_ms": start_ms + 100 * i,
                    "end_ms": start_ms + 100 * (i + 1),
                }
                for i, w in enumerate(words)
            ],
        }

    for backend, offset in (("lineb", 0), ("timeb", 0)):
        (raw_dir / f"{backend}_normalvid.json").write_text(
            json.dumps(
                {
                    "duration_ms": 5000,
                    "lines": [
                        _timed("hello world today", 50 + offset),
                        _timed("second gold line here", 2100 + offset),
                    ],
                }
            )
        )
        (raw_dir / f"{backend}_{rce.POISONED_FIXTURE_VIDEO_ID}.json").write_text(
            json.dumps(
                {
                    "duration_ms": 950_000,
                    "lines": [_timed("keep on doing it", 900_000)],
                }
            )
        )

    report = rce.run_combo(
        line_backend="lineb",
        time_backend="timeb",
        manifest=manifest,
        raw_dir=raw_dir,
        combine_raw_dir=combine_raw_dir,
    )

    assert report["aggregate"]["n_fixtures"] == 1
    assert {s["video_id"] for s in report["per_fixture"]} == {"normalvid"}
    # conservative view pooled over the same set — the poisoned fixture's
    # 900s pair must not appear
    assert report["conservative"]["aggregate"]["n_pairs"] == 2
    # word-alignment rollup likewise
    assert len(report["word_alignment"]["per_fixture"]) == 1
    assert report["poisoned_fixture"]["video_id"] == rce.POISONED_FIXTURE_VIDEO_ID


def test_run_baseline_errored_fixture_records_gold_count(tmp_path: Path) -> None:
    """REGRESSION (synthesis #7): a fixture with no output file must still
    contribute its gold lines to the honest denominator."""
    raw_dir = tmp_path
    manifest = {
        "normalvid": {
            "video_id": "normalvid",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "hello world today", "start_ms": 0, "end_ms": 1000}
            ],
        },
        "crashedvid": {
            "video_id": "crashedvid",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "line one of three", "start_ms": 0, "end_ms": 1000},
                {"text": "line two of three", "start_ms": 1000, "end_ms": 2000},
                {"text": "line three of three", "start_ms": 2000, "end_ms": 3000},
            ],
        },
    }
    (raw_dir / "b_normalvid.json").write_text(
        json.dumps({"lines": [_line("hello world today", 50, 1000)]})
    )

    report = rce.run_baseline("b", manifest, raw_dir)
    errored = [s for s in report["per_fixture"] if s.get("error")]
    assert len(errored) == 1
    assert errored[0]["n_gold"] == 3
    assert report["aggregate"]["total_gold_lines_all_fixtures"] == 4
    assert report["aggregate"]["gold_coverage_pct_all_fixtures"] == 25.0


def test_conservative_aggregate_reports_gold_normalized_pct() -> None:
    matches = [{"abs_delta_ms": 100}, {"abs_delta_ms": 5000}]
    agg = rce.conservative_aggregate(matches, total_gold=10)
    assert agg["n_pairs"] == 2
    assert agg["pct_within_400ms"] == 50.0
    assert agg["pct_gold_within_400ms"] == 10.0
