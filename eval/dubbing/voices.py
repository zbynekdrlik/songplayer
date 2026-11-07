#!/usr/bin/env python3
"""voices.py — the vetted Gemini prebuilt voice catalogue (pure data).

The six voices a dub may pin (`dub_voice`); the sp-ui settings list mirrors
this catalogue and `voice_band_measure.py` renders one Live session per voice.
The `gender` is the documented voice character grouping (metadata, not a
capability claim). No I/O or heavy deps.
"""

from __future__ import annotations

# The catalogue: (name, gender, documented character) — names verified against
# the Gemini speech-generation voice catalogue.
GEMINI_VOICES: tuple[tuple[str, str, str], ...] = (
    ("Charon", "male", "informative"),
    ("Orus", "male", "firm"),
    ("Puck", "male", "upbeat"),
    ("Kore", "female", "firm"),
    ("Aoede", "female", "breezy"),
    ("Leda", "female", "youthful"),
)
