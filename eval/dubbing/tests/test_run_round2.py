"""Pure unit tests for eval/dubbing/run_round2.parse_indices (the --indices
selection parser). No audio/ffmpeg — just the parsing logic."""

from __future__ import annotations

from eval.dubbing import run_round2

ALL = list(range(0, 34))


def test_all_keywords_return_every_index():
    assert run_round2.parse_indices("all", ALL) == ALL
    assert run_round2.parse_indices("*", ALL) == ALL
    assert run_round2.parse_indices("", ALL) == ALL


def test_range_expands_inclusive_and_filters_to_known():
    assert run_round2.parse_indices("2-8", ALL) == [2, 3, 4, 5, 6, 7, 8]
    # out-of-range endpoints are filtered against the known set
    assert run_round2.parse_indices("30-40", ALL) == [30, 31, 32, 33]


def test_comma_list():
    assert run_round2.parse_indices("2,3,4", ALL) == [2, 3, 4]
    assert run_round2.parse_indices("8, 2 , 5", ALL) == [8, 2, 5]


def test_unknown_indices_dropped():
    assert run_round2.parse_indices("99,100", ALL) == []
