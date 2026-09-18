#!/usr/bin/env python3
"""xtts_sk.py — Felagund/XTTSv2-sk voice-cloning DubEngine (local, dev2 GPU).

A community fine-tune of `coqui/XTTS-v2` for Slovak (HF `Felagund/XTTSv2-sk`,
repo MIT / weights under the coqui XTTS license). Unlike Gemini's prebuilt
voices, XTTS clones zero-shot from a reference clip via `speaker_wav`, so round 2
runs it BOTH with the preacher's `clone.wav` AND with a native-Slovak reference
clip — the design wants to see whether a NATIVE-Slovak reference removes the
accent leak that sank round 1's cloned English speaker.

Runs ONLY on dev2 (RTX 5050, Blackwell sm_120) in a dedicated venv
(`~/devel/dubbing-eval/xttsvenv`, torch cu128 — see `.claude/rules/dubbing-eval.md`).
NEVER on win-resolume.

Loading (coqui low-level XTTS API) — the model files (`config.json`, `model.pth`,
`vocab.json`) are downloaded from HF into `XTTS_MODEL_DIR`:

    from TTS.tts.configs.xtts_config import XttsConfig
    from TTS.tts.models.xtts import Xtts
    config = XttsConfig(); config.load_json(f"{dir}/config.json")
    model = Xtts.init_from_config(config)
    model.load_checkpoint(config, checkpoint_dir=dir, vocab_path=f"{dir}/vocab.json",
                          use_deepspeed=False)
    model.cuda()
    out = model.synthesize(text, config, speaker_wav=ref, language="sk")
    # out["wav"] is a float32 numpy waveform at config.audio.output_sample_rate (24 kHz)

Heavy imports (torch / TTS / numpy) are lazy inside methods so this file imports
cleanly under ruff and on dev1 (which never runs it).
"""

from __future__ import annotations

import logging
import os

from eval.dubbing import wavutil

logger = logging.getLogger("dubbing_eval.xtts_sk")

DEFAULT_MODEL_DIR = os.path.expanduser("~/devel/dubbing-eval/models/xtts-sk")


class XttsSkEngine:
    """Felagund/XTTSv2-sk zero-shot voice-cloning engine (dev2 GPU)."""

    name = "xtts"

    def __init__(self, model_dir: str | None = None, device: str | None = None) -> None:
        self._model_dir = model_dir or os.environ.get(
            "XTTS_MODEL_DIR", DEFAULT_MODEL_DIR
        )
        self._device = device or os.environ.get("XTTS_DEVICE", "cuda")
        self._model = None
        self._config = None

    def _load(self):
        if self._model is None:
            import torch  # noqa: F401  (ensures torch/cuda is importable first)
            from TTS.tts.configs.xtts_config import XttsConfig
            from TTS.tts.models.xtts import Xtts

            cfg_path = os.path.join(self._model_dir, "config.json")
            vocab_path = os.path.join(self._model_dir, "vocab.json")
            if not os.path.exists(cfg_path):
                raise RuntimeError(f"xtts: config.json not found in {self._model_dir}")
            logger.info("xtts loading from %s on %s", self._model_dir, self._device)
            config = XttsConfig()
            config.load_json(cfg_path)
            model = Xtts.init_from_config(config)
            model.load_checkpoint(
                config,
                checkpoint_dir=self._model_dir,
                vocab_path=vocab_path,
                use_deepspeed=False,
            )
            if self._device == "cuda":
                model.cuda()
            self._model = model
            self._config = config
        return self._model, self._config

    def clone_voice(self, sample_wav_path: str) -> str:
        # Zero-shot: the reference clip IS the voice reference, used at synth time.
        if not os.path.exists(sample_wav_path):
            raise RuntimeError(f"xtts: reference clip not found: {sample_wav_path}")
        return sample_wav_path

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        import numpy as np

        model, config = self._load()
        out = model.synthesize(text, config, speaker_wav=voice_ref, language=lang)
        wav = out["wav"] if isinstance(out, dict) else out
        arr = np.asarray(wav, dtype=np.float32).reshape(-1)
        # float32 [-1,1] -> little-endian int16 PCM.
        pcm = (np.clip(arr, -1.0, 1.0) * 32767.0).astype("<i2").tobytes()
        sr = int(getattr(config.audio, "output_sample_rate", 24000))
        return wavutil.pcm_l16_to_wav(pcm, sample_rate=sr, channels=1)


def _factory() -> XttsSkEngine:
    return XttsSkEngine()


if __name__ == "__main__":
    import sys

    from eval.dubbing.engines.base import engine_cli

    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    raise SystemExit(engine_cli(_factory, sys.argv[1:]))
