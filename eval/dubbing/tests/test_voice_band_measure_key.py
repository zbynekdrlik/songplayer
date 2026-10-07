"""voice_band_measure._api_key (#229): the Gemini key comes from the
GEMINI_API_KEY environment variable, the first entry of a comma-separated list
(the box settings endpoint shows it masked). No network, no engine calls."""

from __future__ import annotations

import pytest

from eval.dubbing import voice_band_measure as vbm


def test_the_key_is_the_first_entry_of_the_env_list(monkeypatch):
    monkeypatch.setenv("GEMINI_API_KEY", " alpha , beta")
    assert vbm._api_key() == "alpha"


def test_an_unset_key_is_refused(monkeypatch):
    monkeypatch.delenv("GEMINI_API_KEY", raising=False)
    with pytest.raises(RuntimeError, match="GEMINI_API_KEY not set"):
        vbm._api_key()


def test_a_blank_key_is_refused(monkeypatch):
    monkeypatch.setenv("GEMINI_API_KEY", "  ")
    with pytest.raises(RuntimeError, match="GEMINI_API_KEY not set"):
        vbm._api_key()
