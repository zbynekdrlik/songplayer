"""Tests for the wall reader's longest still span (#279's A/B,
``wall_analysis.still_span`` / ``capture.still_ms_max``): a dxcam capture
whose polls run on while the output presents nothing ends the span with a
picture seen at once; a starved capture ends it with a picture as blind as
the span; at the capture's end nothing ends it.
"""

from __future__ import annotations

import math
from fractions import Fraction

import pytest

import wall_analysis as wa
import wall_frames as wf
from wall_fakes import pacer_readings


def dxcam(presents: list[float], polls: list[float]) -> tuple[list[float], list[float]]:
    """What dxcam returns: each poll gets the newest present since the poll
    before, at the poll's time; each picture's blind span (``poll_blind``)."""
    times = []
    seen = -1
    for t in polls:
        newest = max((k for k, p in enumerate(presents) if p <= t), default=-1)
        if newest > seen:
            seen = newest
            times.append(t)
    return times, wf.poll_blind(times, polls)


def every_ms(start: float, end: float) -> list[float]:
    return [start + n / 1000 for n in range(round((end - start) * 1000) + 1)]


def capture(presents: list[float], polls: list[float]) -> dict:
    times, blind = dxcam(presents, polls)
    readings = pacer_readings(30, Fraction(100), len(times), frames=1500, refreshes=1)
    report = wa.analyse(
        readings,
        30,
        times=times,
        blind=blind,
        span=(polls[0], polls[-1]),
        refresh_hz=60,
        min_boundaries=1,
    )
    return report["capture"]


def at_60_hz(start: float, count: int) -> list[float]:
    return [start + n / 60 for n in range(count)]


def test_a_clean_capture_is_still_for_about_one_refresh():
    report = capture(at_60_hz(0.0005, 120), every_ms(0.0, 2.0))
    assert 16 <= report["still_ms_max"] <= 18
    assert report["still_end_blind_ms"] == pytest.approx(1.0)


def test_an_output_that_presents_nothing_is_still_with_its_polls_seen():
    # 1.5 s with no present while the polls run on: a picture seen at once
    # ends the span.
    presents = at_60_hz(0.0005, 60) + at_60_hz(2.5005, 60)
    report = capture(presents, every_ms(0.0, 3.5))
    assert report["still_ms_max"] == pytest.approx(2501 - 984, abs=1)
    assert report["still_end_blind_ms"] == pytest.approx(1.0)


def test_a_starved_capture_ends_its_span_with_a_blind_picture():
    # The polls stop for 1.5 s while the output presents on: the picture
    # that ends the span is as blind as it.
    polls = every_ms(0.0, 1.0) + every_ms(2.5, 3.5)
    report = capture(at_60_hz(0.0005, 210), polls)
    assert report["still_ms_max"] == pytest.approx(2500 - 984, abs=1)
    assert report["still_end_blind_ms"] == pytest.approx(1500, abs=1)


def test_a_still_tail_and_head_count_the_capture_s_own_edges():
    # Nothing presented after 0.98 s while the polls run to 2.5 s: the
    # capture's end ends the span.
    report = capture(at_60_hz(0.0005, 60), every_ms(0.0, 2.5))
    assert report["still_ms_max"] == pytest.approx(2500 - 984, abs=1)
    assert report["still_end_blind_ms"] is None
    # Nothing presented for the first 1.2 s: the first picture ends it.
    report = capture(at_60_hz(1.2005, 60), every_ms(0.0, 2.5))
    assert report["still_ms_max"] == pytest.approx(1201, abs=1)
    assert report["still_end_blind_ms"] == pytest.approx(1.0)


def test_one_picture_s_capture_reports_its_still_span_too():
    # One picture at 1.001 s in a 2.5 s capture: the tail after it.
    report = capture(at_60_hz(1.0005, 1), every_ms(0.0, 2.5))
    assert report["interval_ms_max"] is None
    assert report["still_ms_max"] == pytest.approx(1499, abs=1)
    assert report["still_end_blind_ms"] is None


def test_still_span_without_a_span_or_blind_spans():
    timing = wa.Timing([0.0, 0.5, 1.75], None, None, False, None)
    assert wa.still_span([0.0, 0.5, 1.75], timing) == (1.25, math.inf)
    # An unknown blind span reads null, never Infinity (no JSON has it).
    assert wa.still_report([0.0, 0.5, 1.75], timing) == {
        "still_ms_max": 1250.0,
        "still_end_blind_ms": None,
    }
    late = wa.Timing([2.0, 2.5], None, None, False, None)
    assert wa.still_span([2.0, 2.5], late) == (0.5, math.inf), "no head"
    assert wa.still_span([], wa.Timing([], None, None, False, None)) == (0.0, None)
    one = wa.Timing([2.0], [0.001], None, False, (1.0, 4.5))
    assert wa.still_span([2.0], one) == (2.5, None)
    report = wa.still_report([2.0], one)
    assert report == {"still_ms_max": 2500.0, "still_end_blind_ms": None}
    head = wa.Timing([3.0, 3.5], [0.002, 0.001], None, False, (1.0, 3.6))
    assert wa.still_report([3.0, 3.5], head) == {
        "still_ms_max": 2000.0,
        "still_end_blind_ms": 2.0,
    }
