#!/usr/bin/env python3
"""omnivoice.py — k2-fsa/OmniVoice zero-shot voice-cloning DubEngine (round 3, #175).

OmniVoice is a massively-multilingual (600+ languages incl. `sk`) zero-shot TTS
built on a diffusion-language-model architecture over a Qwen3-0.6B backbone
(HF `k2-fsa/OmniVoice`). Small enough for the dev2 RTX 5050 (8 GB) in fp16.
Runs ONLY on dev2 — NEVER on win-resolume.

LICENSE CAVEAT (card §License): the *code* is Apache-2.0 but the *pre-trained
model* is **CC-BY-NC** (Emilia training-data constraint) — non-commercial. Kept in
the eval as an open-weight data point, but it cannot be the production winner as-is.

Cloning is zero-shot but, unlike XTTS/Chatterbox, OmniVoice needs the reference
clip's TRANSCRIPTION (`ref_text`) alongside the audio:

    from omnivoice import OmniVoice
    model = OmniVoice.from_pretrained("k2-fsa/OmniVoice", device_map="cuda:0",
                                      dtype=torch.float16)
    audio = model.generate(text=<sk>, ref_audio=<ref.wav>, ref_text=<ref transcript>)
    # audio is a list of np.ndarray (T,) at 24 kHz

Install: `pip install omnivoice` + torch 2.8.0+cu128 (Blackwell — see dubbing-eval.md).
Heavy imports are lazy inside methods; the pure `build_generate_kwargs` helper (which
enforces the ref_text requirement) is unit-tested in CI without the model.
"""

from __future__ import annotations

import logging
import os

from eval.dubbing import wavutil

logger = logging.getLogger("dubbing_eval.omnivoice")

MODEL_ID = "k2-fsa/OmniVoice"
OUTPUT_SR = 24000


def build_generate_kwargs(text: str, ref_audio: str, ref_text: str) -> dict:
    """Assemble the kwargs for `OmniVoice.generate`, enforcing OmniVoice's rule
    that a reference clip is useless without its transcription. Pure + CI-tested.
    Raises loudly on a missing gen text, reference clip, or reference transcript."""
    if not text or not text.strip():
        raise ValueError("omnivoice: empty generation text")
    if not ref_audio:
        raise ValueError("omnivoice: ref_audio is required for zero-shot cloning")
    if not ref_text or not ref_text.strip():
        raise ValueError(
            "omnivoice: ref_text (transcription of the reference clip) is required"
        )
    return {"text": text, "ref_audio": ref_audio, "ref_text": ref_text}


class OmniVoiceEngine:
    """k2-fsa/OmniVoice zero-shot cloning engine (dev2 GPU, needs ref_text)."""

    name = "omnivoice"

    def __init__(self, ref_text: str | None = None, device: str | None = None) -> None:
        self._ref_text = (
            ref_text
            if ref_text is not None
            else os.environ.get("OMNIVOICE_REF_TEXT", "")
        )
        self._device = device or os.environ.get("OMNIVOICE_DEVICE", "cuda")
        self._model = None

    def _load(self):
        if self._model is None:
            import torch
            from omnivoice import OmniVoice

            # Bare "cuda" -> "cuda:0"; an explicit "cuda:N" or "cpu" is used as-is
            # (avoid a malformed "cuda:0:0").
            device_map = "cuda:0" if self._device == "cuda" else self._device
            dtype = torch.float16 if self._device == "cuda" else torch.float32
            logger.info("omnivoice loading %s on %s", MODEL_ID, device_map)
            self._model = OmniVoice.from_pretrained(
                MODEL_ID, device_map=device_map, dtype=dtype
            )
        return self._model

    def clone_voice(self, sample_wav_path: str) -> str:
        # Zero-shot: the reference clip is the voice reference (audio + ref_text).
        if not os.path.exists(sample_wav_path):
            raise RuntimeError(
                f"omnivoice: reference clip not found: {sample_wav_path}"
            )
        return sample_wav_path

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        import numpy as np

        model = self._load()
        kwargs = build_generate_kwargs(text, voice_ref, self._ref_text)
        audio = model.generate(**kwargs)
        arr = audio[0] if isinstance(audio, (list, tuple)) else audio
        arr = np.asarray(arr, dtype=np.float32).reshape(-1)
        pcm = (np.clip(arr, -1.0, 1.0) * 32767.0).astype("<i2").tobytes()
        return wavutil.pcm_l16_to_wav(pcm, sample_rate=OUTPUT_SR, channels=1)


def _factory() -> OmniVoiceEngine:
    return OmniVoiceEngine()


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(_factory, sys.argv[1:]))
