"""Pure unit tests for eval/dubbing/wavutil.py — the stdlib-only PCM/WAV helpers
used by the Gemini Live Translate engine. No audio libs, no network."""

from __future__ import annotations

import struct

import pytest

from eval.dubbing import wavutil


def _pcm(n_samples: int) -> bytes:
    # A ramp so the round-trip is not all-zero (a real signal).
    return b"".join(struct.pack("<h", (i % 1000) - 500) for i in range(n_samples))


def test_wrap_preserves_every_sample_and_rate():
    pcm = _pcm(2400)  # 100 ms @ 24 kHz mono
    wav = wavutil.pcm_l16_to_wav(pcm, sample_rate=24000, channels=1)
    assert wav[:4] == b"RIFF"
    assert wav[8:12] == b"WAVE"
    assert wavutil.wav_num_frames(wav) == 2400


def test_wrap_stereo_frame_count():
    # 1000 stereo frames = 2000 int16 samples.
    pcm = _pcm(2000)
    wav = wavutil.pcm_l16_to_wav(pcm, sample_rate=48000, channels=2)
    assert wavutil.wav_num_frames(wav) == 1000


def test_odd_byte_count_raises():
    with pytest.raises(ValueError, match="whole number"):
        wavutil.pcm_l16_to_wav(b"\x01\x02\x03", sample_rate=24000, channels=1)


def test_stereo_half_frame_raises():
    # 2 int16 samples = 1 stereo frame exactly; 3 samples (6 bytes) is 1.5 frames.
    with pytest.raises(ValueError, match="whole number"):
        wavutil.pcm_l16_to_wav(_pcm(3), sample_rate=24000, channels=2)


def test_non_positive_rate_and_channels_raise():
    with pytest.raises(ValueError, match="sample_rate"):
        wavutil.pcm_l16_to_wav(b"", sample_rate=0, channels=1)
    with pytest.raises(ValueError, match="channels"):
        wavutil.pcm_l16_to_wav(b"", sample_rate=24000, channels=0)


def test_empty_pcm_is_a_valid_zero_frame_wav():
    wav = wavutil.pcm_l16_to_wav(b"", sample_rate=24000, channels=1)
    assert wavutil.wav_num_frames(wav) == 0


def test_silence_frame_count_matches_duration():
    # 50 ms of silence @ 24 kHz = 1200 frames = 2400 bytes.
    pcm = struct.pack("<h", 0) * 1200
    wav = wavutil.pcm_l16_to_wav(pcm, sample_rate=24000, channels=1)
    assert wavutil.wav_num_frames(wav) == 1200
