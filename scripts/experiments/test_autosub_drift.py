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
