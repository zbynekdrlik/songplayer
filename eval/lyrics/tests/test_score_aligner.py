"""Tests for score_aligner.py — the forced-aligner shootout scorer."""

from __future__ import annotations

import json
from pathlib import Path

from eval.lyrics import score_aligner


def _line(text: str, start_ms: int | None, end_ms: int | None) -> dict:
    return {
        "text": text,
        "start_ms": start_ms,
        "end_ms": end_ms,
        "text_sk": None,
        "words": None
        if start_ms is None
        else [{"text": text, "start_ms": start_ms, "end_ms": end_ms}],
    }


def test_to_scoreable_lines_filters_untimed() -> None:
    lines = [
        _line("timed line", 100, 500),
        _line("untimed line", None, None),
        _line("another timed line", 600, 900),
    ]
    scoreable, n_untimed = score_aligner.to_scoreable_lines(lines)
    assert len(scoreable) == 2
    assert n_untimed == 1
    assert all(line["start_ms"] is not None for line in scoreable)


def test_to_scoreable_lines_all_timed() -> None:
    lines = [_line("a", 0, 100), _line("b", 100, 200)]
    scoreable, n_untimed = score_aligner.to_scoreable_lines(lines)
    assert len(scoreable) == 2
    assert n_untimed == 0


def test_score_one_fixture_missing_produced_returns_error() -> None:
    score, matches = score_aligner.score_one_fixture(
        backend="test-aligner",
        video_id="abc123",
        category="clean_pop",
        produced=None,
        gold_lines=[{"text": "hello world", "start_ms": 0, "end_ms": 1000}],
    )
    assert score["error"] == "output file missing or unparseable"
    assert matches == []


def test_score_one_fixture_reports_untimed_count_and_runtime() -> None:
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    produced = {
        "lines": [
            _line("hello world today", 50, 1000),
            _line("an untimed line no words matched", None, None),
        ],
        "metadata": {"runtime_sec": 12.5},
    }
    score, matches = score_aligner.score_one_fixture(
        backend="test-aligner",
        video_id="abc123",
        category="clean_pop",
        produced=produced,
        gold_lines=gold,
    )
    assert score["n_lines_total"] == 2
    assert score["n_lines_untimed"] == 1
    assert score["untimed_pct"] == 50.0
    assert score["runtime_sec"] == 12.5
    assert score["n_matched"] == 1


def test_score_backend_excludes_poisoned_fixture_from_aggregate(
    tmp_path: Path,
) -> None:
    raw_dir = tmp_path
    backend = "test-aligner"

    normal_gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    normal_out = {
        "lines": [_line("hello world today", 50, 1000)],
        "metadata": {"runtime_sec": 5.0},
    }
    (raw_dir / f"{backend}_normalvid.json").write_text(json.dumps(normal_out))

    poisoned_gold = [{"text": "keep on doing it", "start_ms": 0, "end_ms": 1000}]
    poisoned_out = {
        "lines": [_line("keep on doing it", None, None) for _ in range(5)],
        "metadata": {"runtime_sec": 30.0},
    }
    (raw_dir / f"{backend}_{score_aligner.POISONED_FIXTURE_VIDEO_ID}.json").write_text(
        json.dumps(poisoned_out)
    )

    manifest = {
        "normalvid": {
            "video_id": "normalvid",
            "category": "clean_pop",
            "gold_lines": normal_gold,
        },
        score_aligner.POISONED_FIXTURE_VIDEO_ID: {
            "video_id": score_aligner.POISONED_FIXTURE_VIDEO_ID,
            "category": "clean_pop",
            "gold_lines": poisoned_gold,
        },
    }

    report = score_aligner.score_backend(backend, manifest, raw_dir)

    # the poisoned fixture must NOT be counted in the pooled aggregate
    assert report["aggregate"]["n_fixtures"] == 1
    assert report["untimed"]["total_lines"] == 1
    assert report["untimed"]["total_untimed"] == 0

    # but it must still be visible, scored on its own
    pf = report["poisoned_fixture"]
    assert pf["video_id"] == score_aligner.POISONED_FIXTURE_VIDEO_ID
    assert pf["untimed_pct"] == 100.0


def test_score_one_fixture_errored_record_carries_gold_count() -> None:
    """REGRESSION (synthesis #7): a crashed fixture with no output file must
    still contribute its gold lines to the honest denominator — otherwise a
    backend that dies silently outscores one that writes all-untimed nulls."""
    gold = [
        {"text": "line one of two", "start_ms": 0, "end_ms": 1000},
        {"text": "line two of two", "start_ms": 1000, "end_ms": 2000},
    ]
    score, matches = score_aligner.score_one_fixture(
        backend="test-aligner",
        video_id="abc123",
        category="clean_pop",
        produced=None,
        gold_lines=gold,
    )
    assert score["error"] == "output file missing or unparseable"
    assert score["n_gold"] == 2
    assert matches == []


def test_score_backend_errored_fixture_stays_in_gold_denominator(
    tmp_path: Path,
) -> None:
    """REGRESSION (synthesis #7): end-to-end — the pooled aggregate must expose
    a denominator that includes the fixtures the backend produced nothing for."""
    raw_dir = tmp_path
    backend = "test-aligner"
    (raw_dir / f"{backend}_okvid.json").write_text(
        json.dumps({"lines": [_line("hello world today", 50, 1000)]})
    )
    manifest = {
        "okvid": {
            "video_id": "okvid",
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
    report = score_aligner.score_backend(backend, manifest, raw_dir)
    agg = report["aggregate"]
    assert agg["total_gold_lines"] == 1
    assert agg["total_gold_lines_all_fixtures"] == 4
    assert agg["gold_coverage_pct"] == 100.0
    assert agg["gold_coverage_pct_all_fixtures"] == 25.0


def test_score_one_fixture_carries_device_and_oom_flag() -> None:
    """synthesis #6: a CPU-fallback run must never be blended into a GPU mean."""
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    produced = {
        "lines": [_line("hello world today", 50, 1000)],
        "metadata": {"runtime_sec": 500.0, "device": "cpu", "cuda_oom_retried": True},
    }
    score, _ = score_aligner.score_one_fixture(
        backend="test-aligner",
        video_id="abc123",
        category="clean_pop",
        produced=produced,
        gold_lines=gold,
    )
    assert score["device"] == "cpu"
    assert score["cuda_oom_retried"] is True


def test_score_backend_splits_runtime_by_device(tmp_path: Path) -> None:
    """synthesis #6: two fixtures that fell back to CPU must not drag the
    reported GPU runtime of the model."""
    raw_dir = tmp_path
    backend = "test-aligner"
    manifest = {}
    for vid, runtime, device, oom in (
        ("gpu1", 100.0, "cuda", False),
        ("gpu2", 120.0, "cuda", False),
        ("cpu1", 600.0, "cpu", True),
    ):
        (raw_dir / f"{backend}_{vid}.json").write_text(
            json.dumps(
                {
                    "lines": [_line("hello world today", 50, 1000)],
                    "metadata": {
                        "runtime_sec": runtime,
                        "device": device,
                        "cuda_oom_retried": oom,
                    },
                }
            )
        )
        manifest[vid] = {
            "video_id": vid,
            "category": "clean_pop",
            "gold_lines": [
                {"text": "hello world today", "start_ms": 0, "end_ms": 1000}
            ],
        }

    report = score_aligner.score_backend(backend, manifest, raw_dir)
    runtime = report["runtime"]
    assert runtime["n_fixtures_with_runtime"] == 3
    assert runtime["mean_runtime_sec"] == 273.3  # device-blended, kept for history
    assert runtime["n_cuda_oom_retried"] == 1
    by_device = runtime["by_device"]
    assert by_device["cuda"]["n_fixtures"] == 2
    assert by_device["cuda"]["mean_runtime_sec"] == 110.0
    assert by_device["cpu"]["n_fixtures"] == 1
    assert by_device["cpu"]["mean_runtime_sec"] == 600.0


def test_score_backend_reports_monotonic_view(tmp_path: Path) -> None:
    """synthesis #3: a third, order-respecting view alongside official and
    conservative — the official matcher may pair a produced line to a gold line
    that precedes one already consumed."""
    raw_dir = tmp_path
    backend = "test-aligner"
    (raw_dir / f"{backend}_v1.json").write_text(
        json.dumps(
            {
                "lines": [
                    _line("delta echo foxtrot", 2000, 3000),
                    _line("alpha bravo charlie", 9000, 10000),
                ]
            }
        )
    )
    manifest = {
        "v1": {
            "video_id": "v1",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "alpha bravo charlie", "start_ms": 1000, "end_ms": 2000},
                {"text": "delta echo foxtrot", "start_ms": 5000, "end_ms": 6000},
            ],
        }
    }
    report = score_aligner.score_backend(backend, manifest, raw_dir)
    assert report["aggregate"]["total_matched_lines"] == 2
    mono = report["monotonic"]["aggregate"]
    assert mono["n_pairs"] == 1
    assert mono["pct_gold_within_400ms"] == 0.0
    assert "clean_pop" in report["monotonic"]["by_category"]


def test_score_backend_monotonic_gold_normalized_differs_from_conditional(
    tmp_path: Path,
) -> None:
    """REGRESSION: `test_score_backend_reports_monotonic_view` above passes
    even if `total_gold` were wired to `n_pairs` instead of the true gold
    count, because its fixture's conditional `pct_within_400ms` is ALSO
    0.0 — 0/1 and 0/2 both round to 0.0, so the test can't tell the two
    apart. Here the single produced line is IN-GATE (within 400ms) while
    gold has 4 lines total, so the two views genuinely diverge: 100%
    conditional vs 25% gold-normalized."""
    raw_dir = tmp_path
    backend = "test-aligner"
    (raw_dir / f"{backend}_v1.json").write_text(
        json.dumps({"lines": [_line("alpha bravo charlie", 50, 1000)]})
    )
    manifest = {
        "v1": {
            "video_id": "v1",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "alpha bravo charlie", "start_ms": 0, "end_ms": 1000},
                {"text": "delta echo foxtrot", "start_ms": 2000, "end_ms": 3000},
                {"text": "golf hotel india", "start_ms": 4000, "end_ms": 5000},
                {"text": "juliet kilo lima", "start_ms": 6000, "end_ms": 7000},
            ],
        }
    }
    report = score_aligner.score_backend(backend, manifest, raw_dir)
    mono = report["monotonic"]["aggregate"]
    assert mono["n_pairs"] == 1
    assert mono["pct_within_400ms"] == 100.0
    assert mono["pct_gold_within_400ms"] == 25.0


def test_score_backend_missing_poisoned_fixture_reports_error(
    tmp_path: Path,
) -> None:
    raw_dir = tmp_path
    backend = "test-aligner"
    normal_gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    normal_out = {"lines": [_line("hello world today", 50, 1000)]}
    (raw_dir / f"{backend}_normalvid.json").write_text(json.dumps(normal_out))

    manifest = {
        "normalvid": {
            "video_id": "normalvid",
            "category": "clean_pop",
            "gold_lines": normal_gold,
        },
    }
    report = score_aligner.score_backend(backend, manifest, raw_dir)
    assert "error" in report["poisoned_fixture"]
