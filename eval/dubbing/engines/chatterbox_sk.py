#!/usr/bin/env python3
"""chatterbox_sk.py — pekiskol/chatterbox-tts-slovak voice-cloning DubEngine
(round 3, #175). Runs on the dev2 GPU (RTX 5050, 8 GB) — NEVER on win-resolume.

A Slovak fine-tune of Resemble AI's Chatterbox Multilingual TTS, published as
**drop-in T3 replacement weights** (`t3_sk_v2.2.safetensors`, ~2 GB) under a real
**MIT license — code AND weights** (per the author's card, the only SOTA-ish open
SK TTS that is commercially usable; XTTS-v2 / F5-TTS / Fish are all non-commercial).
This directly refutes round 1's "Chatterbox has no Slovak" — the base 23-language
checkpoint indeed excludes `sk`, but this fine-tune adds it.

Recipe (HF card `pekiskol/chatterbox-tts-slovak`):
    1. load the base `ChatterboxMultilingualTTS.from_pretrained(device)`,
    2. download `t3_sk_v2.2.safetensors`, reconcile its text-vocab rows against
       the base model's (`plan_vocab_reconcile`), then `t3.load_state_dict(strict)`,
    3. register `sk` in the build's SUPPORTED_LANGUAGES if missing,
    4. zero-shot clone: `generate(text, language_id="sk", audio_prompt_path=ref)`.

Heavy imports (torch / chatterbox / huggingface_hub / safetensors) are lazy inside
methods so this module imports cleanly under ruff and on dev1 (which never runs it).
The pure helpers `plan_vocab_reconcile` and `register_language` are unit-tested in
CI (no torch needed).
"""

from __future__ import annotations

import logging
import os

logger = logging.getLogger("dubbing_eval.chatterbox_sk")

SK_WEIGHTS_REPO = "pekiskol/chatterbox-tts-slovak"
SK_WEIGHTS_FILE = "t3_sk_v2.2.safetensors"


def plan_vocab_reconcile(src_vocab: int, target_vocab: int) -> tuple[str, int]:
    """Decide how to reconcile the SK fine-tune's text-embedding vocab size with
    the base model's, WITHOUT touching any tensor (pure, CI-testable).

    Returns ``("trim", n)`` to drop the last ``n`` rows of the fine-tune's
    ``text_emb`` / ``text_head`` (src bigger than base), ``("pad", n)`` to append
    ``n`` mean-filled rows (src smaller), or ``("ok", 0)`` when they match.
    """
    if src_vocab <= 0 or target_vocab <= 0:
        raise ValueError(
            f"vocab sizes must be positive, got src={src_vocab} target={target_vocab}"
        )
    if src_vocab > target_vocab:
        return ("trim", src_vocab - target_vocab)
    if src_vocab < target_vocab:
        return ("pad", target_vocab - src_vocab)
    return ("ok", 0)


def register_language(supported, code: str = "sk", name: str = "Slovak"):
    """Register ``code`` in a SUPPORTED_LANGUAGES container (dict or list) if the
    build shipped without it. Returns True if it added the code (pure, CI-testable).
    The base Chatterbox build's 23-language map omits `sk`, but the fine-tune
    speaks Slovak, so the language id must be accepted."""
    if isinstance(supported, dict):
        if code not in supported:
            supported[code] = name
            return True
        return False
    if isinstance(supported, list):
        if code not in supported:
            supported.append(code)
            return True
        return False
    return False


class ChatterboxSkEngine:
    """pekiskol/chatterbox-tts-slovak zero-shot voice-cloning engine (dev2 GPU)."""

    name = "chatterbox_sk"

    def __init__(self, device: str | None = None) -> None:
        self._device = device or os.environ.get("CHATTERBOX_DEVICE", "cuda")
        self._model = None

    def _load(self):
        if self._model is None:
            import torch
            from chatterbox.mtl_tts import ChatterboxMultilingualTTS
            from huggingface_hub import hf_hub_download
            from safetensors.torch import load_file

            logger.info("chatterbox_sk loading base model on %s", self._device)
            model = ChatterboxMultilingualTTS.from_pretrained(device=self._device)
            weights = hf_hub_download(repo_id=SK_WEIGHTS_REPO, filename=SK_WEIGHTS_FILE)
            state = load_file(weights, device="cpu")
            target_vocab = int(model.t3.text_emb.weight.shape[0])
            src_vocab = int(state["text_emb.weight"].shape[0])
            op, n = plan_vocab_reconcile(src_vocab, target_vocab)
            logger.info("chatterbox_sk vocab reconcile: %s %d rows", op, n)
            if op == "trim":
                state["text_emb.weight"] = state["text_emb.weight"][:target_vocab, :]
                state["text_head.weight"] = state["text_head.weight"][:target_vocab, :]
            elif op == "pad":
                emb_pad = state["text_emb.weight"].mean(0, keepdim=True).repeat(n, 1)
                head_pad = state["text_head.weight"].mean(0, keepdim=True).repeat(n, 1)
                state["text_emb.weight"] = torch.cat(
                    [state["text_emb.weight"], emb_pad], dim=0
                )
                state["text_head.weight"] = torch.cat(
                    [state["text_head.weight"], head_pad], dim=0
                )
            model.t3.load_state_dict(state, strict=True)
            model.t3.to(self._device).eval()
            from chatterbox import mtl_tts

            register_language(getattr(mtl_tts, "SUPPORTED_LANGUAGES", None))
            self._model = model
        return self._model

    def clone_voice(self, sample_wav_path: str) -> str:
        # Zero-shot: the reference clip IS the voice reference, used at synth time.
        if not os.path.exists(sample_wav_path):
            raise RuntimeError(f"chatterbox_sk: sample not found: {sample_wav_path}")
        return sample_wav_path

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        import io

        import torchaudio as ta

        model = self._load()
        wav = model.generate(text, language_id=lang, audio_prompt_path=voice_ref)
        buf = io.BytesIO()
        ta.save(buf, wav.cpu() if hasattr(wav, "cpu") else wav, model.sr, format="wav")
        return buf.getvalue()


def _factory() -> ChatterboxSkEngine:
    return ChatterboxSkEngine()


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(_factory, sys.argv[1:]))
