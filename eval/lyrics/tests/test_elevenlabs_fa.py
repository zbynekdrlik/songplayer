"""Tests for elevenlabs_fa.py — the ElevenLabs Forced Alignment backend.

Covers the pure functions only (tokenization, transcript building, the
interleaved-whitespace filter, and the positional line reconstruction).
The live HTTP call (`call_forced_alignment`) is intentionally NOT mocked
here — per test-strictness.md, mocking the actual network call would hide
whether the real request shape still works; that is verified live against
the API instead (see README.md for the captured 200 response) and covered
by the fail-loud paths below (bad status, bad JSON, missing 'words' key)
using a fake `requests.Response`-shaped object rather than mocking
`requests` itself.
"""

from __future__ import annotations

import json
import wave

import pytest

from eval.lyrics.aligners_11l.elevenlabs_fa import (
    build_transcript,
    estimate_duration_ms,
    filter_content_words,
    load_reference_lines,
    reconstruct_lines,
    tokenize_line_text,
)


def test_tokenize_line_text_splits_on_whitespace_keeps_punctuation():
    assert tokenize_line_text("Nothing excites us like Jesus,") == [
        "Nothing",
        "excites",
        "us",
        "like",
        "Jesus,",
    ]


def test_tokenize_line_text_empty_string_returns_empty_list():
    assert tokenize_line_text("") == []
    assert tokenize_line_text("   ") == []


def test_load_reference_lines_reads_lines_array(tmp_path):
    p = tmp_path / "ref.json"
    p.write_text(json.dumps({"lines": [{"text": "hello world"}]}), encoding="utf-8")
    lines = load_reference_lines(p)
    assert lines == [{"text": "hello world"}]


def test_load_reference_lines_missing_lines_key_raises():
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "ref.json"
        p.write_text(json.dumps({"not_lines": []}), encoding="utf-8")
        with pytest.raises(RuntimeError, match="no lines"):
            load_reference_lines(p)


def test_load_reference_lines_empty_lines_array_raises(tmp_path):
    p = tmp_path / "ref.json"
    p.write_text(json.dumps({"lines": []}), encoding="utf-8")
    with pytest.raises(RuntimeError, match="no lines"):
        load_reference_lines(p)


def test_build_transcript_joins_tokens_single_space_and_maps_words_to_lines():
    ref_lines = [
        {"text": "hello world"},
        {"text": "second line here"},
    ]
    transcript, word_to_line, words_per_line = build_transcript(ref_lines)
    assert transcript == "hello world second line here"
    assert word_to_line == [0, 0, 1, 1, 1]
    assert words_per_line == [2, 3]


def test_build_transcript_empty_line_contributes_zero_words():
    ref_lines = [{"text": "one two"}, {"text": ""}, {"text": "three"}]
    transcript, word_to_line, words_per_line = build_transcript(ref_lines)
    assert transcript == "one two three"
    assert word_to_line == [0, 0, 2]
    assert words_per_line == [2, 0, 1]


def test_build_transcript_missing_text_key_treated_as_empty():
    ref_lines = [{"no_text_key": True}]
    transcript, word_to_line, words_per_line = build_transcript(ref_lines)
    assert transcript == ""
    assert word_to_line == []
    assert words_per_line == [0]


def test_filter_content_words_drops_interleaved_whitespace_entries():
    # Empirically observed shape: n real words -> 2n-1 total entries, with a
    # whitespace-only entry between every pair of real words.
    raw_words = [
        {"text": "hello", "start": 0.0, "end": 0.5, "loss": 0.1},
        {"text": " ", "start": 0.5, "end": 0.51, "loss": 0.0},
        {"text": "world", "start": 0.51, "end": 1.0, "loss": 0.2},
    ]
    content = filter_content_words(raw_words)
    assert [w["text"] for w in content] == ["hello", "world"]


def test_filter_content_words_drops_tabs_and_newlines_too():
    raw_words = [
        {"text": "a", "start": 0.0, "end": 0.1},
        {"text": "\t", "start": 0.1, "end": 0.11},
        {"text": "b", "start": 0.11, "end": 0.2},
        {"text": "\n", "start": 0.2, "end": 0.21},
        {"text": "c", "start": 0.21, "end": 0.3},
    ]
    content = filter_content_words(raw_words)
    assert [w["text"] for w in content] == ["a", "b", "c"]


def test_filter_content_words_empty_input_returns_empty():
    assert filter_content_words([]) == []


def test_filter_content_words_missing_text_key_treated_as_whitespace():
    raw_words = [{"start": 0.0, "end": 0.1}, {"text": "real", "start": 0.1, "end": 0.2}]
    content = filter_content_words(raw_words)
    assert [w["text"] for w in content] == ["real"]


def test_reconstruct_lines_basic_two_line_case():
    ref_lines = [{"text": "hello world", "text_sk": "ahoj svet"}, {"text": "bye"}]
    word_to_line = [0, 0, 1]
    content_words = [
        {"text": "hello", "start": 1.0, "end": 1.5, "loss": 0.1},
        {"text": "world", "start": 1.5, "end": 2.0, "loss": 0.3},
        {"text": "bye", "start": 3.0, "end": 3.4, "loss": 0.2},
    ]
    lines_out, stats = reconstruct_lines(ref_lines, word_to_line, content_words)

    assert lines_out[0]["text"] == "hello world"
    assert lines_out[0]["text_sk"] == "ahoj svet"
    assert lines_out[0]["start_ms"] == 1000
    assert lines_out[0]["end_ms"] == 2000
    assert lines_out[0]["mean_word_loss"] == pytest.approx(0.2)
    assert [w["text"] for w in lines_out[0]["words"]] == ["hello", "world"]

    assert lines_out[1]["start_ms"] == 3000
    assert lines_out[1]["end_ms"] == 3400
    assert lines_out[1]["mean_word_loss"] == pytest.approx(0.2)

    assert stats["n_lines_untimed_empty_text"] == 0
    assert stats["n_lines_untimed_other"] == 0
    assert stats["n_lines_timed"] == 2
    assert stats["n_lines"] == 2


def test_reconstruct_lines_empty_source_line_is_untimed():
    ref_lines = [{"text": "one"}, {"text": ""}]
    word_to_line = [0]
    content_words = [{"text": "one", "start": 0.0, "end": 0.5, "loss": 0.1}]
    lines_out, stats = reconstruct_lines(ref_lines, word_to_line, content_words)

    assert lines_out[1]["start_ms"] is None
    assert lines_out[1]["end_ms"] is None
    assert lines_out[1]["mean_word_loss"] is None
    assert lines_out[1]["words"] == []
    # a line with no source text is a DIFFERENT failure from a line whose words
    # came back untimed — the two counters must not be conflated
    assert stats["n_lines_untimed_empty_text"] == 1
    assert stats["n_lines_untimed_other"] == 0
    assert stats["n_lines_timed"] == 1


def test_reconstruct_lines_word_count_mismatch_fails_loud():
    ref_lines = [{"text": "hello world"}]
    word_to_line = [0, 0]
    content_words = [{"text": "hello", "start": 0.0, "end": 0.5, "loss": 0.1}]
    with pytest.raises(RuntimeError, match="word count mismatch"):
        reconstruct_lines(ref_lines, word_to_line, content_words)


def test_reconstruct_lines_word_missing_start_or_end_excluded_not_fatal():
    ref_lines = [{"text": "a b c"}]
    word_to_line = [0, 0, 0]
    content_words = [
        {"text": "a", "start": 0.0, "end": 0.1, "loss": 0.1},
        {"text": "b", "start": None, "end": None, "loss": 0.9},  # missing timing
        {"text": "c", "start": 0.3, "end": 0.4, "loss": 0.2},
    ]
    lines_out, stats = reconstruct_lines(ref_lines, word_to_line, content_words)

    assert lines_out[0]["start_ms"] == 0
    assert lines_out[0]["end_ms"] == 400
    # only 'a' and 'c' losses count toward the mean; 'b' was excluded
    assert lines_out[0]["mean_word_loss"] == pytest.approx((0.1 + 0.2) / 2)
    assert [w["text"] for w in lines_out[0]["words"]] == ["a", "c"]
    assert stats["n_words_missing_timing"] == 1


def test_reconstruct_lines_all_words_missing_timing_line_is_untimed():
    ref_lines = [{"text": "a b"}]
    word_to_line = [0, 0]
    content_words = [
        {"text": "a", "start": None, "end": None, "loss": 0.1},
        {"text": "b", "start": None, "end": None, "loss": 0.2},
    ]
    lines_out, stats = reconstruct_lines(ref_lines, word_to_line, content_words)
    assert lines_out[0]["start_ms"] is None
    # words WERE sent for this line, they just came back untimed — so this is
    # the `other` bucket, not `empty_text`
    assert stats["n_lines_untimed_other"] == 1
    assert stats["n_lines_untimed_empty_text"] == 0
    assert stats["n_lines_timed"] == 0
    assert stats["n_words_missing_timing"] == 2


def test_estimate_duration_ms_reads_real_wav_header(tmp_path):
    wav_path = tmp_path / "test.wav"
    with wave.open(str(wav_path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(16000)
        wf.writeframes(b"\x00\x00" * 16000)  # exactly 1 second of silence

    assert estimate_duration_ms(wav_path) == 1000


def test_estimate_duration_ms_non_wav_file_returns_none(tmp_path):
    p = tmp_path / "not_a_wav.wav"
    p.write_text("this is not audio", encoding="utf-8")
    assert estimate_duration_ms(p) is None


def test_estimate_duration_ms_missing_file_returns_none(tmp_path):
    assert estimate_duration_ms(tmp_path / "does_not_exist.wav") is None
