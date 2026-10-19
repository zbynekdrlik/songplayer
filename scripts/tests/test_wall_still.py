"""Tests for the wall reader's still spans (#279's A/B,
``wall_spans.still_spans`` / ``capture.still_ms_max`` /
``capture.held_ms_max``): the longest span with no new picture, the
capture's own edges included, and the longest its own polls saw the output
hold. An empty dxcam poll proves nothing new was presented, so a span is
held but for the blind span of the picture that ended it; a starved
capture's span holds little; the capture's end holds whole.
"""

from __future__ import annotations

import math
from fractions import Fraction

import pytest

import wall_analysis as wa
import wall_frames as wf
import wall_spans as ws
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


def test_a_clean_capture_holds_about_one_refresh():
    report = capture(at_60_hz(0.0005, 120), every_ms(0.0, 2.0))
    assert 16 <= report["still_ms_max"] <= 18
    assert 15 <= report["held_ms_max"] <= 17


def test_an_output_that_presents_nothing_is_held_while_the_polls_go_on():
    presents = at_60_hz(0.0005, 60) + at_60_hz(2.5005, 60)
    report = capture(presents, every_ms(0.0, 3.5))
    assert report["still_ms_max"] == pytest.approx(2501 - 984, abs=1)
    assert report["held_ms_max"] == pytest.approx(2500 - 984, abs=1)


def test_a_hold_whose_ending_poll_came_late_is_held_but_for_that_poll():
    # The output resumes at 2.46 s; the polls pause 2.45-2.5 s: the picture
    # at 2.5 s can have come anywhere in those 50 ms.
    presents = at_60_hz(0.0005, 60) + at_60_hz(2.4605, 60)
    report = capture(presents, every_ms(0.0, 2.45) + every_ms(2.5, 3.5))
    assert report["still_ms_max"] == pytest.approx(2500 - 984, abs=1)
    assert report["held_ms_max"] == pytest.approx(2450 - 984, abs=1)


def test_a_starved_capture_holds_little():
    # The polls stop for 1.5 s while the output presents on.
    polls = every_ms(0.0, 1.0) + every_ms(2.5, 3.5)
    report = capture(at_60_hz(0.0005, 210), polls)
    assert report["still_ms_max"] == pytest.approx(2500 - 984, abs=1)
    assert report["held_ms_max"] < 20


def test_a_hold_is_seen_beside_a_starved_stretch():
    # A 1.3 s hold with the polls on, then the polls starved 3.0-4.5 s:
    # the longest still span is the starvation, the longest held the hold.
    presents = at_60_hz(0.0005, 60) + at_60_hz(2.3005, 160)
    report = capture(presents, every_ms(0.0, 3.0) + every_ms(4.5, 5.0))
    assert report["still_ms_max"] == pytest.approx(4500 - 2984, abs=1)
    assert report["held_ms_max"] == pytest.approx(2300 - 984, abs=1)


def test_the_capture_s_edges_count():
    # Nothing presented after 0.98 s while the polls run to 2.5 s: the last
    # poll found nothing new, so the tail is held whole.
    report = capture(at_60_hz(0.0005, 60), every_ms(0.0, 2.5))
    assert report["still_ms_max"] == pytest.approx(2500 - 984, abs=1)
    assert report["held_ms_max"] == pytest.approx(2500 - 984, abs=1)
    # Nothing presented for the first 1.2 s: held but for the first
    # picture's own blind span.
    report = capture(at_60_hz(1.2005, 60), every_ms(0.0, 2.5))
    assert report["still_ms_max"] == pytest.approx(1201, abs=1)
    assert report["held_ms_max"] == pytest.approx(1200, abs=1)


def test_one_picture_s_capture_reports_its_spans_too():
    # One picture at 1.001 s in a 2.5 s capture: the tail after it.
    report = capture(at_60_hz(1.0005, 1), every_ms(0.0, 2.5))
    assert report["interval_ms_max"] is None
    assert report["still_ms_max"] == pytest.approx(1499, abs=1)
    assert report["held_ms_max"] == pytest.approx(1499, abs=1)


def test_still_spans_without_a_span_or_blind_spans():
    # No blind spans known: nothing proven held between pictures.
    timing = wa.Timing([0.0, 0.5, 1.75], None, None, False, None)
    spans = ws.still_spans([0.0, 0.5, 1.75], timing)
    assert spans == [(0.0, 0.0), (0.5, 0.0), (1.25, 0.0), (0.0, 0.0)]
    late = wa.Timing([2.0, 2.5], None, None, False, None)
    assert max(ws.still_spans([2.0, 2.5], late)) == (0.5, 0.0), "no head"
    assert ws.still_spans([], wa.Timing([], None, None, False, None)) == []
    report = ws.still_report([], wa.Timing([], None, None, False, None))
    assert report == {"still_ms_max": 0.0, "held_ms_max": 0.0}
    one = wa.Timing([2.0], [0.4], None, False, (1.0, 4.5))
    assert ws.still_spans([2.0], one) == [(1.0, 0.6), (2.5, 2.5)]
    assert ws.still_report([2.0], one) == {
        "still_ms_max": 2500.0,
        "held_ms_max": 2500.0,
    }
    head = wa.Timing([3.0, 3.5], [0.25, 0.001], None, False, (1.0, 3.75))
    assert ws.still_report([3.0, 3.5], head) == {
        "still_ms_max": 2000.0,
        "held_ms_max": 1750.0,
    }
    assert math.isfinite(ws.still_report([0.0, 0.5, 1.75], timing)["held_ms_max"])
