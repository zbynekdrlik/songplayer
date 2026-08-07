"""Tests for confidence_correlation.py — the ElevenLabs mean_word_loss vs
greedy-match timing-error correlation used in the 2026-08-05 elevenlabs-fa
report. Covers the pure math helpers (pearson_r, bucket_by_quartile) with
hand-computable inputs, and the manifest/raw-dir join against small
in-memory fixture data written to a tmp_path."""

from __future__ import annotations

import json
import math

from eval.lyrics.aligners_11l.confidence_correlation import (
    bucket_by_quartile,
    collect_loss_delta_pairs,
    pearson_r,
)


def test_pearson_r_perfect_positive_correlation():
    xs = [1.0, 2.0, 3.0, 4.0]
    ys = [10.0, 20.0, 30.0, 40.0]
    r = pearson_r(xs, ys)
    assert r is not None
    assert math.isclose(r, 1.0, rel_tol=1e-9)


def test_pearson_r_perfect_negative_correlation():
    xs = [1.0, 2.0, 3.0, 4.0]
    ys = [40.0, 30.0, 20.0, 10.0]
    r = pearson_r(xs, ys)
    assert r is not None
    assert math.isclose(r, -1.0, rel_tol=1e-9)


def test_pearson_r_no_correlation_returns_none_on_zero_variance():
    # constant y series -> variance is zero -> undefined correlation
    xs = [1.0, 2.0, 3.0]
    ys = [5.0, 5.0, 5.0]
    assert pearson_r(xs, ys) is None


def test_pearson_r_requires_at_least_two_points():
    assert pearson_r([1.0], [2.0]) is None
    assert pearson_r([], []) is None


def test_pearson_r_mismatched_lengths_returns_none():
    assert pearson_r([1.0, 2.0], [1.0]) is None


def test_pearson_r_matches_known_value():
    # classic textbook example: r = 0.9535...
    xs = [43, 21, 25, 42, 57, 59]
    ys = [99, 65, 79, 75, 87, 81]
    r = pearson_r([float(x) for x in xs], [float(y) for y in ys])
    assert r is not None
    assert math.isclose(r, 0.5298, abs_tol=0.01)


def test_bucket_by_quartile_splits_into_four_groups_ascending_by_loss():
    # 8 pairs, evenly splittable into 4 buckets of 2
    pairs = [
        (0.1, 100.0),
        (0.2, 150.0),
        (0.3, 200.0),
        (0.4, 250.0),
        (0.5, 300.0),
        (0.6, 500.0),
        (0.7, 700.0),
        (0.8, 900.0),
    ]
    buckets = bucket_by_quartile(pairs)
    assert len(buckets) == 4
    assert all(b["n"] == 2 for b in buckets)
    # ascending loss order: Q1 has the lowest-loss (most confident) pairs
    assert buckets[0]["loss_range"] == [0.1, 0.2]
    assert buckets[-1]["loss_range"] == [0.7, 0.8]
    # Q1's timing error must be lower than Q4's on this monotonic fixture
    assert buckets[0]["mean_abs_delta_ms"] < buckets[-1]["mean_abs_delta_ms"]


def test_bucket_by_quartile_empty_input_returns_empty_list():
    assert bucket_by_quartile([]) == []


def test_bucket_by_quartile_pct_within_400ms():
    pairs = [(0.1, 100.0), (0.2, 500.0), (0.3, 200.0), (0.4, 900.0)]
    buckets = bucket_by_quartile(pairs)
    # single bucket of 4 (4 // 4 quartiles = 1 each)
    assert len(buckets) == 4
    within = [b["pct_within_400ms"] for b in buckets]
    assert within == [100.0, 0.0, 100.0, 0.0]


def _write_fixture(raw_dir, backend, video_id, lines):
    raw_dir.mkdir(parents=True, exist_ok=True)
    (raw_dir / f"{backend}_{video_id}.json").write_text(
        json.dumps({"backend_id": backend, "lines": lines}), encoding="utf-8"
    )


def test_collect_loss_delta_pairs_joins_matched_lines_and_excludes_poisoned(
    tmp_path,
):
    raw_dir = tmp_path / "raw"
    backend = "elevenlabs-fa"

    manifest = {
        "song_a": {
            "video_id": "song_a",
            "category": "clean_pop",
            "gold_lines": [
                {"text": "hello world", "start_ms": 1000, "end_ms": 2000},
                {"text": "second line here", "start_ms": 3000, "end_ms": 4000},
            ],
        },
        "Xvm4_fWkXe8": {
            "video_id": "Xvm4_fWkXe8",
            "category": "chant_repetition",
            "gold_lines": [
                {"text": "repeat phrase", "start_ms": 500, "end_ms": 1500},
            ],
        },
    }

    _write_fixture(
        raw_dir,
        backend,
        "song_a",
        [
            {
                "text": "hello world",
                "start_ms": 1100,
                "end_ms": 2100,
                "mean_word_loss": 0.05,
            },
            {
                "text": "second line here",
                "start_ms": 3500,
                "end_ms": 4400,
                "mean_word_loss": 0.9,
            },
        ],
    )
    _write_fixture(
        raw_dir,
        backend,
        "Xvm4_fWkXe8",
        [
            {
                "text": "repeat phrase",
                "start_ms": 600,
                "end_ms": 1600,
                "mean_word_loss": 0.4,
            },
        ],
    )

    pooled, poisoned = collect_loss_delta_pairs(manifest, raw_dir, backend)

    assert len(pooled) == 2
    assert len(poisoned) == 1
    # song_a's two lines: delta 100ms (loss 0.05) and 500ms (loss 0.9)
    assert (0.05, 100.0) in pooled
    assert (0.9, 500.0) in pooled
    # poisoned fixture's single matched line kept separate, never pooled
    assert poisoned == [(0.4, 100.0)]


def test_collect_loss_delta_pairs_skips_lines_with_no_loss_value(tmp_path):
    raw_dir = tmp_path / "raw"
    backend = "elevenlabs-fa"
    manifest = {
        "song_b": {
            "video_id": "song_b",
            "category": "clean_pop",
            "gold_lines": [{"text": "only line", "start_ms": 0, "end_ms": 1000}],
        }
    }
    _write_fixture(
        raw_dir,
        backend,
        "song_b",
        [{"text": "only line", "start_ms": 50, "end_ms": 1050, "mean_word_loss": None}],
    )

    pooled, poisoned = collect_loss_delta_pairs(manifest, raw_dir, backend)
    assert pooled == []
    assert poisoned == []


def test_collect_loss_delta_pairs_missing_output_file_is_skipped_not_fatal(
    tmp_path,
):
    raw_dir = tmp_path / "raw"
    backend = "elevenlabs-fa"
    manifest = {
        "missing_song": {
            "video_id": "missing_song",
            "category": "clean_pop",
            "gold_lines": [{"text": "x", "start_ms": 0, "end_ms": 1000}],
        }
    }
    pooled, poisoned = collect_loss_delta_pairs(manifest, raw_dir, backend)
    assert pooled == []
    assert poisoned == []
