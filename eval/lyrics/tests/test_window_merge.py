"""Tests for window_merge.py — the pure window math behind the #144 `win60`
arm (60 s windows, 5 s overlap, offsets added, overlap de-duplicated, lines
past the audio end clipped out and COUNTED)."""

from __future__ import annotations

import pytest

from eval.lyrics import window_merge


def _line(text: str, start_ms: int, end_ms: int, text_sk: str = "sk") -> dict:
    return {"text": text, "start_ms": start_ms, "end_ms": end_ms, "text_sk": text_sk}


# ── split_windows ────────────────────────────────────────────────────────────


def test_split_windows_covers_whole_song_with_overlap() -> None:
    wins = window_merge.split_windows(180_000, win_ms=60_000, overlap_ms=5_000)
    assert wins == [
        (0, 60_000),
        (55_000, 115_000),
        (110_000, 170_000),
        (165_000, 180_000),
    ]
    # contiguous coverage: every window starts before the previous one ends,
    # by exactly the overlap
    for (_, prev_end), (start, _) in zip(wins, wins[1:]):
        assert prev_end - start == 5_000
    assert wins[0][0] == 0
    assert wins[-1][1] == 180_000


def test_split_windows_last_window_is_shorter() -> None:
    wins = window_merge.split_windows(130_000, win_ms=60_000, overlap_ms=5_000)
    assert wins[-1] == (110_000, 130_000)
    assert wins[-1][1] - wins[-1][0] < 60_000
    assert all(end - start == 60_000 for start, end in wins[:-1])


def test_split_windows_song_shorter_than_one_window() -> None:
    assert window_merge.split_windows(42_000) == [(0, 42_000)]


def test_split_windows_exact_multiple_has_no_empty_tail() -> None:
    # 60 s exactly -> ONE window; never a zero-length trailing window
    assert window_merge.split_windows(60_000) == [(0, 60_000)]
    wins = window_merge.split_windows(115_000)
    assert wins == [(0, 60_000), (55_000, 115_000)]
    assert all(end > start for start, end in wins)


def test_split_windows_defaults_are_60s_and_5s() -> None:
    assert window_merge.split_windows(120_000) == [
        (0, 60_000),
        (55_000, 115_000),
        (110_000, 120_000),
    ]


@pytest.mark.parametrize(
    ("duration_ms", "win_ms", "overlap_ms"),
    [
        (0, 60_000, 5_000),
        (-1, 60_000, 5_000),
        (90_000, 5_000, 5_000),
        (90_000, 60_000, -1),
    ],
)
def test_split_windows_rejects_invalid_arguments(
    duration_ms: int, win_ms: int, overlap_ms: int
) -> None:
    with pytest.raises(ValueError):
        window_merge.split_windows(duration_ms, win_ms=win_ms, overlap_ms=overlap_ms)


# ── clip_past_end ────────────────────────────────────────────────────────────


def test_clip_past_end_drops_and_counts_lines_starting_at_or_after_end() -> None:
    lines = [
        _line("a", 1_000, 2_000),
        _line("b", 9_000, 10_500),  # starts inside, ends past -> kept, overrun counted
        _line("c", 10_000, 11_000),  # starts AT the end -> past
        _line("d", 12_000, 13_000),  # past
    ]
    res = window_merge.clip_past_end(lines, 10_000)
    assert [ln["text"] for ln in res.lines] == ["a", "b"]
    assert res.n_past_end == 2
    assert res.n_end_overrun == 1
    # kept lines are never re-timed
    assert res.lines[1]["end_ms"] == 10_500


# ── merge_window_lines ───────────────────────────────────────────────────────


def test_merge_adds_window_offsets() -> None:
    per_window = [
        (0, [_line("first line", 1_000, 3_000)]),
        (55_000, [_line("second line here", 10_000, 12_000)]),
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    assert [(ln["text"], ln["start_ms"], ln["end_ms"]) for ln in res.lines] == [
        ("first line", 1_000, 3_000),
        ("second line here", 65_000, 67_000),
    ]
    assert res.n_duplicates_dropped == 0
    assert res.n_past_end == 0


def test_merge_does_not_mutate_input_lines() -> None:
    w1 = [_line("x y z", 2_000, 3_000)]
    window_merge.merge_window_lines(
        [(0, []), (55_000, w1)], audio_end_ms=115_000, overlap_ms=5_000
    )
    assert w1[0]["start_ms"] == 2_000


def test_merge_drops_near_duplicate_in_overlap() -> None:
    per_window = [
        (0, [_line("Holy is the Lord", 56_000, 58_500)]),
        # same sung line heard again by window 2 (relative 1.2 s -> 56.2 s),
        # punctuation/case differ -> normalized ratio >= 0.8 -> dropped
        (55_000, [_line("holy is the lord!", 1_200, 3_600)]),
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    assert len(res.lines) == 1
    assert res.lines[0]["start_ms"] == 56_000  # the earlier window's copy wins
    assert res.n_duplicates_dropped == 1


def test_merge_keeps_different_text_in_overlap() -> None:
    per_window = [
        (0, [_line("Holy is the Lord", 56_000, 58_500)]),
        (55_000, [_line("Worthy is the Lamb forever", 2_000, 4_000)]),
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    assert len(res.lines) == 2
    assert res.n_duplicates_dropped == 0


def test_merge_keeps_repeat_outside_overlap() -> None:
    """A chorus repeated LATER in the song (outside the overlap) is a real
    second occurrence, never a duplicate."""
    per_window = [
        (0, [_line("Holy is the Lord", 56_000, 58_500)]),
        (55_000, [_line("Holy is the Lord", 30_000, 32_000)]),  # 85 s
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    assert [ln["start_ms"] for ln in res.lines] == [56_000, 85_000]
    assert res.n_duplicates_dropped == 0


def test_merge_keeps_repeat_in_overlap_when_earlier_copy_is_far_away() -> None:
    """Same text in the overlap, but the only earlier copy is in the song's
    first seconds — a chorus repeat, not the same sung line seen twice."""
    per_window = [
        (0, [_line("Holy is the Lord", 2_000, 4_000)]),
        (55_000, [_line("Holy is the Lord", 1_000, 3_000)]),  # 56 s
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    assert [ln["start_ms"] for ln in res.lines] == [2_000, 56_000]
    assert res.n_duplicates_dropped == 0


def test_merge_returns_monotonic_start_order() -> None:
    per_window = [
        (0, [_line("b", 20_000, 21_000), _line("a", 5_000, 6_000)]),
        (55_000, [_line("d", 30_000, 31_000), _line("c", 8_000, 9_000)]),
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=115_000, overlap_ms=5_000
    )
    starts = [ln["start_ms"] for ln in res.lines]
    assert starts == sorted(starts)
    assert [ln["text"] for ln in res.lines] == ["a", "b", "c", "d"]


def test_merge_clips_and_counts_lines_past_audio_end() -> None:
    per_window = [
        (0, [_line("in range", 1_000, 2_000)]),
        (55_000, [_line("tail", 20_000, 22_000), _line("ghost", 70_000, 72_000)]),
    ]
    res = window_merge.merge_window_lines(
        per_window, audio_end_ms=100_000, overlap_ms=5_000
    )
    assert [ln["text"] for ln in res.lines] == ["in range", "tail"]
    assert res.n_past_end == 1


def test_merge_empty_windows() -> None:
    res = window_merge.merge_window_lines(
        [(0, []), (55_000, [])], audio_end_ms=100_000, overlap_ms=5_000
    )
    assert res.lines == []
    assert res.n_past_end == 0
    assert res.n_duplicates_dropped == 0


def test_text_similarity_is_normalized() -> None:
    assert window_merge.text_similarity("Holy, HOLY!", "holy holy") == 1.0
    assert window_merge.text_similarity("abc", "xyz") < 0.8
