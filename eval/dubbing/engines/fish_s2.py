#!/usr/bin/env python3
"""fish_s2.py — fishaudio/s2-pro: reason-row engine (round 3, #175).

Fish Audio S2 Pro is a strong multilingual TTS (`fish_qwen3_omni` — a Qwen3-Omni
backbone, 2 safetensors shards) with fine-grained inline prosody/emotion control
and, per its card, Slovak among 80+ languages. It is NOT rendered in round 3 for
two decisive, verified reasons:

1. **Licence — non-commercial.** The weights are under the *Fish Audio Research
   License* (`LICENSE.md`): "This Agreement is intended to allow research and
   non-commercial uses of the Materials free of charge. Any Commercial use of the
   Materials requires a separate license from Fish Audio." A church-service dubbing
   feature embedded in SongPlayer is a commercial/product use → cannot be the
   production winner without a separate paid licence.
2. **Serving footprint.** `fish_qwen3_omni` ships as a 2-shard safetensors model
   with an SGLang-based streaming inference engine — a heavyweight serving stack
   that does not fit the round-3 constraint of a plain in-process load on the dev2
   RTX 5050 (8 GB), unlike the 0.3–0.6 B open engines actually rendered.

This module is intentionally NOT runnable: it fails loud with the exact reason so
the harness records it as the blocking reason (never a silent skip). `blocking_reason`
is the pure, CI-tested source of truth for the report row.
"""

from __future__ import annotations

MODEL_ID = "fishaudio/s2-pro"


def blocking_reason() -> dict:
    """Return the structured, verified reason this candidate is a reason row
    (pure + CI-tested — quoted from the model card, not memory)."""
    return {
        "model": MODEL_ID,
        "slovak": True,  # card "Supported Languages" lists sk among 80+
        "license": "Fish Audio Research License (non-commercial)",
        "license_quote": (
            "This Agreement is intended to allow research and non-commercial uses "
            "of the Materials free of charge. Any Commercial use of the Materials "
            "requires a separate license from Fish Audio."
        ),
        "runnable_8gb": False,
        "reason": (
            "non-commercial licence (product use needs a separate Fish Audio "
            "licence) + fish_qwen3_omni 2-shard model served via SGLang, not a "
            "plain in-process 8 GB load"
        ),
    }


class FishS2Engine:
    """Reason-row stub — fishaudio/s2-pro is NC-licensed and not rendered."""

    name = "fish_s2"

    def clone_voice(self, sample_wav_path: str) -> str:
        raise RuntimeError(f"fish_s2 not rendered: {blocking_reason()['reason']}")

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        raise RuntimeError(f"fish_s2 not rendered: {blocking_reason()['reason']}")
