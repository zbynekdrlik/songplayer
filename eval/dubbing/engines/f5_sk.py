#!/usr/bin/env python3
"""f5_sk.py — petercheben/F5_TTS_Slovak voice-cloning DubEngine (round 3, #175).

A Slovak fine-tune of SWivid/F5-TTS (HF `petercheben/F5_TTS_Slovak`, **GPL**),
trained on CV-Corpus-21 (40k samples, 11 epochs). F5-TTS is a flow-matching DiT
TTS that clones zero-shot from a reference clip + its transcript, decoded through a
vocos vocoder. ~0.3 B params — fits the dev2 RTX 5050 (8 GB). Runs ONLY on dev2.

The repo ships the raw fine-tune checkpoint + the F5 char vocab:
    model_30000.safetensors   # the SK DiT weights
    model_30000.txt           # the vocab (base F5 pinyin+latin set)

Recipe (f5-tts package):
    from f5_tts.api import F5TTS
    f5 = F5TTS(ckpt_file=<model_30000.safetensors>, vocab_file=<model_30000.txt>)
    wav, sr, _ = f5.infer(ref_file=<ref.wav>, ref_text=<ref transcript>,
                          gen_text=<sk sentence>, remove_silence=True)

Card note: "Numbers are not recognized, please use words instead." F5-TTS is
primarily an EN/ZH base, so the SK fine-tune is intended for a NATIVE-Slovak
reference (SK ref -> SK gen); a cross-lingual EN reference is the weaker path.

Install: `pip install f5-tts` + torch 2.8.0+cu128 (Blackwell). Heavy imports are
lazy inside methods; the pure `build_infer_kwargs` helper is unit-tested in CI.
"""

from __future__ import annotations

import logging
import os

logger = logging.getLogger("dubbing_eval.f5_sk")

CKPT_REPO = "petercheben/F5_TTS_Slovak"
CKPT_FILE = "model_30000.safetensors"
VOCAB_FILE = "model_30000.txt"


def build_infer_kwargs(
    ref_file: str, ref_text: str, gen_text: str, remove_silence: bool = True
) -> dict:
    """Assemble kwargs for `F5TTS.infer`, enforcing that F5 needs a reference clip
    AND its transcript. Pure + CI-tested. Raises loudly on any missing input."""
    if not gen_text or not gen_text.strip():
        raise ValueError("f5_sk: empty generation text")
    if not ref_file:
        raise ValueError("f5_sk: ref_file is required for zero-shot cloning")
    if not ref_text or not ref_text.strip():
        raise ValueError(
            "f5_sk: ref_text (transcription of the reference clip) is required"
        )
    return {
        "ref_file": ref_file,
        "ref_text": ref_text,
        "gen_text": gen_text,
        "remove_silence": remove_silence,
    }


class F5SkEngine:
    """petercheben/F5_TTS_Slovak zero-shot cloning engine (dev2 GPU, needs ref_text)."""

    name = "f5_sk"

    def __init__(self, ref_text: str | None = None, device: str | None = None) -> None:
        self._ref_text = (
            ref_text if ref_text is not None else os.environ.get("F5_REF_TEXT", "")
        )
        self._device = device or os.environ.get("F5_DEVICE", "cuda")
        self._model = None

    def _load(self):
        if self._model is None:
            from f5_tts.api import F5TTS
            from huggingface_hub import hf_hub_download

            ckpt = hf_hub_download(repo_id=CKPT_REPO, filename=CKPT_FILE)
            vocab = hf_hub_download(repo_id=CKPT_REPO, filename=VOCAB_FILE)
            logger.info("f5_sk loading ckpt=%s on %s", ckpt, self._device)
            self._model = F5TTS(ckpt_file=ckpt, vocab_file=vocab, device=self._device)
        return self._model

    def clone_voice(self, sample_wav_path: str) -> str:
        if not os.path.exists(sample_wav_path):
            raise RuntimeError(f"f5_sk: reference clip not found: {sample_wav_path}")
        return sample_wav_path

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        import io

        import numpy as np
        import soundfile as sf

        model = self._load()
        kwargs = build_infer_kwargs(voice_ref, self._ref_text, text)
        wav, sr, _ = model.infer(**kwargs)
        arr = np.asarray(wav, dtype=np.float32).reshape(-1)
        buf = io.BytesIO()
        sf.write(buf, arr, int(sr), format="WAV", subtype="PCM_16")
        return buf.getvalue()


def _factory() -> F5SkEngine:
    return F5SkEngine()


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(_factory, sys.argv[1:]))
