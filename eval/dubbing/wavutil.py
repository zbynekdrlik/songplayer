#!/usr/bin/env python3
"""wavutil.py — pure, stdlib-only WAV helpers shared by the round-2 engines.

Gemini TTS returns raw little-endian 16-bit PCM (`audio/L16`, 24 kHz mono); this
module wraps that into a canonical RIFF/WAV container so the rest of the harness
(`fit.py`/`run_listening_test.py`, ffmpeg, soundfile) reads it like any other
WAV. It is deliberately dependency-free (stdlib `wave`/`struct` only) so every
branch is unit-tested in CI's `eval-checks` job, exactly like `fit.py`.
"""

from __future__ import annotations

import io
import struct
import wave


def pcm_l16_to_wav(pcm: bytes, sample_rate: int = 24000, channels: int = 1) -> bytes:
    """Wrap little-endian signed 16-bit PCM samples in a RIFF/WAV container.

    `pcm` is raw sample bytes (no header). Returns the full WAV file bytes.
    Raises on an odd byte count (16-bit samples are 2 bytes) or a
    non-positive sample rate / channel count — a malformed engine response
    must fail loudly, never silently truncate.
    """
    if sample_rate <= 0:
        raise ValueError(f"sample_rate must be positive, got {sample_rate}")
    if channels <= 0:
        raise ValueError(f"channels must be positive, got {channels}")
    frame_bytes = 2 * channels
    if len(pcm) % frame_bytes != 0:
        raise ValueError(
            f"PCM length {len(pcm)} is not a whole number of "
            f"{frame_bytes}-byte frames ({channels}ch × 16-bit)"
        )
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(sample_rate)
        w.writeframes(pcm)
    return buf.getvalue()


def parse_l16_mime_rate(mime_type: str | None, default: int = 24000) -> int:
    """Extract the sample rate from a Gemini `audio/L16;rate=24000` mime string.

    Gemini stamps the PCM sample rate in the inlineData `mimeType`. Falls back to
    `default` when the mime is absent or carries no `rate=` parameter.
    """
    if not mime_type:
        return default
    for part in mime_type.split(";"):
        part = part.strip()
        if part.lower().startswith("rate="):
            try:
                rate = int(part.split("=", 1)[1])
            except ValueError:
                return default
            return rate if rate > 0 else default
    return default


def wav_num_frames(wav_bytes: bytes) -> int:
    """Return the frame (sample-per-channel) count of an in-memory WAV — used by
    tests to assert the wrapper preserved every sample."""
    with wave.open(io.BytesIO(wav_bytes), "rb") as w:
        return w.getnframes()


def silence_pcm_l16(ms: int, sample_rate: int = 24000, channels: int = 1) -> bytes:
    """Return `ms` milliseconds of 16-bit PCM silence — a tiny helper for tests
    and for padding a zero-length synthesis result."""
    frames = max(0, int(round(ms * sample_rate / 1000.0)))
    return struct.pack("<h", 0) * frames * channels
