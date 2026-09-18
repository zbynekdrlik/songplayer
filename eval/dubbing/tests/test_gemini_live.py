"""Pure unit tests for the Gemini Live Translate engine's dependency-free helpers
(the ffmpeg trailing-silence filter and the drain-deadline math). No network,
no google-genai import."""

from __future__ import annotations

from eval.dubbing.engines import gemini_live_translate as glt


def test_trim_af_is_a_reverse_silenceremove_reverse_chain():
    af = glt.trim_af()
    assert af.startswith("areverse,silenceremove=")
    assert af.endswith(",areverse")
    assert "start_threshold=-45dB" in af
    assert "start_duration=0.2" in af


def test_trim_af_parametrized():
    af = glt.trim_af(threshold_db=-50, min_silence_s=0.5)
    assert "start_threshold=-50dB" in af
    assert "start_duration=0.5" in af


def test_drain_deadline_adds_realtime_input_plus_drain():
    # 16 kHz s16le mono: 1 s == 32000 bytes.
    one_second = 32000
    assert glt.drain_deadline_s(one_second, drain_s=27.0) == 28.0
    # An 18.6 s span.
    assert glt.drain_deadline_s(int(18.6 * one_second), drain_s=27.0) == 45.6


def test_drain_deadline_zero_input_is_just_drain():
    assert glt.drain_deadline_s(0, drain_s=10.0) == 10.0


def test_constants_match_live_api_contract():
    assert glt.INPUT_SR == 16000
    assert glt.OUTPUT_SR == 24000
    assert glt.MODEL == "gemini-3.5-live-translate-preview"
    # 100 ms input chunk at 16 kHz s16le mono.
    assert glt.CHUNK_BYTES == 3200
