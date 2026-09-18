"""Pure unit tests for eval/dubbing/voices.py — the round-2 candidate matrix
expansion and slug generation. No network, no engine calls."""

from __future__ import annotations

import pytest

from eval.dubbing import voices
from eval.dubbing.voices import Candidate


def test_gemini_matrix_full_is_models_times_voices():
    rows = voices.gemini_matrix()
    assert len(rows) == len(voices.GEMINI_MODELS) * len(voices.GEMINI_VOICES)
    assert {r.engine for r in rows} == {"gemini"}
    assert {r.model for r in rows} == set(voices.GEMINI_MODELS)


def test_gemini_matrix_capped_keeps_male_and_female_per_model():
    rows = voices.gemini_matrix(voices_per_model=2)
    assert len(rows) == len(voices.GEMINI_MODELS) * 2
    for model in voices.GEMINI_MODELS:
        genders = {r.gender for r in rows if r.model == model}
        assert genders == {"male", "female"}


def test_gemini_matrix_cap_larger_than_catalogue_returns_all():
    rows = voices.gemini_matrix(voices_per_model=99)
    assert len(rows) == len(voices.GEMINI_MODELS) * len(voices.GEMINI_VOICES)


def test_gemini_matrix_rejects_non_positive_cap():
    with pytest.raises(ValueError):
        voices.gemini_matrix(voices_per_model=0)


def test_candidate_slug_is_filesystem_safe_and_distinct():
    a = Candidate("gemini", "gemini-3.1-flash-tts-preview", "Kore", "female")
    b = Candidate("gemini", "gemini-2.5-pro-preview-tts", "Kore", "female")
    assert a.slug == "gemini_3_1_flash_Kore"
    assert b.slug == "gemini_2_5_pro_Kore"
    assert a.slug != b.slug
    # No characters that would break a filename.
    for ch in " /.":
        assert ch not in a.slug


def test_candidate_slug_handles_reference_labels_with_spaces_and_slashes():
    c = Candidate("xtts", "xtts-sk", "native ref/clip", "unknown")
    assert " " not in c.slug and "/" not in c.slug


def test_balanced_head_preserves_catalogue_order():
    rows = voices.gemini_matrix(voices_per_model=4)
    first_model = [r.voice for r in rows if r.model == voices.GEMINI_MODELS[0]]
    order = [v[0] for v in voices.GEMINI_VOICES]
    assert first_model == sorted(first_model, key=order.index)
