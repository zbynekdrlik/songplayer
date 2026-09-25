#!/usr/bin/env python3
"""gemini_live_translate.py — Gemini Live API voice-to-voice translation (audio→audio).

The round-2 TOP audio-to-audio candidate (#175): stream the ORIGINAL English
dabing audio into `gemini-3.5-live-translate-preview` and receive translated
Slovak SPEECH that keeps the speaker's own prosody, pacing and sentence
boundaries — the property per-sentence TTS cannot reproduce (the owner's core
complaint was that per-sentence dubs read "ako rozprávanie príbehu", losing the
preacher's intensity and chopping his phrasing).

Unlike the TTS engines this is NOT a `DubEngine` (there is no text or voice
clone): its interface is audio-in → audio-out. `translate_pcm16k(pcm)` streams
16 kHz mono s16le PCM in and returns the concatenated 24 kHz PCM output plus the
output transcript; `trim_af()` is the pure ffmpeg trailing-silence filter (the
Live session streams silence until closed, so the tail must be trimmed).

Config (Gemini Live docs): `LiveConnectConfig(response_modalities=["AUDIO"],
translation_config=TranslationConfig(target_language_code="sk",
echo_target_language=True), output_audio_transcription=AudioTranscriptionConfig())`.
Input 16 kHz s16le mono; output 24 kHz s16le mono. Key from `GEMINI_API_KEY` env
only (never logged). NOTE: `gemini-3.8-live` with a translate *instruction* stops
after ~1.4 s (turn-taking) and is unusable for continuous dubbing — use the
dedicated `-live-translate-` model.

Heavy import (`google.genai`) is lazy so this module imports under ruff/CI.
"""

from __future__ import annotations

import asyncio
import os
import time

from eval.dubbing import wavutil

MODEL = "gemini-3.5-live-translate-preview"
INPUT_SR = 16000
OUTPUT_SR = 24000
CHUNK_BYTES = 3200  # 100 ms @ 16 kHz s16le mono


def trim_af(threshold_db: int = -45, min_silence_s: float = 0.2) -> str:
    """Pure: the ffmpeg `-af` filter that strips trailing silence (reverse →
    remove leading silence → reverse). Unit-tested; no I/O."""
    return (
        f"areverse,silenceremove=start_periods=1:start_threshold={threshold_db}dB:"
        f"start_duration={min_silence_s},areverse"
    )


def drain_deadline_s(input_pcm_bytes: int, drain_s: float = 27.0) -> float:
    """Pure: how long to keep the receive stream open — the input's real-time
    duration plus a drain window for the model to finish translating."""
    input_s = input_pcm_bytes / 2 / INPUT_SR  # s16le mono
    return round(input_s + drain_s, 2)


def _api_key() -> str:
    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise RuntimeError("GEMINI_API_KEY not set in the environment")
    return key


class GeminiLiveTranslateEngine:
    """Audio→audio Slovak translation over the Gemini Live API."""

    name = "gemini_live_translate"

    def __init__(self, model: str = MODEL, target_lang: str = "sk") -> None:
        self._model = model
        self._target = target_lang
        self._key = _api_key()

    def translate_pcm16k(self, pcm: bytes) -> tuple[bytes, str, float]:
        """Stream 16 kHz mono s16le `pcm` in; return (out_pcm_24k, transcript,
        elapsed_s). Blocking wrapper around the async Live session."""
        return asyncio.run(self._run(pcm))

    async def _run(self, pcm: bytes) -> tuple[bytes, str, float]:
        import google.genai as genai
        from google.genai import types

        client = genai.Client(
            api_key=self._key, http_options={"api_version": "v1alpha"}
        )
        config = types.LiveConnectConfig(
            response_modalities=["AUDIO"],
            output_audio_transcription=types.AudioTranscriptionConfig(),
            translation_config=types.TranslationConfig(
                target_language_code=self._target,
                echo_target_language=True,
            ),
        )
        out = bytearray()
        transcript: list[str] = []
        t0 = time.monotonic()
        deadline = t0 + drain_deadline_s(len(pcm))
        async with client.aio.live.connect(model=self._model, config=config) as session:

            async def send():
                for i in range(0, len(pcm), CHUNK_BYTES):
                    await session.send_realtime_input(
                        audio=types.Blob(
                            data=bytes(pcm[i : i + CHUNK_BYTES]),
                            mime_type=f"audio/pcm;rate={INPUT_SR}",
                        )
                    )
                    await asyncio.sleep(0.1)
                await session.send_realtime_input(audio_stream_end=True)

            send_task = asyncio.create_task(send())
            gen = session.receive().__aiter__()
            while time.monotonic() < deadline:
                try:
                    resp = await asyncio.wait_for(gen.__anext__(), timeout=5.0)
                except asyncio.TimeoutError:
                    continue
                except StopAsyncIteration:
                    break
                data = getattr(resp, "data", None)
                if data:
                    out.extend(data)
                sc = getattr(resp, "server_content", None)
                if sc is not None:
                    oat = getattr(sc, "output_transcription", None)
                    if oat is not None and getattr(oat, "text", None):
                        transcript.append(oat.text)
                    if getattr(sc, "turn_complete", False) and send_task.done():
                        break
            if not send_task.done():
                send_task.cancel()
                try:
                    await send_task
                except asyncio.CancelledError:
                    # airuleset:script-ok awaiting our own just-cancelled sender
                    # is the standard clean-teardown idiom; nothing to log.
                    pass
        return bytes(out), "".join(transcript), round(time.monotonic() - t0, 2)

    def to_wav(self, out_pcm: bytes) -> bytes:
        """Wrap the raw 24 kHz output PCM as WAV (untrimmed)."""
        return wavutil.pcm_l16_to_wav(out_pcm, sample_rate=OUTPUT_SR, channels=1)
