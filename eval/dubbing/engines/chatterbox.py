#!/usr/bin/env python3
"""chatterbox.py — Chatterbox Multilingual (Resemble AI, MIT) voice-cloning DubEngine.

Runs on the dev2 GPU (RTX 5050, 8 GB) — NEVER on win-resolume. Install into a
venv under `~/devel/dubbing-eval/` with `pip install chatterbox-tts`. Cloning is
ZERO-SHOT: the reference clip is passed as `audio_prompt_path` at synthesis time,
so `clone_voice` just returns the reference path.

API (github.com/resemble-ai/chatterbox, src/chatterbox/mtl_tts.py):

    from chatterbox.mtl_tts import ChatterboxMultilingualTTS
    model = ChatterboxMultilingualTTS.from_pretrained(device="cuda")
    wav = model.generate(text, language_id="sk", audio_prompt_path=ref)  # torch tensor
    torchaudio.save(out, wav, model.sr)

IMPORTANT — Slovak availability is version-dependent. The model validates
`language_id` against `SUPPORTED_LANGUAGES` and raises `ValueError` for an
unknown code. Resemble's v3 announcement lists Slovak, but some published package
builds ship a 23-language map WITHOUT `sk`. This engine therefore checks the
INSTALLED package's real `SUPPORTED_LANGUAGES` and, if the requested language is
missing, fails loudly with the exact supported set (the runner records it as the
engine's failure mode for the report) unless `CHATTERBOX_ALLOW_UNLISTED=1` is set
to force the attempt anyway. The device is `CHATTERBOX_DEVICE` (default "cuda").

Heavy imports (torch / torchaudio / chatterbox) are done lazily inside methods so
this module imports cleanly on any box (ruff, and the dev1 orchestrator).
"""

from __future__ import annotations

import io
import logging
import os

logger = logging.getLogger("dubbing_eval.chatterbox")


class ChatterboxEngine:
    """Chatterbox Multilingual zero-shot voice-cloning engine (dev2 GPU)."""

    name = "chatterbox"

    def __init__(self, device: str | None = None) -> None:
        self._device = device or os.environ.get("CHATTERBOX_DEVICE", "cuda")
        self._model = None

    def _load(self):
        if self._model is None:
            from chatterbox.mtl_tts import ChatterboxMultilingualTTS

            logger.info("chatterbox loading model on device=%s", self._device)
            self._model = ChatterboxMultilingualTTS.from_pretrained(device=self._device)
        return self._model

    def _check_language(self, lang: str) -> None:
        supported = (
            getattr(self._model, "SUPPORTED_LANGUAGES", None)
            or _module_supported_languages()
        )
        if supported is None:
            logger.warning("chatterbox: could not read SUPPORTED_LANGUAGES")
            return
        if lang.lower() not in {k.lower() for k in supported}:
            msg = (
                f"chatterbox: language '{lang}' not in the installed package's "
                f"SUPPORTED_LANGUAGES ({sorted(supported)})"
            )
            if os.environ.get("CHATTERBOX_ALLOW_UNLISTED") == "1":
                logger.warning(
                    "%s — forcing the attempt (CHATTERBOX_ALLOW_UNLISTED=1)", msg
                )
                return
            raise RuntimeError(msg)

    def clone_voice(self, sample_wav_path: str) -> str:
        # Zero-shot: the reference clip IS the voice reference, used at synth time.
        if not os.path.exists(sample_wav_path):
            raise RuntimeError(f"chatterbox: sample not found: {sample_wav_path}")
        return sample_wav_path

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        import torchaudio as ta

        model = self._load()
        self._check_language(lang)
        wav = model.generate(text, language_id=lang, audio_prompt_path=voice_ref)
        buf = io.BytesIO()
        ta.save(buf, wav.cpu() if hasattr(wav, "cpu") else wav, model.sr, format="wav")
        return buf.getvalue()


def _module_supported_languages():
    """Read SUPPORTED_LANGUAGES from the module as a fallback if the class does
    not expose it. Returns None if the package is not importable."""
    try:
        from chatterbox import mtl_tts

        return getattr(mtl_tts, "SUPPORTED_LANGUAGES", None)
    except ImportError:
        return None


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(ChatterboxEngine, sys.argv[1:]))
