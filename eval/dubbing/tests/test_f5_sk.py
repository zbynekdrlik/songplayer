"""Pure unit tests for eval/dubbing/engines/f5_sk.build_infer_kwargs — the
ref_text-requirement enforcement. No model, no network (heavy imports are lazy)."""

from __future__ import annotations

import pytest

from eval.dubbing.engines import f5_sk


def test_build_infer_kwargs_valid():
    kw = f5_sk.build_infer_kwargs("/tmp/ref.wav", "referencia", "Ahoj svet.")
    assert kw == {
        "ref_file": "/tmp/ref.wav",
        "ref_text": "referencia",
        "gen_text": "Ahoj svet.",
        "remove_silence": True,
    }


def test_remove_silence_flag_passthrough():
    kw = f5_sk.build_infer_kwargs("/r.wav", "r", "g", remove_silence=False)
    assert kw["remove_silence"] is False


def test_empty_gen_text_raises():
    with pytest.raises(ValueError, match="empty generation text"):
        f5_sk.build_infer_kwargs("/tmp/ref.wav", "ref", "  ")


def test_missing_ref_file_raises():
    with pytest.raises(ValueError, match="ref_file"):
        f5_sk.build_infer_kwargs("", "ref", "gen")


def test_missing_ref_text_raises():
    with pytest.raises(ValueError, match="ref_text"):
        f5_sk.build_infer_kwargs("/tmp/ref.wav", "", "gen")
