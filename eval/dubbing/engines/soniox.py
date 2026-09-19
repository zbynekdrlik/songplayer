#!/usr/bin/env python3
"""soniox.py — Soniox TTS v2 (`tts-rt-v2`) voice-cloning DubEngine (cloud).

Runs from dev1 over HTTPS; no GPU. Verified against the Soniox Python SDK's own
REST surface (github.com/soniox/soniox-python, src/soniox/api/{voices,tts}.py):

  Voice cloning  — base `https://api.soniox.com/v1`, `Authorization: Bearer <key>`:
    POST   /voices                (multipart: form field `name`, file field `file`)
                                  -> {id, name, filename, created_at,
                                       models:[{model, status, error_type,
                                                error_message}]}
    POST   /voices/{id}/recompute ({"model": "tts-rt-v2"})  # prepare for a model
    GET    /voices/{id}           # poll a model entry's status
    DELETE /voices/{id}           # cleanup
    A model entry's `status` is one of not_computed | processing | ready | failed;
    it must be `ready` before the voice can be used with that model.

  Synthesis      — base `https://tts-rt.soniox.com`:
    POST   /tts  ({model, language, voice, audio_format, sample_rate, text})
                 -> raw audio bytes (WAV for audio_format="wav").

The API key is read from the `SONIOX_API_KEY` environment variable only. It is
never logged, never placed on a command line, and never written to a file.
"""

from __future__ import annotations

import logging
import os
import time

import requests

logger = logging.getLogger("dubbing_eval.soniox")

VOICES_API_BASE = "https://api.soniox.com/v1"
TTS_API_BASE = "https://tts-rt.soniox.com"

# Stock (predefined) voice catalogue: `GET /v1/tts-models` returns `models[]`,
# each with `languages[] {code,name}` and `voices[] {id,gender,description}`.
TTS_MODELS_URL = f"{VOICES_API_BASE}/tts-models"

MODEL = "tts-rt-v2"
AUDIO_FORMAT = "wav"
SAMPLE_RATE = 24000

# Voice-cloning readiness poll.
CLONE_POLL_INTERVAL_S = 3.0
CLONE_POLL_TIMEOUT_S = 300.0

# Synthesis request timeout.
SYNTH_TIMEOUT_S = 120.0


# Soniox is fronted by Cloudflare, which blocks the default `python-urllib`/
# generic User-Agent with `error code: 1010` ("banned by browser signature").
# `requests` normally passes, but set an explicit UA on every call so the ban
# never recurs (#175 round-2 finding). NOT a rate limit — no pacing needed.
_UA = "songplayer-dubbing-eval/1.0"
COMMON_HEADERS = {"User-Agent": _UA, "Accept": "*/*"}


def _api_key() -> str:
    key = os.environ.get("SONIOX_API_KEY")
    if not key:
        raise RuntimeError("SONIOX_API_KEY not set in the environment")
    return key


class SonioxEngine:
    """Soniox tts-rt-v2 voice-cloning engine."""

    name = "soniox"

    def __init__(self, model: str = MODEL) -> None:
        self._model = model
        self._auth = {"Authorization": f"Bearer {_api_key()}", **COMMON_HEADERS}

    def _model_status(self, voice: dict) -> str | None:
        for m in voice.get("models") or []:
            if m.get("model") == self._model:
                return m.get("status")
        return None

    def clone_voice(self, sample_wav_path: str) -> str:
        """Create a cloned voice from the reference clip and wait until it is
        `ready` for tts-rt-v2. Returns the voice id."""
        with open(sample_wav_path, "rb") as fh:
            r = requests.post(
                f"{VOICES_API_BASE}/voices",
                headers=self._auth,
                data={"name": f"dubeval-{int(time.time())}"},
                files={"file": (os.path.basename(sample_wav_path), fh, "audio/wav")},
                timeout=SYNTH_TIMEOUT_S,
            )
        if r.status_code >= 400:
            raise RuntimeError(f"soniox create voice {r.status_code}: {r.text[:400]}")
        voice = r.json()
        voice_id = voice.get("id")
        if not voice_id:
            raise RuntimeError(f"soniox create voice returned no id: {r.text[:400]}")
        logger.info("soniox voice created id=%s", voice_id)

        # Ensure the voice is being prepared for our TTS model.
        status = self._model_status(voice)
        if status is None or status == "not_computed":
            rc = requests.post(
                f"{VOICES_API_BASE}/voices/{voice_id}/recompute",
                headers=self._auth,
                json={"model": self._model},
                timeout=SYNTH_TIMEOUT_S,
            )
            if rc.status_code >= 400:
                raise RuntimeError(
                    f"soniox recompute {rc.status_code}: {rc.text[:400]}"
                )

        deadline = time.monotonic() + CLONE_POLL_TIMEOUT_S
        while True:
            g = requests.get(
                f"{VOICES_API_BASE}/voices/{voice_id}",
                headers=self._auth,
                timeout=SYNTH_TIMEOUT_S,
            )
            if g.status_code >= 400:
                raise RuntimeError(f"soniox get voice {g.status_code}: {g.text[:400]}")
            voice = g.json()
            status = self._model_status(voice)
            logger.info("soniox voice %s status=%s", voice_id, status)
            if status == "ready":
                return voice_id
            if status == "failed":
                raise RuntimeError(f"soniox voice cloning failed: {voice}")
            if time.monotonic() > deadline:
                raise RuntimeError(
                    f"soniox voice {voice_id} not ready after {CLONE_POLL_TIMEOUT_S}s "
                    f"(status={status})"
                )
            time.sleep(CLONE_POLL_INTERVAL_S)

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        r = requests.post(
            f"{TTS_API_BASE}/tts",
            headers={**self._auth, "Content-Type": "application/json"},
            json={
                "model": self._model,
                "language": lang,
                "voice": voice_ref,
                "audio_format": AUDIO_FORMAT,
                "sample_rate": SAMPLE_RATE,
                "text": text,
            },
            timeout=SYNTH_TIMEOUT_S,
        )
        if r.status_code >= 400:
            raise RuntimeError(f"soniox tts {r.status_code}: {r.text[:400]}")
        return r.content

    def delete_voice(self, voice_id: str) -> None:
        """Best-effort cleanup so the org's 20-voice cap is not exhausted."""
        try:
            requests.delete(
                f"{VOICES_API_BASE}/voices/{voice_id}",
                headers=self._auth,
                timeout=SYNTH_TIMEOUT_S,
            )
            logger.info("soniox voice %s deleted", voice_id)
        except requests.RequestException:
            logger.warning(
                "soniox voice %s delete failed (non-fatal)", voice_id, exc_info=True
            )


def list_tts_models() -> dict:
    """List Soniox TTS models with their languages + stock voices.

    Returns the parsed `GET /v1/tts-models` JSON. Raises on HTTP >= 400 with the
    exact status + body so the caller (round-2 stock-voice discovery) can record
    a 403/quota block verbatim in the report — the key is never in the body."""
    r = requests.get(
        TTS_MODELS_URL,
        headers={"Authorization": f"Bearer {_api_key()}", **COMMON_HEADERS},
        timeout=SYNTH_TIMEOUT_S,
    )
    if r.status_code >= 400:
        raise RuntimeError(f"soniox list models {r.status_code}: {r.text[:400]}")
    return r.json()


def stock_voices_for_language(lang: str = "sk", model: str = MODEL) -> list[dict]:
    """Return the stock voices of `model` when it advertises `lang`.

    Each voice is `{id, gender, description}`. Empty list if the model does not
    list the language. Raises (via `list_tts_models`) on an API error."""
    data = list_tts_models()
    for m in data.get("models") or []:
        if m.get("id") == model or m.get("model") == model or m.get("name") == model:
            langs = {
                (lc.get("code") or lc.get("language") or "").lower()
                for lc in (m.get("languages") or [])
            }
            if lang.lower() in langs or not langs:
                return list(m.get("voices") or [])
    return []


class SonioxStockEngine(SonioxEngine):
    """Soniox STOCK (predefined) voice — no cloning. `clone_voice` ignores the
    sample and returns the fixed stock voice name; synthesis is the inherited
    `/tts` call. Round 2 tests native `sk` stock voices as an alternative to the
    accent-leaking cross-lingual clone rejected in round 1."""

    name = "soniox_stock"

    def __init__(self, voice: str, model: str = MODEL) -> None:
        super().__init__(model=model)
        self._stock_voice = voice

    def clone_voice(self, sample_wav_path: str) -> str:
        return self._stock_voice


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    raise SystemExit(engine_cli(SonioxEngine, sys.argv[1:]))
