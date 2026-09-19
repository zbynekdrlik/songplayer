#!/usr/bin/env python3
"""gemini_tts.py — Gemini TTS prebuilt-voice DubEngine (cloud, native Slovak).

Round 2 (#175): the owner rejected round 1's CLONED English speaker pushed into
Slovak ("slovenčina bez mäkčeňov" — source-accent transfer). Gemini TTS speaks
Slovak with a NATIVE prebuilt voice, so there is no cross-lingual accent to leak;
there is NO voice cloning here — `clone_voice` ignores the sample and returns the
prebuilt voice name.

REST surface (Gemini API `generateContent`, AUDIO modality):

    POST https://generativelanguage.googleapis.com/v1beta/models/<model>:generateContent
    header:  x-goog-api-key: <key>
    body:    {"contents":[{"parts":[{"text": "<style>: <sentence>"}]}],
              "generationConfig":{
                 "responseModalities":["AUDIO"],
                 "speechConfig":{"voiceConfig":{
                     "prebuiltVoiceConfig":{"voiceName":"<voice>"}}}}}
    reply:   candidates[0].content.parts[0].inlineData
                 {mimeType:"audio/L16;rate=24000", data:"<base64 PCM>"}

The PCM is wrapped to WAV via `wavutil.pcm_l16_to_wav`. Model under test (round-2
newest-only ruling): `gemini-3.1-flash-tts-preview` (2.5-pro dropped as superseded).

The API key is read from the `GEMINI_API_KEY` environment variable only — never
logged, never on a command line, never written to a file. On the box it is the
first entry of the settings `gemini_api_key`, injected into the env by the
runner.
"""

from __future__ import annotations

import base64
import logging
import os

import requests

from eval.dubbing import wavutil
from eval.dubbing.voices import GEMINI_STYLE

logger = logging.getLogger("dubbing_eval.gemini_tts")

API_ROOT = "https://generativelanguage.googleapis.com/v1beta/models"
# Newest-only (owner ruling): gemini-2.5-pro-preview-tts is dropped as superseded.
DEFAULT_MODEL = "gemini-3.1-flash-tts-preview"
SYNTH_TIMEOUT_S = 180.0
DEFAULT_SAMPLE_RATE = 24000


def _api_key() -> str:
    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise RuntimeError("GEMINI_API_KEY not set in the environment")
    return key


class GeminiTTSEngine:
    """Gemini TTS prebuilt-voice engine (no cloning; native Slovak)."""

    name = "gemini"

    def __init__(
        self,
        voice: str = "Kore",
        model: str = DEFAULT_MODEL,
        style: str = GEMINI_STYLE,
    ) -> None:
        self._voice = voice
        self._model = model
        self._style = style
        self._key = _api_key()

    def clone_voice(self, sample_wav_path: str) -> str:
        # No cloning: the prebuilt voice name IS the voice reference. The sample
        # is intentionally ignored — round 2 tests native voices, not a clone.
        return self._voice

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        prompt = f"{self._style} {text}" if self._style else text
        body = {
            "contents": [{"parts": [{"text": prompt}]}],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice_ref}}
                },
            },
        }
        r = requests.post(
            f"{API_ROOT}/{self._model}:generateContent",
            headers={
                "x-goog-api-key": self._key,
                "Content-Type": "application/json",
            },
            json=body,
            timeout=SYNTH_TIMEOUT_S,
        )
        if r.status_code >= 400:
            # r.text never contains the key (it is header-only).
            raise RuntimeError(f"gemini tts {r.status_code}: {r.text[:400]}")
        pcm, rate = _extract_pcm(r.json())
        return wavutil.pcm_l16_to_wav(pcm, sample_rate=rate, channels=1)


def _extract_pcm(payload: dict) -> tuple[bytes, int]:
    """Pull the base64 PCM + sample rate out of a generateContent reply. Fails
    loudly if the expected inlineData is missing (a blocked/empty response)."""
    try:
        parts = payload["candidates"][0]["content"]["parts"]
    except (KeyError, IndexError, TypeError) as e:
        raise RuntimeError(f"gemini tts: no candidate audio in reply: {payload}") from e
    for part in parts:
        inline = part.get("inlineData") or part.get("inline_data")
        if inline and inline.get("data"):
            pcm = base64.b64decode(inline["data"])
            rate = wavutil.parse_l16_mime_rate(
                inline.get("mimeType") or inline.get("mime_type"),
                default=DEFAULT_SAMPLE_RATE,
            )
            return pcm, rate
    raise RuntimeError(f"gemini tts: reply had no inlineData audio part: {payload}")


def _factory() -> GeminiTTSEngine:
    voice = os.environ.get("GEMINI_TTS_VOICE", "Kore")
    model = os.environ.get("GEMINI_TTS_MODEL", DEFAULT_MODEL)
    return GeminiTTSEngine(voice=voice, model=model)


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(_factory, sys.argv[1:]))
