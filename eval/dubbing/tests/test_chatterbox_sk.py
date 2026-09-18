"""Pure unit tests for eval/dubbing/engines/chatterbox_sk.py — the vocab-reconcile
plan and the SUPPORTED_LANGUAGES registration. No torch, no chatterbox, no network
(heavy imports in the module are lazy inside methods)."""

from __future__ import annotations

import pytest

from eval.dubbing.engines import chatterbox_sk


def test_plan_vocab_trim_when_finetune_bigger():
    assert chatterbox_sk.plan_vocab_reconcile(2500, 2000) == ("trim", 500)


def test_plan_vocab_pad_when_finetune_smaller():
    assert chatterbox_sk.plan_vocab_reconcile(1800, 2000) == ("pad", 200)


def test_plan_vocab_ok_when_equal():
    assert chatterbox_sk.plan_vocab_reconcile(2000, 2000) == ("ok", 0)


def test_plan_vocab_non_positive_raises():
    with pytest.raises(ValueError, match="positive"):
        chatterbox_sk.plan_vocab_reconcile(0, 2000)
    with pytest.raises(ValueError, match="positive"):
        chatterbox_sk.plan_vocab_reconcile(2000, -1)


def test_register_language_dict_adds_when_missing():
    d = {"en": "English", "cs": "Czech"}
    assert chatterbox_sk.register_language(d) is True
    assert d["sk"] == "Slovak"


def test_register_language_dict_noop_when_present():
    d = {"sk": "Slovak"}
    assert chatterbox_sk.register_language(d) is False
    assert d == {"sk": "Slovak"}


def test_register_language_list_adds_when_missing():
    lst = ["en", "cs"]
    assert chatterbox_sk.register_language(lst) is True
    assert "sk" in lst


def test_register_language_list_noop_when_present():
    lst = ["sk"]
    assert chatterbox_sk.register_language(lst) is False
    assert lst == ["sk"]


def test_register_language_unknown_container_is_noop():
    assert chatterbox_sk.register_language(None) is False


def test_register_custom_code():
    d = {}
    assert chatterbox_sk.register_language(d, code="pl", name="Polish") is True
    assert d["pl"] == "Polish"
