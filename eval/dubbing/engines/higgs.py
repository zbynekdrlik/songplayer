#!/usr/bin/env python3
"""higgs.py — bosonai/higgs-* : reason-row engine (round 3, #175).

Boson AI's Higgs Audio family (Higgs TTS 3 `bosonai/higgs-tts-3-4b`, Higgs Audio v2
`bosonai/higgs-audio-v2-generation-3B-base`, community `drbaph/Higgs-Audio-v3-Studio`)
is an expressive, controllable multilingual TTS with Slovak listed among its
supported languages (Higgs TTS 3 card: "🇸🇰 Slovak"). It is NOT rendered in round 3
for two decisive, verified reasons:

1. **Licence — research & non-commercial.** All three are under a Boson
   Research-and-Non-Commercial licence (`bosonai/higgs-tts-3-4b/LICENSE`): "This
   Agreement is intended to allow Research and Non-Commercial use of the Higgs
   Materials free of charge. Any Commercial use ... requires a separate written
   license from Boson." (A "Creator Use Grant" permits monetised *creative content*
   with attribution, but embedding the model in a product/service still needs a
   commercial licence.) → cannot be the production winner as-is.
2. **VRAM — does not fit the 8 GB card.** `bosonai/higgs-tts-3-4b`'s
   `model.safetensors.index.json` reports `total_size = 8_489_763_794` bytes of
   weights (7.91 GiB) versus the dev2 RTX 5050's 8_151 MiB total VRAM (7.96 GiB) —
   only ~55 MiB of headroom below the weights alone, before the CUDA context
   (~0.4–0.6 GB), the separate audio tokenizer/codec, and activations, which a
   load needs → cannot fit. The v2 3B / v3 variants likewise need the full
   `boson_multimodal` serving stack + a separate audio tokenizer, beyond the
   round-3 plain-load 8 GB budget.

This module is intentionally NOT runnable: it fails loud with the exact reason so
the harness records it as the blocking reason (never a silent skip). `blocking_reason`
is the pure, CI-tested source of truth for the report row.
"""

from __future__ import annotations

MODEL_IDS = (
    "bosonai/higgs-tts-3-4b",
    "bosonai/higgs-audio-v2-generation-3B-base",
    "drbaph/Higgs-Audio-v3-Studio",
)
# Measured from bosonai/higgs-tts-3-4b/model.safetensors.index.json (weights only).
HIGGS_TTS_3_4B_WEIGHT_BYTES = 8_489_763_794
DEV2_VRAM_MIB = 8_151


# A bare CUDA context on this driver reserves several hundred MB before any
# tensor; require at least this much free VRAM above the weights to have any
# hope of loading (still ignores the audio tokenizer + activations).
CUDA_CONTEXT_FLOOR_MIB = 400


def blocking_reason() -> dict:
    """Return the structured, verified reason this candidate is a reason row
    (pure + CI-tested — quoted/measured from the model card, not memory)."""
    vram_bytes = DEV2_VRAM_MIB * 1024 * 1024
    weight_gib = round(HIGGS_TTS_3_4B_WEIGHT_BYTES / (1024**3), 2)
    vram_gib = round(DEV2_VRAM_MIB / 1024, 2)
    headroom_mib = round((vram_bytes - HIGGS_TTS_3_4B_WEIGHT_BYTES) / (1024**2), 1)
    # Fits only if the weights leave room for at least the CUDA context.
    runnable = headroom_mib >= CUDA_CONTEXT_FLOOR_MIB
    return {
        "models": list(MODEL_IDS),
        "slovak": True,  # Higgs TTS 3 card lists "🇸🇰 Slovak"
        "license": "Boson Higgs Research & Non-Commercial License",
        "license_quote": (
            "This Agreement is intended to allow Research and Non-Commercial use of "
            "the Higgs Materials free of charge. Any Commercial use of the Higgs "
            "Materials requires a separate written license from Boson."
        ),
        "weight_bytes": HIGGS_TTS_3_4B_WEIGHT_BYTES,
        "weight_gib": weight_gib,
        "vram_gib": vram_gib,
        "headroom_mib": headroom_mib,
        "runnable_8gb": runnable,
        "reason": (
            "non-commercial licence (product use needs a separate Boson licence) + "
            f"{weight_gib} GiB weights leave only ~{headroom_mib} MiB below the "
            f"{vram_gib} GiB dev2 VRAM — the CUDA context + audio tokenizer + "
            "activations do not fit 8 GB"
        ),
    }


class HiggsEngine:
    """Reason-row stub — bosonai/higgs-* is NC-licensed and >8 GB; not rendered."""

    name = "higgs"

    def clone_voice(self, sample_wav_path: str) -> str:
        raise RuntimeError(f"higgs not rendered: {blocking_reason()['reason']}")

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        raise RuntimeError(f"higgs not rendered: {blocking_reason()['reason']}")
