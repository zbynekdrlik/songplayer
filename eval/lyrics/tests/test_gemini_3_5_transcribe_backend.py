"""Unit tests for gemini_3_5_transcribe backend output parsing.

No network — every test builds the response dict by hand, per this
project's test-strictness rules (internal code paths must use real
implementations, but there is no internal Gemini implementation to call;
the HTTP calls themselves are the external boundary this test suite never
crosses).
"""

from __future__ import annotations

from typing import Any

from eval.lyrics.backends import gemini_3_5_transcribe as g35t


def test_parse_offset_to_ms_with_decimal() -> None:
    assert g35t.parse_offset_to_ms("5.200s") == 5200


def test_parse_offset_to_ms_without_decimal() -> None:
    assert g35t.parse_offset_to_ms("9s") == 9000


def test_parse_offset_to_ms_small_decimal() -> None:
    assert g35t.parse_offset_to_ms("0.05s") == 50


def test_parse_offset_to_ms_missing_returns_none() -> None:
    assert g35t.parse_offset_to_ms(None) is None
    assert g35t.parse_offset_to_ms("") is None


def test_parse_offset_to_ms_malformed_returns_none() -> None:
    assert g35t.parse_offset_to_ms("not-a-number") is None
    assert g35t.parse_offset_to_ms("5.2") is None  # missing trailing "s"


def test_word_infos_to_words_keeps_punctuation_and_null_confidence() -> None:
    word_infos: list[dict[str, Any]] = [
        {
            "type": "word_info",
            "text": "Jesus.",
            "start_index": 0,
            "end_index": 6,
            "start_offset": "5.200s",
            "end_offset": "5.9s",
        }
    ]
    words = g35t.word_infos_to_words(word_infos)
    assert words == [
        {"text": "Jesus.", "start_ms": 5200, "end_ms": 5900, "confidence": None}
    ]


def test_word_infos_to_words_skips_malformed_entries() -> None:
    word_infos: list[dict[str, Any]] = [
        {"type": "word_info", "text": "ok", "start_offset": "0s", "end_offset": "0.5s"},
        {"type": "word_info", "text": "", "start_offset": "1s", "end_offset": "1.5s"},
        {"type": "word_info", "text": "bad", "start_offset": None, "end_offset": "2s"},
    ]
    words = g35t.word_infos_to_words(word_infos)
    assert len(words) == 1
    assert words[0]["text"] == "ok"


def test_group_words_into_lines_splits_on_gap() -> None:
    words = [
        {"text": "hello", "start_ms": 0, "end_ms": 500, "confidence": None},
        {"text": "world", "start_ms": 550, "end_ms": 900, "confidence": None},
        # gap of 600ms > LINE_GAP_MS(400) -> new line
        {"text": "second", "start_ms": 1500, "end_ms": 2000, "confidence": None},
    ]
    lines = g35t.group_words_into_lines(words)
    assert len(lines) == 2
    assert lines[0]["text"] == "hello world"
    assert lines[0]["start_ms"] == 0
    assert lines[0]["end_ms"] == 900
    assert lines[0]["text_sk"] is None
    assert lines[1]["text"] == "second"
    assert lines[1]["start_ms"] == 1500


def test_group_words_into_lines_keeps_punctuation_in_joined_text() -> None:
    words = [
        {"text": "Hello,", "start_ms": 0, "end_ms": 300, "confidence": None},
        {"text": "world.", "start_ms": 320, "end_ms": 700, "confidence": None},
    ]
    lines = g35t.group_words_into_lines(words)
    assert len(lines) == 1
    assert lines[0]["text"] == "Hello, world."


def test_group_words_into_lines_empty_input_returns_zero_lines() -> None:
    assert g35t.group_words_into_lines([]) == []


def test_collect_word_infos_walks_multiple_steps_and_content_blocks() -> None:
    response: dict[str, Any] = {
        "steps": [
            {
                "content": [
                    {
                        "text": "Hello ",
                        "annotations": [
                            {
                                "type": "word_info",
                                "text": "Hello",
                                "start_offset": "0s",
                                "end_offset": "0.5s",
                            }
                        ],
                    },
                    {
                        "text": "world",
                        "annotations": [
                            {
                                "type": "word_info",
                                "text": "world",
                                "start_offset": "0.6s",
                                "end_offset": "1s",
                            }
                        ],
                    },
                ]
            },
            {
                "content": [
                    {
                        "text": "second line",
                        "annotations": [
                            {
                                "type": "word_info",
                                "text": "second",
                                "start_offset": "2s",
                                "end_offset": "2.4s",
                            },
                            {
                                "type": "word_info",
                                "text": "line",
                                "start_offset": "2.5s",
                                "end_offset": "3s",
                            },
                        ],
                    }
                ]
            },
        ]
    }
    word_infos = g35t.collect_word_infos(response)
    assert [wi["text"] for wi in word_infos] == ["Hello", "world", "second", "line"]


def test_collect_word_infos_ignores_non_word_info_annotations() -> None:
    response: dict[str, Any] = {
        "steps": [
            {
                "content": [
                    {
                        "annotations": [
                            {"type": "something_else", "text": "ignored"},
                            {
                                "type": "word_info",
                                "text": "kept",
                                "start_offset": "0s",
                                "end_offset": "0.5s",
                            },
                        ]
                    }
                ]
            }
        ]
    }
    word_infos = g35t.collect_word_infos(response)
    assert len(word_infos) == 1
    assert word_infos[0]["text"] == "kept"


def test_collect_word_infos_empty_response_returns_empty_list() -> None:
    assert g35t.collect_word_infos({}) == []
    assert g35t.collect_word_infos({"steps": []}) == []


def test_resolve_language_codes_default(monkeypatch: Any) -> None:
    monkeypatch.delenv("G35T_LANGUAGE_CODES", raising=False)
    assert g35t.resolve_language_codes() == ["en-US"]


def test_resolve_language_codes_comma_separated(monkeypatch: Any) -> None:
    monkeypatch.setenv("G35T_LANGUAGE_CODES", "en-US, es-ES")
    assert g35t.resolve_language_codes() == ["en-US", "es-ES"]


def test_resolve_language_codes_auto_means_empty_list(monkeypatch: Any) -> None:
    monkeypatch.setenv("G35T_LANGUAGE_CODES", "auto")
    assert g35t.resolve_language_codes() == []


def test_emit_result_writes_schema_valid_json(tmp_path: Any) -> None:
    out = tmp_path / "result.json"
    g35t.emit_result(
        out_path=out,
        wav_path="/abs/path/vocal16k.wav",
        duration_ms=900,
        lines=[
            {
                "text": "hi",
                "start_ms": 0,
                "end_ms": 500,
                "text_sk": None,
                "words": [
                    {"text": "hi", "start_ms": 0, "end_ms": 500, "confidence": None}
                ],
            }
        ],
        raw_confidence=None,
        metadata={"model": g35t.MODEL_SLUG, "elapsed_s": 3.2},
    )
    import json

    data = json.loads(out.read_text(encoding="utf-8"))
    assert data["backend_id"] == "gemini-3-5-transcribe"
    assert data["backend_revision"] == 1
    assert data["wav_path"] == "/abs/path/vocal16k.wav"
    assert data["duration_ms"] == 900
    assert data["raw_confidence"] is None
    assert data["lines"][0]["words"][0]["confidence"] is None
    assert data["metadata"]["model"] == "gemini-3.5-transcribe"


def test_estimate_duration_ms_returns_max_end() -> None:
    lines = [
        {"text": "a", "start_ms": 0, "end_ms": 500},
        {"text": "b", "start_ms": 1000, "end_ms": 3500},
    ]
    assert g35t.estimate_duration_ms(lines) == 3500


def test_estimate_duration_ms_returns_zero_on_empty() -> None:
    assert g35t.estimate_duration_ms([]) == 0


def test_is_retryable_status() -> None:
    assert g35t._is_retryable_status(429) is True
    assert g35t._is_retryable_status(500) is True
    assert g35t._is_retryable_status(503) is True
    assert g35t._is_retryable_status(404) is False
    assert g35t._is_retryable_status(200) is False
