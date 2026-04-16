"""Unit tests for autosub_drift.py pure functions."""

def test_placeholder():
    assert True


from autosub_drift import normalize_word


def test_normalize_lowercases():
    assert normalize_word("Love") == "love"


def test_normalize_strips_punctuation():
    assert normalize_word("love!") == "love"
    assert normalize_word("you're") == "youre"
    assert normalize_word("...don't?") == "dont"


def test_normalize_returns_empty_for_noise():
    assert normalize_word("[music]") == ""
    assert normalize_word(">>") == ""
    assert normalize_word("") == ""
    assert normalize_word("   ") == ""


def test_normalize_keeps_alphanumeric():
    assert normalize_word("123abc") == "123abc"


import json
from autosub_drift import parse_json3, Word


def test_parse_json3_extracts_word_level_starts():
    """yt-dlp json3 events contain segs with offset (relative to event tStartMs)."""
    raw = {
        "events": [
            {
                "tStartMs": 1000,
                "dDurationMs": 2000,
                "segs": [
                    {"utf8": "hello", "tOffsetMs": 0},
                    {"utf8": " ", "tOffsetMs": 200},
                    {"utf8": "world", "tOffsetMs": 250},
                ],
            }
        ]
    }
    words = parse_json3(json.dumps(raw))
    assert words == [
        Word(text="hello", start_ms=1000),
        Word(text="world", start_ms=1250),
    ]


def test_parse_json3_skips_whitespace_only_segs():
    raw = {
        "events": [
            {
                "tStartMs": 5000,
                "segs": [
                    {"utf8": "\n", "tOffsetMs": 0},
                    {"utf8": "ok", "tOffsetMs": 100},
                ],
            }
        ]
    }
    assert parse_json3(json.dumps(raw)) == [Word(text="ok", start_ms=5100)]


def test_parse_json3_falls_back_to_event_start_when_no_segs():
    """Sentence-level event without per-word offsets uses event tStartMs for the whole text."""
    raw = {
        "events": [
            {"tStartMs": 9000, "dDurationMs": 1000, "segs": [{"utf8": "whole sentence"}]}
        ]
    }
    # Single seg with no tOffsetMs -> treated as starting at the event boundary,
    # split by whitespace, all words get the same start_ms.
    assert parse_json3(json.dumps(raw)) == [
        Word(text="whole", start_ms=9000),
        Word(text="sentence", start_ms=9000),
    ]


def test_parse_json3_ignores_events_without_segs():
    raw = {"events": [{"tStartMs": 0}, {"tStartMs": 1000, "segs": [{"utf8": "yo"}]}]}
    assert parse_json3(json.dumps(raw)) == [Word(text="yo", start_ms=1000)]


from autosub_drift import match_word_streams, MatchResult


def test_match_perfect_alignment():
    qwen = [Word("hello", 1000), Word("world", 2000)]
    auto = [Word("hello", 1050), Word("world", 1980)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 0
    assert result.drifts_ms == [50, -20]


def test_match_skips_qwen_word_with_no_autosub_counterpart():
    qwen = [Word("hello", 1000), Word("ghost", 1500), Word("world", 2000)]
    auto = [Word("hello", 1000), Word("world", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 1
    assert result.drifts_ms == [0, 0]


def test_match_advances_only_on_match():
    """An unmatched qwen word does NOT advance the autosub pointer."""
    qwen = [Word("missing", 500), Word("hello", 1000), Word("world", 2000)]
    auto = [Word("hello", 1000), Word("world", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.skipped == 1


def test_match_window_boundary_no_match_beyond_n():
    """If the matching autosub word is beyond window_n, it's a skip."""
    qwen = [Word("hello", 1000), Word("target", 2000)]
    # 11 noise words then "target"
    auto = [Word(f"noise{i}", 1000 + i * 10) for i in range(11)] + [Word("target", 2000)]
    qwen_only_target = [Word("target", 2000)]
    result = match_word_streams(qwen_only_target, auto, window_n=10)
    assert result.matched == 0
    assert result.skipped == 1


def test_match_returns_zero_match_rate_when_disjoint():
    qwen = [Word("apple", 1000), Word("banana", 2000)]
    auto = [Word("orange", 1000), Word("grape", 2000)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 0
    assert result.skipped == 2
    assert result.drifts_ms == []


def test_match_normalizes_before_compare():
    qwen = [Word("Hello!", 1000), Word("world", 2000)]
    auto = [Word("hello", 1100), Word("World.", 2050)]
    result = match_word_streams(qwen, auto, window_n=10)
    assert result.matched == 2
    assert result.drifts_ms == [100, 50]


import math
from autosub_drift import compute_stats, make_histogram, DriftStats


def test_compute_stats_basic():
    drifts = [-100, 0, 100, 200]
    s = compute_stats(drifts)
    assert s.count == 4
    assert s.mean_ms == 50
    assert s.median_ms == 50
    assert s.min_ms == -100
    assert s.max_ms == 200
    assert math.isclose(s.rms_ms, math.sqrt((10000 + 0 + 10000 + 40000) / 4))


def test_compute_stats_empty_returns_zeros():
    s = compute_stats([])
    assert s.count == 0
    assert s.mean_ms == 0
    assert s.rms_ms == 0.0
    assert s.median_ms == 0
    assert s.min_ms == 0
    assert s.max_ms == 0
    assert s.p05_ms == 0
    assert s.p95_ms == 0


def test_compute_stats_percentiles_with_100_values():
    drifts = list(range(100))  # 0..99
    s = compute_stats(drifts)
    # nearest-rank percentile: p95 = value at index ceil(0.95 * 100) - 1 = 94
    assert s.p95_ms == 94
    assert s.p05_ms == 4


def test_make_histogram_buckets_and_renders_ascii():
    drifts = [-1500, -400, -50, 50, 400, 1500]
    buckets = [-2000, -1000, -500, -300, -100, 0, 100, 300, 500, 1000, 2000]
    text = make_histogram(drifts, buckets)
    assert "[-2000, -1000)" in text
    assert "[1000, 2000)" in text
    # one drift in each of the 6 represented buckets, others empty
    assert text.count("#") == 6


def test_make_histogram_handles_empty_drifts():
    text = make_histogram([], [-1000, 0, 1000])
    assert "no data" in text.lower()
