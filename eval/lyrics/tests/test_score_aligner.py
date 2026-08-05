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
        "words": None if start_ms is None else [{"text": text, "start_ms": start_ms, "end_ms": end_ms}],
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
