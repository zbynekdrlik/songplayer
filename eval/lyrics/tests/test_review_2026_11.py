"""Tests for the #349 review runners' pure logic (no torch: CI has numpy only)."""

from __future__ import annotations

from pathlib import Path

from eval.lyrics.review_2026_11 import ctc_lyrics as c
from eval.lyrics.review_2026_11.heart_asr import chunks_to_words
from eval.lyrics.review_2026_11.score_review import NO_BASELINE

VOCAB = {"<blank>": 0, "|": 1, "a": 2, "b": 3, "l": 4, "o": 5, "<unk>": 6}
ID_TO_TOK = {v: k for k, v in VOCAB.items()}


def test_window_plan_tiles_and_reads_at_most_30_s():
    n = int(95.3 * c.SR)
    plan = c.window_plan(n)
    assert plan[0][2] == 0 and plan[-1][3] == n
    for (_, _, _, k1), (_, _, k0, _) in zip(plan, plan[1:]):
        assert k1 == k0  # kept spans tile [0, n) with no gap or overlap
    for r0, r1, k0, k1 in plan:
        assert r1 - r0 <= c.READ_S * c.SR
        assert r0 == max(0, k0 - int(c.CONTEXT_S * c.SR))
        assert r1 == min(n, k1 + int(c.CONTEXT_S * c.SR))


def test_window_plan_join_frames_line_up():
    plan = c.window_plan(int(60 * c.SR))
    r0, _, k0, k1 = plan[1]
    # the kept slice of window 1 starts at its context, in 20 ms frames
    assert c.frames_for(k0 - r0) == 100
    assert c.frames_for(k1 - k0) == 1300  # 26 s kept / 20 ms


def test_kept_slice_interior_short_raises_last_short_is_cut():
    n = int(60 * c.SR)
    r0, _, k0, k1 = c.window_plan(n)[1]
    assert c.kept_slice(1500, r0, k0, k1, n) == (100, 1400)
    try:
        c.kept_slice(1399, r0, k0, k1, n)
    except RuntimeError as exc:
        assert "1399 frames < 1400" in str(exc)
    else:
        raise AssertionError("an interior window short of frames must raise")
    r0, _, k0, k1 = c.window_plan(n)[-1]
    assert k1 == n
    assert c.kept_slice(450, r0, k0, k1, n) == (100, 450)  # tail cut at the end


def test_build_targets_skips_unknown_words_and_tracks_owners():
    targets, owner, words = c.build_targets(VOCAB, ["Ab, ?!", "lo"])
    assert words == [["Ab,"], ["lo"]]
    assert targets == [1, 2, 3, 1, 4, 5, 1]
    assert owner == [None, (0, 0), (0, 0), None, (1, 0), (1, 0), None]


def test_word_spans_held_and_doubled_letters():
    # target "|ll|": l, l is a doubled letter -> blank between the copies
    owner = [None, (0, 0), (0, 0), None]
    path = [1, 4, 4, 0, 4, 0, 1]
    spans = c.word_spans(path, owner)
    assert spans == {(0, 0): [1, 4]}


def test_word_spans_two_words_and_held_separator():
    _, owner, _ = c.build_targets(VOCAB, ["ab lo"])
    path = [1, 1, 2, 3, 3, 1, 4, 0, 5, 1]
    spans = c.word_spans(path, owner)
    assert spans == {(0, 0): [2, 4], (0, 1): [6, 8]}


def test_lines_from_spans_keeps_untimed_line_null():
    lines = c.lines_from_spans(["ab", "lo"], [["ab"], ["lo"]], {(0, 0): [5, 9]})
    assert lines[0]["start_ms"] == 100 and lines[0]["end_ms"] == 200
    assert lines[1] == {"text": "lo", "start_ms": None, "end_ms": None, "words": None}


def test_greedy_words_ends_on_last_letter_not_unk():
    best = [0, 2, 2, 0, 2, 6, 6, 1, 0, 4, 5, 1]
    words = c.greedy_words(best, ID_TO_TOK)
    assert words == [
        {"text": "aa", "start_ms": 20, "end_ms": 100},
        {"text": "lo", "start_ms": 180, "end_ms": 220},
    ]


def test_group_lines_splits_at_400_ms():
    words = [
        {"text": "a", "start_ms": 0, "end_ms": 100},
        {"text": "b", "start_ms": 499, "end_ms": 600},  # gap 399 -> same line
        {"text": "c", "start_ms": 1000, "end_ms": 1100},  # gap 400 -> new line
    ]
    lines = c.group_lines(words)
    assert [ln["text"] for ln in lines] == ["a b", "c"]
    assert lines[0]["end_ms"] == 600


def test_chunks_to_words_drops_open_ends_and_sorts():
    chunks = [
        {"text": " late", "timestamp": (2.0, 2.5)},
        {"text": " open", "timestamp": (3.0, None)},
        {"text": " early", "timestamp": (0.5, 1.0)},
        {"text": "  ", "timestamp": (1.0, 1.2)},
    ]
    words = chunks_to_words(chunks)
    assert [w["text"] for w in words] == ["early", "late"]
    assert words[0] == {"text": "early", "start_ms": 500, "end_ms": 1000}


def test_common_view_filter_matches_file_stems():
    stem = Path("w2v-bert-lyrics-asr_vpwDdb8r9Bk.json").stem
    assert stem.endswith(NO_BASELINE)
    assert not Path("w2v-bert-lyrics-asr_5JW87KKDTcU.json").stem.endswith(NO_BASELINE)
