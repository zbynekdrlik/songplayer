#!/usr/bin/env python3
"""voice_band_measure.py — measure each catalogue voice's f0 band (#184 round E2).

A dev1-only harness (NOT wired into CI): render one fixed 120-s English slice
(`~/.claude/work-products/songplayer/dubbing-test/seg.wav`) through ONE pinned
Gemini Live Translate session per catalogue voice and print the voiced 5-s f0
median band (p10 / median / p90). The printed medians are pasted into
`scripts/dub_worker.py::VOICE_F0_BAND` so the round-E2 chunk-0 seed check is
grounded in MEASURED numbers, not guesses.

Reuses the reference streaming pattern of `engines/gemini_live_translate.py` and
`scripts/dub_worker.py::_translate_pcm` (pin the voice via `speech_config`,
stream 16 kHz mono s16le at 100 ms chunks, drain, receive 24 kHz PCM). The f0
per-window median is `scripts/dub_voice_check.window_medians` (the SAME helper the
prod guard uses). The Gemini key is read INSIDE python from the box settings
endpoint (`GET .../api/v1/settings`, `gemini_api_key` csv, first entry) and is
NEVER printed, logged, put on a command line, or committed.

Run once, as a MODULE from the repo root, in the eval venv (has google-genai +
numpy + soundfile; add librosa for a sharper f0 read) — the module form puts the
repo root on `sys.path` so the `eval.dubbing.voices` import resolves:
    ~/.claude/work-products/songplayer/dubbing-test/.venv-live/bin/python \
        -m eval.dubbing.voice_band_measure --voices Charon,Orus
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import subprocess
import sys
import time
import urllib.request

import numpy as np

# The six catalogue voices (mirrors sp-ui settings + eval/dubbing/voices.py).
from eval.dubbing.voices import GEMINI_VOICES

MODEL = "gemini-3.5-live-translate-preview"
INPUT_SR = 16000
OUTPUT_SR = 24000
CHUNK_BYTES = 3200  # 100 ms @ 16 kHz s16le mono
TARGET_LANG = "sk"
DRAIN_S = 27.0
WINDOW_S = 5.0
DEFAULT_SEG = os.path.expanduser(
    "~/.claude/work-products/songplayer/dubbing-test/seg.wav"
)
SETTINGS_URL = "http://10.77.9.201:8920/api/v1/settings"


def _load_dub_voice_check():
    """Import `scripts/dub_voice_check.py` by path (its `window_medians` is the
    SAME f0-per-window helper the prod guard + CI use)."""
    import importlib.util

    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    path = os.path.join(root, "scripts", "dub_voice_check.py")
    spec = importlib.util.spec_from_file_location("dub_voice_check", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _api_key() -> str:
    """Read the Gemini key INSIDE python from the box settings endpoint (csv,
    first entry). NEVER printed / logged / on a command line / committed."""
    with urllib.request.urlopen(SETTINGS_URL, timeout=10) as r:
        settings = json.load(r)
    raw = (settings.get("gemini_api_key") or "").strip()
    key = raw.split(",")[0].strip()
    if not key:
        raise RuntimeError("no gemini_api_key in the box settings endpoint")
    return key


def _seg_pcm16k(seg_path: str) -> bytes:
    """ffmpeg-resample the 120-s seg.wav to 16 kHz mono s16le PCM (Live input)."""
    args = [
        os.environ.get("DUB_FFMPEG", "ffmpeg"),
        "-hide_banner",
        "-nostdin",
        "-y",
        "-i",
        seg_path,
        "-ac",
        "1",
        "-ar",
        str(INPUT_SR),
        "-f",
        "s16le",
        "-acodec",
        "pcm_s16le",
        "-",
    ]
    r = subprocess.run(args, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(f"ffmpeg resample failed: {r.stderr[-400:]!r}")
    return r.stdout


async def _translate(pcm: bytes, voice: str, key: str) -> bytes:
    """Stream `pcm` into the pinned Live Translate session; return 24 kHz PCM."""
    import google.genai as genai
    from google.genai import types

    client = genai.Client(api_key=key, http_options={"api_version": "v1alpha"})
    config = types.LiveConnectConfig(
        response_modalities=["AUDIO"],
        translation_config=types.TranslationConfig(
            target_language_code=TARGET_LANG, echo_target_language=True
        ),
        speech_config=types.SpeechConfig(
            voice_config=types.VoiceConfig(
                prebuilt_voice_config=types.PrebuiltVoiceConfig(voice_name=voice)
            )
        ),
    )
    out = bytearray()
    deadline = time.monotonic() + (len(pcm) / 2 / INPUT_SR) + DRAIN_S
    async with client.aio.live.connect(model=MODEL, config=config) as session:

        async def send() -> None:
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
            if (
                sc is not None
                and getattr(sc, "turn_complete", False)
                and send_task.done()
            ):
                break
        if not send_task.done():
            send_task.cancel()
            try:
                await send_task
            except asyncio.CancelledError:
                pass  # airuleset:script-ok clean teardown of our own sender
    return bytes(out)


def measure_voice(pcm16k: bytes, voice: str, key: str, dvc) -> dict:
    """Render `voice` and return {voice, windows, voiced, p10, median, p90}.

    The per-window f0 uses `use_librosa=False` — the dependency-free
    autocorrelation path — on purpose: the PROD guard's chunk-0 seed median is
    computed the same way (`dub_worker._pcm_window_medians` / `_wav_window_medians`
    both pass `use_librosa=False`, since the child stays numpy-only). Measuring the
    band with pyin would put `VOICE_F0_BAND` in a different unit than the runtime
    seed median and mis-seed real chunks, so we match the runtime estimator here."""
    out_pcm = asyncio.run(_translate(pcm16k, voice, key))
    samples = np.frombuffer(out_pcm, dtype="<i2").astype("float32") / 32768.0
    medians = dvc.window_medians(samples, OUTPUT_SR, WINDOW_S, use_librosa=False)
    voiced = [m for m in medians if m > 0]
    if not voiced:
        return {"voice": voice, "windows": len(medians), "voiced": 0}
    arr = np.asarray(voiced, dtype=np.float64)
    return {
        "voice": voice,
        "windows": len(medians),
        "voiced": len(voiced),
        "p10": float(np.percentile(arr, 10)),
        "median": float(np.median(arr)),
        "p90": float(np.percentile(arr, 90)),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seg", default=DEFAULT_SEG, help="120-s input slice")
    parser.add_argument(
        "--voices",
        default=",".join(v[0] for v in GEMINI_VOICES),
        help="comma-separated voice names (default: all six catalogue voices)",
    )
    args = parser.parse_args()

    dvc = _load_dub_voice_check()
    key = _api_key()
    pcm16k = _seg_pcm16k(args.seg)
    voices = [v.strip() for v in args.voices.split(",") if v.strip()]

    for voice in voices:
        try:
            r = measure_voice(pcm16k, voice, key, dvc)
        except Exception as e:  # a per-voice failure must not lose the others
            # Defensive: never let the key leak into a logged error, even if an
            # SDK exception embedded it (mirrors dub_worker.main's redaction).
            msg = str(e).replace(key, "<redacted>") if key else str(e)
            print(f"{voice}: ERROR {type(e).__name__}: {msg}", file=sys.stderr)
            continue
        if not r.get("voiced"):
            print(f"{voice}: no voiced windows ({r['windows']} total)")
            continue
        # Paste (round(p10), round(p90)) into VOICE_F0_BAND; median is the check.
        print(
            f"{voice}: band ({r['p10']:.1f}, {r['p90']:.1f}) Hz  "
            f"median {r['median']:.1f}  voiced {r['voiced']}/{r['windows']}"
        )


if __name__ == "__main__":
    main()
