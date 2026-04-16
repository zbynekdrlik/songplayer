"""Unit tests for autosub_drift.py pure functions."""

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


def test_parse_json3_sentence_level_event_assigns_all_words_event_start():
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
    assert s.p05_ms == -100
    assert s.p95_ms == 200


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


from autosub_drift import classify_bucket, recommendation_from_buckets


def test_classify_bucket_green():
    assert classify_bucket(0) == "green"
    assert classify_bucket(299) == "green"


def test_classify_bucket_amber():
    assert classify_bucket(300) == "amber"
    assert classify_bucket(700) == "amber"


def test_classify_bucket_red():
    assert classify_bucket(701) == "red"
    assert classify_bucket(5000) == "red"


def test_recommendation_red_kills_project():
    assert recommendation_from_buckets(["green", "amber", "red"]) == "kill"


def test_recommendation_amber_downgrades_to_refine():
    assert recommendation_from_buckets(["green", "amber", "green"]) == "refine"


def test_recommendation_all_green_greenlights():
    assert recommendation_from_buckets(["green", "green", "green"]) == "greenlight"


def test_recommendation_empty_input_kills():
    """No data is not a positive signal."""
    assert recommendation_from_buckets([]) == "kill"


from autosub_drift import write_report, SongResult


def test_write_report_includes_required_sections(tmp_path):
    results = [
        SongResult(
            video_id="abc123",
            title="Get This Party Started",
            artist="Planetshakers",
            error=None,
            match=MatchResult(
                matched=200, skipped=50,
                drifts_ms=[-100, 0, 100, 200, 300],
                total_qwen_words=250, total_autosub_words=270,
            ),
            stats=DriftStats(5, 100, 100, 173.2, -100, 300, -100, 300),
            histogram="bucket: ###",
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)

    text = out.read_text()
    assert "# Phase 2 Auto-Sub Drift Experiment" in text
    assert "## Methodology" in text
    assert "## Per-song results" in text
    assert "Get This Party Started" in text
    assert "Planetshakers" in text
    assert "abc123" in text
    assert "## Conclusion" in text
    assert "## Recommendation" in text
    assert "200/250" in text and "Qwen3 words attempted" in text  # match rate
    assert "bucket: ###" in text  # histogram


def test_write_report_handles_missing_data_song(tmp_path):
    results = [
        SongResult(
            video_id="ghi789",
            title="?",
            artist="?",
            error="no auto-subs available",
            match=None,
            stats=None,
            histogram=None,
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "no auto-subs available" in text
    assert "ghi789" in text


def test_write_report_recommendation_section_cites_worst_bucket(tmp_path):
    results = [
        SongResult("a", "T1", "A1", None,
                   MatchResult(10, 0, [0] * 10, 10, 10),
                   DriftStats(10, 0, 0, 100.0, 0, 0, 0, 0), "h"),
        SongResult("b", "T2", "A2", None,
                   MatchResult(10, 0, [800] * 10, 10, 10),
                   DriftStats(10, 800, 800, 800.0, 800, 800, 800, 800), "h"),
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "kill" in text.lower()
    assert "T2" in text  # the red song must be cited


def test_write_report_includes_raw_data_references_section(tmp_path):
    results = [
        SongResult(
            video_id="abc123",
            title="T",
            artist="A",
            error=None,
            match=MatchResult(matched=1, skipped=0, drifts_ms=[0],
                              total_qwen_words=1, total_autosub_words=1),
            stats=DriftStats(1, 0, 0, 0.0, 0, 0, 0, 0),
            histogram="h",
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "## Raw data references" in text
    assert "tempfile.mkdtemp" in text
    assert "_lyrics.json" in text


def test_write_report_match_rate_denominator_uses_attempted_not_total(tmp_path):
    """Noise tokens skipped without a match attempt must not dilute the rate."""
    results = [
        SongResult(
            video_id="v",
            title="T",
            artist="A",
            error=None,
            # 80 attempted (70 matched + 10 skipped), 100 total qwen words
            # (the extra 20 were noise tokens normalized away). Rate must be
            # 70/80 = 87.5%, not 70/100 = 70%.
            match=MatchResult(matched=70, skipped=10,
                              drifts_ms=[0]*70,
                              total_qwen_words=100, total_autosub_words=120),
            stats=DriftStats(70, 0, 0, 0.0, 0, 0, 0, 0),
            histogram="h",
        )
    ]
    out = tmp_path / "report.md"
    write_report(results, out)
    text = out.read_text()
    assert "70/80" in text
    assert "87.5%" in text


def test_make_histogram_includes_drift_at_last_edge():
    drifts = [2000]  # exactly the last bucket edge
    buckets = [-2000, -1000, 0, 1000, 2000]
    text = make_histogram(drifts, buckets)
    # the [1000, 2000] bin should have one entry; total ##s == 1
    assert text.count("#") == 1
