"""Pure unit tests for eval/dubbing/engines/gemini_tts._extract_pcm — pulling the
base64 PCM + sample rate out of a generateContent reply, and failing loud on a
blocked/empty response. No network."""

from __future__ import annotations

import base64

import pytest

from eval.dubbing.engines import gemini_tts


def _reply(data_bytes: bytes, mime: str = "audio/L16;rate=24000") -> dict:
    return {
        "candidates": [
            {
                "content": {
                    "parts": [
                        {
                            "inlineData": {
                                "mimeType": mime,
                                "data": base64.b64encode(data_bytes).decode(),
                            }
                        }
                    ]
                }
            }
        ]
    }


def test_extract_pcm_returns_bytes_and_rate():
    pcm, rate = gemini_tts._extract_pcm(_reply(b"\x01\x02\x03\x04"))
    assert pcm == b"\x01\x02\x03\x04"
    assert rate == 24000


def test_extract_pcm_reads_rate_from_mime():
    _, rate = gemini_tts._extract_pcm(_reply(b"\x00\x00", mime="audio/L16;rate=16000"))
    assert rate == 16000


def test_extract_pcm_snake_case_inline_data():
    payload = {
        "candidates": [
            {
                "content": {
                    "parts": [
                        {
                            "inline_data": {
                                "mime_type": "audio/L16;rate=24000",
                                "data": base64.b64encode(b"ab").decode(),
                            }
                        }
                    ]
                }
            }
        ]
    }
    pcm, rate = gemini_tts._extract_pcm(payload)
    assert pcm == b"ab" and rate == 24000


def test_extract_pcm_no_candidate_raises():
    with pytest.raises(RuntimeError, match="no candidate audio"):
        gemini_tts._extract_pcm({"candidates": [{"finishReason": "OTHER"}]})


def test_extract_pcm_no_inline_data_raises():
    payload = {"candidates": [{"content": {"parts": [{"text": "nope"}]}}]}
    with pytest.raises(RuntimeError, match="no inlineData"):
        gemini_tts._extract_pcm(payload)
