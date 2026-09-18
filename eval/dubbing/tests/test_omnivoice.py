"""Pure unit tests for eval/dubbing/engines/omnivoice.build_generate_kwargs — the
ref_text-requirement enforcement. No model, no network (heavy imports are lazy)."""

from __future__ import annotations

import pytest

from eval.dubbing.engines import omnivoice


def test_build_generate_kwargs_valid():
    kw = omnivoice.build_generate_kwargs(
        "Ahoj svet.", "/tmp/ref.wav", "Hello world reference."
    )
    assert kw == {
        "text": "Ahoj svet.",
        "ref_audio": "/tmp/ref.wav",
        "ref_text": "Hello world reference.",
    }


def test_empty_gen_text_raises():
    with pytest.raises(ValueError, match="empty generation text"):
        omnivoice.build_generate_kwargs("   ", "/tmp/ref.wav", "ref")


def test_missing_ref_audio_raises():
    with pytest.raises(ValueError, match="ref_audio"):
        omnivoice.build_generate_kwargs("Ahoj.", "", "ref")


def test_missing_ref_text_raises():
    with pytest.raises(ValueError, match="ref_text"):
        omnivoice.build_generate_kwargs("Ahoj.", "/tmp/ref.wav", "")
    with pytest.raises(ValueError, match="ref_text"):
        omnivoice.build_generate_kwargs("Ahoj.", "/tmp/ref.wav", "   ")
