"""Tests for score_one_call.py — the mechanical (non-judge) scorer for the
2026-08-05 one-call north-star sweep."""

from __future__ import annotations

from eval.lyrics import score_one_call


def test_normalize_text_strips_punctuation_and_case() -> None:
    assert score_one_call.normalize_text("Hey, don't STOP!") == "hey dont stop"
    assert score_one_call.normalize_text("  multiple   spaces  ") == "multiple spaces"


def test_percentile_linear_interpolation() -> None:
    assert score_one_call.percentile([1, 2, 3, 4, 5], 0.9) == 4.6
    assert score_one_call.percentile([5], 0.9) == 5
    assert score_one_call.percentile([], 0.9) is None


def test_greedy_match_disambiguates_repeated_text_by_closest_start() -> None:
    """Chant-repetition style fixtures: identical text repeats at several
    gold timestamps. Each produced line must land on the NEAREST-in-time
    occurrence, not an arbitrary one, and no gold line is used twice."""
    gold = [
        {"text": "Burn, burn", "start_ms": 1000, "end_ms": 2000},
        {"text": "Burn, burn", "start_ms": 5000, "end_ms": 6000},
        {"text": "Burn, burn", "start_ms": 9000, "end_ms": 10000},
    ]
    produced = [
        {"text": "Burn, burn", "start_ms": 5200, "end_ms": 6200},
        {"text": "Burn, burn", "start_ms": 900, "end_ms": 2000},
        {"text": "Burn, burn", "start_ms": 9100, "end_ms": 10000},
    ]
    matches = score_one_call.greedy_match(produced, gold)
    assert len(matches) == 3
    gold_indices = {m["gold_idx"] for m in matches}
    assert gold_indices == {0, 1, 2}  # every gold line used exactly once
    for m in matches:
        p = produced[m["produced_idx"]]
        g = gold[m["gold_idx"]]
        assert m["abs_delta_ms"] == abs(p["start_ms"] - g["start_ms"])
        # each produced line's chosen match really is its closest-in-time gold line
        assert m["abs_delta_ms"] <= 4200


def test_greedy_match_rejects_dissimilar_text() -> None:
    gold = [{"text": "completely different lyrics here", "start_ms": 0, "end_ms": 1000}]
    produced = [
        {"text": "totally unrelated words indeed", "start_ms": 0, "end_ms": 1000}
    ]
    assert score_one_call.greedy_match(produced, gold) == []


def test_score_fixture_matched_line_stats() -> None:
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    produced = [
        {
            "text": "hello world today",
            "start_ms": 150,
            "end_ms": 1000,
            "text_sk": "ahoj svet dnes",
        }
    ]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced},
        gold_lines=gold,
    )
    assert score["n_matched"] == 1
    assert score["gold_coverage_pct"] == 100.0
    assert score["produced_precision_pct"] == 100.0
    assert score["mean_abs_delta_ms"] == 150.0
    assert score["median_abs_delta_ms"] == 150
    assert score["pct_within_400ms"] == 100.0
    assert score["pct_within_1000ms"] == 100.0
    assert score["error"] is None


def test_score_fixture_no_match_reports_none_timing() -> None:
    gold = [{"text": "completely different lyrics here", "start_ms": 0, "end_ms": 1000}]
    produced = [
        {
            "text": "totally unrelated words indeed",
            "start_ms": 0,
            "end_ms": 1000,
            "text_sk": None,
        }
    ]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced},
        gold_lines=gold,
    )
    assert score["n_matched"] == 0
    assert score["gold_coverage_pct"] == 0.0
    assert score["mean_abs_delta_ms"] is None
    assert score["pct_within_400ms"] is None


def test_score_fixture_empty_produced_lines() -> None:
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": []},
        gold_lines=gold,
    )
    assert score["n_produced"] == 0
    assert score["n_matched"] == 0
    assert score["produced_precision_pct"] is None
    assert score["line_count_ratio"] == 0.0


def test_score_fixture_sk_ok_requires_diacritic_and_non_duplicate() -> None:
    gold = [
        {"text": "line one text", "start_ms": 0, "end_ms": 1000},
        {"text": "line two text", "start_ms": 1000, "end_ms": 2000},
        {"text": "line three text", "start_ms": 2000, "end_ms": 3000},
    ]
    produced = [
        # has a diacritic, meaningfully different from English -> counts
        {
            "text": "line one text",
            "start_ms": 0,
            "end_ms": 1000,
            "text_sk": "riadok jeden text so ž",
        },
        # no diacritic at all -> does not count
        {
            "text": "line two text",
            "start_ms": 1000,
            "end_ms": 2000,
            "text_sk": "riadok dva text",
        },
        # missing translation entirely -> does not count
        {"text": "line three text", "start_ms": 2000, "end_ms": 3000, "text_sk": None},
    ]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced},
        gold_lines=gold,
    )
    assert score["sk_ok_pct"] == round(1 / 3 * 100.0, 1)


def test_score_fixture_word_timings_presence_and_count() -> None:
    gold = [{"text": "hello world", "start_ms": 0, "end_ms": 1000}]
    produced_with_words = [
        {
            "text": "hello world",
            "start_ms": 0,
            "end_ms": 1000,
            "words": [
                {"text": "hello", "start_ms": 0, "end_ms": 400},
                {"text": "world", "start_ms": 400, "end_ms": 1000},
            ],
        }
    ]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced_with_words},
        gold_lines=gold,
    )
    assert score["has_word_timings"] is True
    assert score["word_count"] == 2

    produced_no_words = [{"text": "hello world", "start_ms": 0, "end_ms": 1000}]
    score2 = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced_no_words},
        gold_lines=gold,
    )
    assert score2["has_word_timings"] is False
    assert score2["word_count"] is None


def test_score_fixture_gt32_chars_measures_original_language_by_default() -> None:
    gold = [{"text": "x", "start_ms": 0, "end_ms": 1000}]
    long_line = (
        "This produced line text is definitely longer than thirty two characters"
    )
    produced = [
        {"text": long_line, "start_ms": 0, "end_ms": 1000, "text_sk": "krátky sk"}
    ]
    score = score_one_call.score_fixture(
        backend="b",
        video_id="v",
        category="clean_pop",
        produced={"lines": produced},
        gold_lines=gold,
    )
    assert score["pct_gt32_chars_en"] == 100.0
    assert score["pct_gt32_chars_sk"] == 0.0


def test_pooled_aggregate_handles_errored_fixtures_without_crashing() -> None:
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    ok_score = score_one_call.score_fixture(
        backend="b",
        video_id="v1",
        category="clean_pop",
        produced={
            "lines": [{"text": "hello world today", "start_ms": 50, "end_ms": 1000}]
        },
        gold_lines=gold,
    )
    errored = {
        "backend": "b",
        "video_id": "v2",
        "category": "clean_pop",
        "error": "output file missing or unparseable",
    }
    agg = score_one_call.pooled_aggregate([ok_score, errored])
    assert agg["n_fixtures"] == 1
    assert agg["n_fixtures_errored"] == 1
    assert agg["total_matched_lines"] == 1


def test_build_backend_report_groups_by_category() -> None:
    gold = [{"text": "hello world today", "start_ms": 0, "end_ms": 1000}]
    s1 = score_one_call.score_fixture(
        backend="b",
        video_id="v1",
        category="clean_pop",
        produced={
            "lines": [{"text": "hello world today", "start_ms": 50, "end_ms": 1000}]
        },
        gold_lines=gold,
    )
    s2 = score_one_call.score_fixture(
        backend="b",
        video_id="v2",
        category="dense_vocal",
        produced={
            "lines": [{"text": "hello world today", "start_ms": 50, "end_ms": 1000}]
        },
        gold_lines=gold,
    )
    report = score_one_call.build_backend_report("b", [s1, s2])
    assert set(report["by_category"].keys()) == {"clean_pop", "dense_vocal"}
    assert report["aggregate"]["n_fixtures"] == 2


def _fake_score(
    video_id: str, pct_within_400ms: float, median_abs_delta_ms: float
) -> dict:
    return {
        "backend": "b",
        "video_id": video_id,
        "category": "clean_pop",
        "n_matched": 5,
        "pct_within_400ms": pct_within_400ms,
        "median_abs_delta_ms": median_abs_delta_ms,
        "gold_coverage_pct": 80.0,
        "error": None,
    }


def test_rank_fixtures_best_and_worst_by_within_400_then_median_delta() -> None:
    scores = [
        _fake_score("aaaaaaaaaaa", pct_within_400ms=100.0, median_abs_delta_ms=50.0),
        _fake_score("bbbbbbbbbbb", pct_within_400ms=90.0, median_abs_delta_ms=150.0),
        _fake_score("ccccccccccc", pct_within_400ms=90.0, median_abs_delta_ms=90.0),
        _fake_score("ddddddddddd", pct_within_400ms=10.0, median_abs_delta_ms=900.0),
        _fake_score("eeeeeeeeeee", pct_within_400ms=0.0, median_abs_delta_ms=2000.0),
    ]
    best, worst = score_one_call.rank_fixtures(scores, n=3)
    assert [s["video_id"] for s in best] == [
        "aaaaaaaaaaa",
        "ccccccccccc",
        "bbbbbbbbbbb",
    ]
    assert [s["video_id"] for s in worst] == [
        "eeeeeeeeeee",
        "ddddddddddd",
        "bbbbbbbbbbb",
    ]


def test_rank_fixtures_excludes_errored_and_unmatched() -> None:
    scores = [
        _fake_score("aaaaaaaaaaa", pct_within_400ms=100.0, median_abs_delta_ms=50.0),
        {
            "backend": "b",
            "video_id": "bbbbbbbbbbb",
            "category": "clean_pop",
            "error": "output file missing or unparseable",
        },
        {
            "backend": "b",
            "video_id": "ccccccccccc",
            "category": "clean_pop",
            "n_matched": 0,
            "pct_within_400ms": None,
            "median_abs_delta_ms": None,
            "gold_coverage_pct": 0.0,
            "error": None,
        },
    ]
    best, worst = score_one_call.rank_fixtures(scores, n=3)
    assert [s["video_id"] for s in best] == ["aaaaaaaaaaa"]
    assert [s["video_id"] for s in worst] == ["aaaaaaaaaaa"]
