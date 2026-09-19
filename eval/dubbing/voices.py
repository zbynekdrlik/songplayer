#!/usr/bin/env python3
"""voices.py — the round-2 candidate voice matrix (pure data + selection helpers).

Round 2 (#175) compares VERIFIED native-Slovak voices on ONE fixed sentence set.
This module carries the candidate catalogue for the engines that expose named
voices (Gemini prebuilt voices; the Soniox stock catalogue is discovered live at
run time) and the pure helpers that expand a model × voice matrix into candidate
rows. It has no I/O or heavy deps, so the expansion logic is unit-tested in CI.

Gemini prebuilt voices (from the Gemini speech-generation docs, 30 total). The
`gender` here is the documented voice character grouping used only to guarantee
a male+female spread in the listening test; it is metadata for the report, not a
capability claim — every row is still backed by a REAL generated sample.
"""

from __future__ import annotations

from dataclasses import dataclass

# Gemini TTS model under test. Round-2 third verdict: NEWEST-ONLY — the owner
# dropped gemini-2.5-pro-preview-tts (a superseded model); only the newest
# 3.1-flash preview is rendered as a cloud reference row.
GEMINI_MODELS = ("gemini-3.1-flash-tts-preview",)

# A male+female spread of Gemini prebuilt voices for the listening test.
# (name, gender, documented character) — names verified against the Gemini
# speech-generation voice catalogue.
GEMINI_VOICES: tuple[tuple[str, str, str], ...] = (
    ("Charon", "male", "informative"),
    ("Orus", "male", "firm"),
    ("Puck", "male", "upbeat"),
    ("Kore", "female", "firm"),
    ("Aoede", "female", "breezy"),
    ("Leda", "female", "youthful"),
)

# The Slovak style instruction prepended to each sentence (design §1).
GEMINI_STYLE = "Prečítaj prirodzene, ako kazateľ na bohoslužbe, po slovensky:"


@dataclass(frozen=True)
class Candidate:
    """One listening-test candidate: an engine + model + voice combination."""

    engine: str  # "gemini" | "xtts" | "soniox"
    model: str  # e.g. "gemini-3.1-flash-tts-preview"
    voice: str  # voice name / reference label
    gender: str  # "male" | "female" | "unknown"
    note: str = ""  # short human note (character, reference kind, …)

    @property
    def slug(self) -> str:
        """A filesystem-safe id: `<engine>_<model-tail>_<voice>`."""
        tail = self.model.replace("gemini-", "").replace("-preview", "")
        tail = tail.replace("-tts", "").replace(".", "_").replace("-", "_")
        voice = self.voice.replace(" ", "_").replace("/", "_")
        return f"{self.engine}_{tail}_{voice}"


def gemini_matrix(
    models: tuple[str, ...] = GEMINI_MODELS,
    voices: tuple[tuple[str, str, str], ...] = GEMINI_VOICES,
    voices_per_model: int | None = None,
) -> list[Candidate]:
    """Expand the Gemini model × voice matrix into `Candidate` rows.

    `voices_per_model` caps how many voices are used per model (kept balanced
    male/female by taking from the front of the ordered `voices`); `None` uses
    all. Guarantees at least one male and one female voice per model when the
    catalogue has both and the cap allows it.
    """
    if voices_per_model is not None and voices_per_model <= 0:
        raise ValueError("voices_per_model must be positive or None")
    selected = _balanced_head(voices, voices_per_model)
    rows: list[Candidate] = []
    for model in models:
        for name, gender, character in selected:
            rows.append(
                Candidate(
                    engine="gemini",
                    model=model,
                    voice=name,
                    gender=gender,
                    note=character,
                )
            )
    return rows


def _balanced_head(
    voices: tuple[tuple[str, str, str], ...], n: int | None
) -> list[tuple[str, str, str]]:
    """Take `n` voices keeping a male/female balance, or all when `n` is None."""
    if n is None or n >= len(voices):
        return list(voices)
    males = [v for v in voices if v[1] == "male"]
    females = [v for v in voices if v[1] == "female"]
    out: list[tuple[str, str, str]] = []
    i = 0
    while len(out) < n and (males or females):
        pool = males if (i % 2 == 0 and males) or not females else females
        if pool:
            out.append(pool.pop(0))
        i += 1
    # Preserve the catalogue order for stable, readable output.
    order = {v: k for k, v in enumerate(voices)}
    return sorted(out, key=lambda v: order[v])
