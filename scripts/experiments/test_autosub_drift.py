"""Unit tests for autosub_drift.py pure functions."""

def test_placeholder():
    assert True


from autosub_drift import normalize_word


def test_normalize_lowercases():
    assert normalize_word("Love") == "love"


def test_normalize_strips_punctuation():
    assert normalize_word("love!") == "love"
    assert normalize_word("you're") == "youre"
    assert normalize_word("...don't?") == "dont"


def test_normalize_returns_empty_for_noise():
    assert normalize_word("[music]") == ""
    assert normalize_word(">>") == ""
    assert normalize_word("") == ""
    assert normalize_word("   ") == ""


def test_normalize_keeps_alphanumeric():
    assert normalize_word("123abc") == "123abc"
