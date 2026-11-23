"""soniox_v6.tokens_to_words: sub-word tokens joined on leading spaces (#349)."""

from eval.lyrics.review_2026_11.soniox_v6 import tokens_to_words


def test_joins_subwords_and_splits_on_leading_space():
    tokens = [
        {"text": "Hal", "start_ms": 100, "end_ms": 200},
        {"text": "le", "start_ms": 200, "end_ms": 300},
        {"text": "lujah", "start_ms": 300, "end_ms": 600},
        {"text": " Lord", "start_ms": 900, "end_ms": 1200},
    ]
    assert tokens_to_words(tokens) == [
        {"text": "Hallelujah", "start_ms": 100, "end_ms": 600},
        {"text": "Lord", "start_ms": 900, "end_ms": 1200},
    ]


def test_whitespace_token_is_a_boundary_and_events_dropped():
    tokens = [
        {"text": "a", "start_ms": 0, "end_ms": 10},
        {"text": " ", "start_ms": 10, "end_ms": 10},
        {"text": "b", "start_ms": 20, "end_ms": 30},
        {"text": "<music>", "start_ms": 30, "end_ms": 90, "is_audio_event": True},
        {"text": "c", "start_ms": 40, "end_ms": 50},
        {"text": " d", "start_ms": None, "end_ms": None},
    ]
    assert [w["text"] for w in tokens_to_words(tokens)] == ["a", "b", "c"]
