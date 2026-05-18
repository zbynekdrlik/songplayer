"""Unit tests for whisperx_replicate backend output parsing."""

import json
from pathlib import Path
from typing import Any

import pytest

from eval.lyrics.backends import whisperx_replicate as wx


def test_parse_replicate_output_to_lines() -> None:
    raw: dict[str, Any] = {
        "segments": [
            {
                "start": 1.0,
                "end": 2.5,
                "text": " Hello world ",
                "words": [
                    {"word": "Hello", "start": 1.0, "end": 1.5, "score": 0.99},
                    {"word": "world", "start": 1.6, "end": 2.5, "score": 0.97},
                ],
            },
            {
                "start": 3.0,
                "end": 4.2,
                "text": "Second line",
                "words": [],
            },
            {
                "start": 5.0,
                "end": 5.5,
                "text": "   ",
                "words": [],
            },
        ]
    }
    lines = wx.parse_output(raw)
    assert len(lines) == 2
    assert lines[0]["text"] == "Hello world"
    assert lines[0]["start_ms"] == 1000
    assert lines[0]["end_ms"] == 2500
    assert lines[0]["words"] == [
        {"text": "Hello", "start_ms": 1000, "end_ms": 1500, "confidence": 0.99},
        {"text": "world", "start_ms": 1600, "end_ms": 2500, "confidence": 0.97},
    ]
    assert lines[1]["text"] == "Second line"
    assert lines[1]["words"] is None  # no word-level info from this segment


def test_parse_replicate_output_skips_words_with_missing_timing() -> None:
    """Words with start=None or end=None must be excluded."""
    raw: dict[str, Any] = {
        "segments": [
            {
                "start": 0.0,
                "end": 2.0,
                "text": "partial",
                "words": [
                    {"word": "ok", "start": 0.0, "end": 0.5, "score": 0.9},
                    {"word": "bad", "start": None, "end": 1.0, "score": 0.5},
                    {"word": "alsobad", "start": 1.5, "end": None, "score": 0.5},
                ],
            }
        ]
    }
    lines = wx.parse_output(raw)
    assert len(lines) == 1
    assert lines[0]["words"] == [
        {"text": "ok", "start_ms": 0, "end_ms": 500, "confidence": 0.9}
    ]


def test_parse_replicate_output_raises_when_segments_missing() -> None:
    with pytest.raises(ValueError, match="missing segments"):
        wx.parse_output({})


def test_build_predict_input_shape() -> None:
    body = wx.build_predict_input("https://example.com/audio.wav", "en")
    assert body == {
        "audio_file": "https://example.com/audio.wav",
        "language": "en",
        "align_output": True,
        "diarization": False,
        "batch_size": 32,
    }


def test_emit_result_writes_schema_valid_json(tmp_path: Path) -> None:
    out = tmp_path / "result.json"
    wx.emit_result(
        out_path=out,
        wav_path="C:\\test.wav",
        duration_ms=10000,
        lines=[
            {
                "text": "hi",
                "start_ms": 0,
                "end_ms": 500,
                "words": None,
            }
        ],
        raw_confidence=0.9,
        metadata={"model": "whisperx", "elapsed_s": 12.3},
    )
    data = json.loads(out.read_text(encoding="utf-8"))
    assert data["backend_id"] == "whisperx-large-v3"
    assert data["backend_revision"] == 1
    assert data["wav_path"] == "C:\\test.wav"
    assert data["duration_ms"] == 10000
    assert len(data["lines"]) == 1
    assert data["raw_confidence"] == 0.9
    assert data["metadata"]["model"] == "whisperx"


def test_estimate_duration_ms_returns_max_end() -> None:
    lines = [
        {"text": "a", "start_ms": 0, "end_ms": 500, "words": None},
        {"text": "b", "start_ms": 1000, "end_ms": 3500, "words": None},
        {"text": "c", "start_ms": 2000, "end_ms": 2200, "words": None},
    ]
    assert wx.estimate_duration_ms(lines) == 3500


def test_estimate_duration_ms_returns_zero_on_empty() -> None:
    assert wx.estimate_duration_ms([]) == 0


def test_pinned_version_hash_matches_rust_constant() -> None:
    """Mirror the constant in crates/sp-server/src/lyrics/whisperx_replicate.rs.

    If this fails, the Rust impl bumped the pin without updating the eval
    backend — bring them back in sync.
    """
    expected = "84d2ad2d6194fe98a17d2b60bef1c7f910c46b2f6fd38996ca457afd9c8abfcb"
    assert wx.WHISPERX_VERSION == expected
