"""Tests for scripts/wall_frames.py's UTC anchoring (#289): each clip frame's
first clean picture in UTC, so its lag behind its program boundary can be
read against SongPlayer's ``on_air_item.frame_utc_ns``.
"""

from __future__ import annotations

from wall_analysis import Reading

import wall_frames as wf


def test_the_anchor_takes_the_closest_pair_and_the_counter_middle():
    counter = iter([0.0, 0.004, 1.0, 1.001, 2.0, 2.010])
    walls = iter([5_000, 6_000, 7_000])
    c, ns = wf.utc_anchor(lambda: next(counter), lambda: next(walls), tries=3)
    assert (c, ns) == (1.0005, 6_000)


def test_a_counter_instant_maps_onto_the_line_through_the_anchors():
    start, end = (10.0, 1_000_000_000), (20.0, 11_000_001_000)
    assert wf.to_utc_ns(10.0, start, end) == 1_000_000_000
    assert wf.to_utc_ns(20.0, start, end) == 11_000_001_000
    # Halfway: the counter's 100 ppb rate against UTC is followed.
    assert wf.to_utc_ns(15.0, start, end) == 6_000_000_500
    # One anchor instant twice: the counter's own seconds.
    assert wf.to_utc_ns(10.5, start, start) == 1_500_000_000


def test_each_frame_keeps_its_first_clean_picture_only():
    readings = [
        Reading("no_strip"),
        Reading("ok", 7, 8, 128, 1),  # a blend: not clean
        Reading("ok", 7, None, 0, 1),
        Reading("ok", 7, None, 0, 1),  # the same frame again: not first
        Reading("ok", 8, None, 0, 1),
        Reading("disagree", strips=2),
    ]
    times = [1.0, 1.01, 1.02, 1.03, 1.05, 1.06]
    seen = wf.first_seen_utc_ns(readings, times, (1.0, 0), (2.0, 1_000_000_000))
    assert seen == {"7": 20_000_000, "8": 50_000_000}
