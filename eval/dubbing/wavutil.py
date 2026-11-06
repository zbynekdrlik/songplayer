#!/usr/bin/env python3
"""wavutil.py — pure, stdlib-only WAV helpers for the dubbing eval engines.

Gemini Live Translate returns raw little-endian 16-bit PCM (24 kHz mono); this
module wraps that into a canonical RIFF/WAV container so ffmpeg and soundfile
read it like any other WAV. It is deliberately dependency-free (stdlib `wave`
only) so every branch is unit-tested in CI's `eval-checks` job.
"""

from __future__ import annotations

import io
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


def wav_num_frames(wav_bytes: bytes) -> int:
    """Return the frame (sample-per-channel) count of an in-memory WAV — used by
    tests to assert the wrapper preserved every sample."""
    with wave.open(io.BytesIO(wav_bytes), "rb") as w:
        return w.getnframes()
