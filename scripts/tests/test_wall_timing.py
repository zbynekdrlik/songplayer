"""Tests for scripts/wall_analysis.py (#272): a capture's own timing.

A boundary is judged only when the capture's brackets prove its refresh
count (``held_range``): on screen at each of its pictures, begun after the
last look that still showed the boundary before and ended by the next
boundary's first picture. Review round 5 replaced the earlier blind-span
rules with these brackets: every case below was a wrong verdict there.

* a locked recording (each picture one refresh) and dxcam on time count
  every boundary exactly; a camera fast enough to sample every refresh
  proves each count and finds judder;
* a camera that cannot resolve a refresh (slower than the wall, or at its
  rate unlocked) never reads a clean wall as judder, and never calls a
  juddering one clean;
* a stall is judder wherever it lies, the capture's edges included;
* a refresh the capture was blind to can hide a boundary, one per refresh
  a boundary takes; a frozen wall; the wall's rate must be given.
"""

from __future__ import annotations

import math
from fractions import Fraction

import pytest

import wall_analysis as wa
from wall_fakes import at_60_hz, camera, judder, pacer_readings, wall_refreshes


def locked(readings, fps=30, **kw):
    """A locked recording at 60 Hz: each reading one refresh."""
    kw.setdefault("refresh_hz", 60)
    return wa.analyse(readings, fps, times=at_60_hz(readings), locked=True, **kw)


def dxcam(readings, fps=30, lag=0.0005, **kw):
    """dxcam polling every millisecond on a 60 Hz wall that presents every
    refresh: each picture taken ``lag`` after its present, its poll 1 ms
    after the one before."""
    times = [n / 60 + lag for n in range(len(readings))]
    blind = [math.inf] + [0.001] * (len(readings) - 1)
    return wa.analyse(readings, fps, times=times, refresh_hz=60, blind=blind, **kw)


def test_a_locked_recording_counts_every_boundary_exactly():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    report = locked(readings, min_boundaries=38)
    assert report["refreshes"]["judged"] == 38, "all but the capture's edges"
    assert report["refreshes"]["uneven"] == 0 and report["verdict"] == "clean"
    readings[10:10] = readings[10:12]  # boundary 5 for 4 refreshes
    report = locked(readings, min_boundaries=38)
    assert report["refreshes"]["uneven_samples"] == [10]
    assert report["verdict"] == "defects"


def test_dxcam_on_time_counts_every_boundary_exactly():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    report = dxcam(readings, min_boundaries=38)
    assert report["refreshes"]["judged"] == 38 and report["verdict"] == "clean"
    jud = wall_refreshes(judder(40, every=10), 30, start=Fraction(0))
    report = dxcam(jud, min_boundaries=1)
    assert report["refreshes"]["uneven"] == 7, "4 pairs, the first one's 1 an edge"


@pytest.mark.parametrize("cam_fps", [120, 240])
def test_a_camera_sampling_every_refresh_proves_each_count(cam_fps):
    wall = wall_refreshes([2] * 360, 25)
    readings, times = camera(wall, 60, cam_fps, offset=0.0003)
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["refreshes"]["judged"] >= 0.9 * report["boundaries"]
    assert report["refreshes"]["uneven"] == 0 and report["verdict"] == "clean"
    wall = wall_refreshes(judder(360), 25)
    readings, times = camera(wall, 60, cam_fps, offset=0.0003)
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["refreshes"]["uneven"] == 35, "18 pairs, the first 1 an edge"
    assert report["verdict"] == "defects"


@pytest.mark.parametrize(
    ("cam_fps", "wall_hz", "counts"),
    [
        (60, 60, [2]),  # at the wall's rate, unlocked
        (59.94, 60, [2]),
        (50, 60, [2]),
        (30, 60, [2]),
        (60, 50, [2, 2, 1]),  # 50 Hz on the 30 fps grid
        (120, 144, [5, 5, 5, 5, 4]),
    ],
)
def test_a_camera_that_cannot_resolve_a_refresh_never_calls_judder(
    cam_fps, wall_hz, counts
):
    # Review round 5: 50 / 59.94 fps cameras on 60 Hz, 60 fps on 50 Hz and
    # 120 fps on 144 Hz read a clean wall as defects.
    wall = wall_refreshes([counts[k % len(counts)] for k in range(360)], 25)
    readings, times = camera(wall, wall_hz, cam_fps, offset=0.0037)
    report = wa.analyse(readings, 25, times=times, refresh_hz=wall_hz)
    assert report["refreshes"]["uneven"] == 0, report["refreshes"]
    assert report["lost_boundaries"] == report["cadence_errors"] == 0
    assert report["verdict"] in ("clean", "incomplete")


def test_a_capture_too_coarse_to_resolve_judder_is_never_clean():
    # Review round 5: a 60 fps camera on a 75 Hz wall (slower than it)
    # proves each 2 / 3 refresh boundary, but a 4-refresh one only to 3..5:
    # unresolved, so never clean, whatever share was judged.
    clean = [2, 3] * 180
    readings, times = camera(wall_refreshes(clean, 25), 75, 60, offset=0.0037)
    report = wa.analyse(readings, 25, times=times, refresh_hz=75)
    assert report["refreshes"]["unresolved"] == 0 and report["verdict"] == "clean"
    counts = list(clean)
    for k in range(20, 360, 40):
        counts[k] = 4
    readings, times = camera(wall_refreshes(counts, 25), 75, 60, offset=0.0037)
    report = wa.analyse(readings, 25, times=times, refresh_hz=75)
    assert report["refreshes"]["unresolved"] > 0
    assert report["verdict"] != "clean"
    # A capture as fast as the wall leaves nothing open on a clean wall.
    wall = wall_refreshes([2] * 360, 25)
    readings, times = camera(wall, 60, 240, offset=0.0003)
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["refreshes"]["unresolved"] == 0


def test_a_camera_slower_than_the_wall_never_calls_judder_clean():
    # Review round 5: a 30 fps camera on a 60 Hz wall with 1 + 3 judder read
    # clean when the rate came from the capture.
    wall = wall_refreshes(judder(360, every=10), 25)
    readings, times = camera(wall, 60, 30, offset=0.0037)
    assert wa.analyse(readings, 25, times=times)["verdict"] == "incomplete"
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["verdict"] != "clean"
    # It still sees a 1 s freeze: on screen at instants 1 s apart.
    counts = [2] * 360
    counts[180] = 60
    readings, times = camera(wall_refreshes(counts, 25), 60, 30, offset=0.0037)
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["refreshes"]["uneven"] == 1 and report["verdict"] == "defects"


def test_a_stall_is_judder_wherever_it_lies():
    # Arena presents nothing while it holds boundary 6 for 12 refreshes:
    # dxcam's polls on time find nothing new, then the next boundary.
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    times = [n / 60 + 0.0005 for n in range(len(readings))]
    keep = readings[:13] + readings[24:]  # boundary 6 from refresh 12 to 23
    kept = times[:13] + times[24:]
    blind = [math.inf] + [0.001] * (len(keep) - 1)
    report = wa.analyse(
        keep, 30, times=kept, refresh_hz=60, blind=blind, min_boundaries=1
    )
    assert report["refreshes"]["uneven_samples"] == [12]
    assert report["lost_boundaries"] == 5, "7..11 never presented"
    # At the capture's start: one picture, then nothing new for 10 s.
    head = readings[:1] + readings[2:]
    times = [0.0005] + [10 + n / 60 for n in range(len(head) - 1)]
    blind = [math.inf] + [0.001] * (len(head) - 1)
    report = wa.analyse(head, 30, times=times, refresh_hz=60, blind=blind)
    assert report["refreshes"]["uneven_samples"] == [0]
    # At its end: the polls run on 10 s after the last present.
    times = [n / 60 + 0.0005 for n in range(len(readings))]
    blind = [math.inf] + [0.001] * (len(readings) - 1)
    report = wa.analyse(
        readings, 30, times=times, refresh_hz=60, blind=blind, span=(0, times[-1] + 10)
    )
    assert report["refreshes"]["uneven_samples"] == [len(readings) - 2]


def test_the_capture_s_edges_prove_only_a_hold():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)[1:-1]
    report = locked(readings, min_boundaries=38)
    assert report["refreshes"]["judged"] == 38 and report["verdict"] == "clean"
    held = [readings[0]] * 5 + readings
    assert locked(held)["refreshes"]["uneven_samples"] == [0]


def test_a_refresh_the_capture_was_blind_to_can_hide_one_boundary():
    readings = pacer_readings(30, Fraction(0), 60, frames=1800)
    times = at_60_hz(readings)
    # The recording dropped one frame (2 periods) where the wall lost
    # boundary 8: one refresh cannot hide a 2-refresh boundary.
    keep = readings[:16] + readings[18:]
    kept = times[:16] + [t - 1 / 60 for t in times[18:]]
    report = wa.analyse(keep, 30, times=kept, refresh_hz=60, locked=True)
    assert report["lost_boundaries"] == 1 and report["verdict"] == "defects"
    # Two dropped frames (boundary 8 whole): one boundary excused.
    keep = readings[:16] + readings[18:]
    kept = times[:16] + times[18:]
    report = wa.analyse(
        keep, 30, times=kept, refresh_hz=60, locked=True, min_boundaries=1
    )
    assert report["lost_boundaries"] == 0
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["capture"]["blind_pictures"] == 1
    assert report["verdict"] == "incomplete"
    # At 50 Hz a boundary can be one refresh: one dropped frame hides one.
    wall = wall_refreshes([2, 2, 1] * 20, 30, start=Fraction(0))
    times = [n / 50 for n in range(len(wall))]
    assert wall[19] != wall[18] != wall[20], "boundary 11: one refresh"
    keep, kept = wall[:19] + wall[20:], times[:19] + times[20:]
    report = wa.analyse(keep, 30, times=kept, refresh_hz=50, locked=True)
    assert report["unverified"]["boundaries"] == 1 and report["lost_boundaries"] == 0


def test_hidden_refreshes_are_all_a_blind_span_can_hold_but_one():
    # The wall's period is known to RATE_TOLERANCE (0.1 %): a span of a
    # whole nominal period may hold two refreshes of a faster wall.
    p = 1 / 60
    cases = [(0.6, 0), (0.998, 0), (1.0, 1), (1.99, 1), (2.0, 2), (2.02, 2), (2.99, 2)]
    for span, hidden in cases:
        assert wa.hidden_refreshes(span * p, p) == hidden, span
    # The instants' own error lengthens it too.
    assert wa.hidden_refreshes(0.99 * p, p, error=0.0002) == 1


def test_unseen_refreshes_add_the_unread_pictures_and_the_blind_ones():
    reading = wa.Reading("ok", 10, None, 0, 8)
    prev, run = wa.Boundary(0, 0, reading), wa.Boundary(3, 3, reading)
    p = 1 / 60

    def unseen(blind, period=p):
        return wa.unseen_refreshes(
            prev, run, wa.Timing([0.0] * 4, blind, period, False, None)
        )

    assert unseen(None) == 2, "two unread pictures"
    assert unseen([math.inf, 0.5 * p, 0.5 * p, 0.5 * p]) == 2
    assert unseen([math.inf, 0.9 * p, 3.01 * p, 2.5 * p]) == 7
    assert unseen([math.inf, p, p, p], None) == 2


def test_held_range_brackets_a_boundary_by_what_the_capture_saw():
    def ranges(readings, times, blind=None, lock=False, span=None):
        runs = wa.boundaries(readings)
        blind = wa.capture_blind(times, blind)
        timing = wa.Timing(times, blind, 1 / 60, lock, span)
        return [wa.held_range(n, runs, timing) for n in range(len(runs))]

    readings = pacer_readings(30, Fraction(0), 4, frames=1800)
    exact = at_60_hz(readings)
    inf = math.inf
    assert ranges(readings, exact, lock=True) == [(2, inf), (2, 2), (2, 2), (2, inf)]
    # A recording at the wall's rate, unlocked: each transition anywhere in
    # the period before the picture that shows it, the period itself known
    # to 0.1 %: 1..3 refreshes.
    off = [t + 0.4 / 60 for t in exact]
    off[2:] = [t + 0.3 / 60 for t in off[2:]]
    assert ranges(readings, off)[1:3] == [(1, 3), (1, 3)]
    # A partly read boundary: on screen at 0 and 2 periods: 3 refreshes.
    part = [readings[0], wa.Reading("no_strip"), readings[1]] + readings[2:]
    times = at_60_hz(part)
    # Unlocked, two instants a nominal 2 periods apart may lie in only 2
    # refreshes of a wall 0.1 % slower; locked, they are 3 refreshes.
    assert ranges(part, times)[0][0] == 2
    assert ranges(part, times, lock=True)[0][0] == 3
    assert ranges(part, times, lock=True)[1] == (2, 2)
    # Read at 0, 1, 3 and 4 periods with an unreadable one between: 5.
    hold = readings[:2] + [wa.Reading("no_strip"), readings[1], readings[1]]
    hold += readings[2:]
    assert ranges(hold, at_60_hz(hold))[0][0] == 4
    assert ranges(hold, at_60_hz(hold), lock=True)[0][0] == 5
    # The capture's last boundary (from 0.1 s), held to the capture's end
    # (1.0 s): 54 periods, so 55 refreshes.
    assert ranges(readings, exact, lock=True, span=(0, 1.0))[3] == (55, inf)


def test_a_frozen_wall_is_never_clean():
    # dxcam's pictures are the wall's presents: few of them = a stopped wall.
    readings = [pacer_readings(30, Fraction(100), 1, frames=1800)[0]]
    report = wa.analyse(
        readings, 30, times=[0.0], refresh_hz=60, blind=[math.inf], span=(0.0, 20.0)
    )
    assert report["frozen"] is True and report["verdict"] == "defects"
    two = pacer_readings(30, Fraction(100), 2, frames=1800)[::2]
    report = wa.analyse(
        two, 30, times=[0.0, 10.0], refresh_hz=60, blind=[math.inf, 0.001],
        span=(0.0, 20.0),
    )  # fmt: skip
    assert report["frozen"] is True and report["verdict"] == "defects"
    readings = pacer_readings(30, Fraction(0), 320, frames=1800, refreshes=1)
    presents = [math.inf] + [0.001] * 319
    times = [n / 14 for n in range(280)]
    report = wa.analyse(
        readings[:280], 30, times=times, refresh_hz=60, blind=presents[:280]
    )
    assert report["frozen"]
    times = [n / 16 for n in range(320)]
    assert not wa.analyse(readings, 30, times=times, refresh_hz=60, blind=presents)[
        "frozen"
    ]
    # A recording keeps its frames whatever the wall does (review round 6:
    # a 14 fps camera read as a stopped wall): never frozen, just coarse.
    times = [n / 14 for n in range(280)]
    report = wa.analyse(readings[:280], 30, times=times, refresh_hz=60)
    assert report["frozen"] is False and report["verdict"] != "clean"


def test_too_few_boundaries_judged_is_incomplete():
    # A locked recording that dropped one frame of 5 boundaries of 40: the
    # boundary and the next one are not judged (28 of 40).
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    times = at_60_hz(readings)
    drop = {2 * k + 1 for k in (4, 12, 20, 28, 36)}
    keep = [r for n, r in enumerate(readings) if n not in drop]
    kept = [t for n, t in enumerate(times) if n not in drop]
    report = wa.analyse(
        keep, 30, times=kept, refresh_hz=60, locked=True, min_boundaries=1
    )
    assert report["unverified"]["steps"] == 0 and report["lost_boundaries"] == 0
    assert report["refreshes"]["judged"] == 28 and report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete"


def test_the_wall_s_rate_must_be_given():
    readings = pacer_readings(30, Fraction(0), 340, frames=1800)
    report = wa.analyse(readings, 30, times=at_60_hz(readings), locked=True)
    assert report["refreshes"] is None and report["verdict"] == "incomplete"
    with pytest.raises(ValueError, match="below the 30 fps grid"):
        wa.analyse(readings, 30, times=at_60_hz(readings), refresh_hz=25)


def test_an_unreadable_picture_beside_a_transition_leaves_it_open():
    # dxcam on time, but boundary 5's first refresh and boundary 8's last
    # are unreadable: either could have been the boundary or the one
    # beside it, so neither count is proven — never judder.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    readings[10] = readings[17] = wa.Reading("no_strip")
    report = dxcam(readings, min_boundaries=1)
    assert report["refreshes"]["uneven"] == 0
    assert report["refreshes"]["judged"] == 34, "not 4, 5, 8, 9 nor the edges"


def test_the_capture_s_last_boundary_is_held_to_its_end_by_instants():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    times = [n / 60 + 0.0005 for n in range(len(readings))]
    blind = [math.inf] + [0.001] * (len(readings) - 1)
    a = times[-2]  # the last boundary's first picture
    for held, uneven in ((1.9, 0), (2.3, 1)):
        report = wa.analyse(
            readings, 30, times=times, refresh_hz=60, blind=blind,
            span=(0.0, a + held / 60),
        )  # fmt: skip
        assert report["refreshes"]["uneven"] == uneven, held


def test_a_capture_unresolved_in_one_place_is_never_clean():
    # 200 fps everywhere (0.3 of a period apart), but around boundaries
    # 100-104 the recorder kept only every third picture (0.9 of a period
    # apart: nothing hidden, every transition known only to 0.9 of a
    # period): some of those boundaries stay open between 1 and 2 or 2 and 3.
    wall = wall_refreshes([2] * 360, 25)
    readings, times = camera(wall, 60, 200, offset=0.0003)
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["verdict"] == "clean"
    first = round(100 * 2 / 60 * 200)
    keep = [n for n in range(len(times)) if not first <= n < first + 36 or n % 3 == 0]
    report = wa.analyse(
        [readings[n] for n in keep], 25, times=[times[n] for n in keep], refresh_hz=60
    )
    assert report["refreshes"]["unresolved"] > 0
    assert report["refreshes"]["judged"] >= 0.9 * report["boundaries"]
    assert report["refreshes"]["uneven"] == report["unverified"]["steps"] == 0
    assert report["verdict"] == "incomplete"


def test_capture_blind_is_the_time_since_the_picture_before():
    assert wa.capture_blind([0.0, 0.1, 0.3], None) == [math.inf, 0.1, 0.3 - 0.1]
    assert wa.capture_blind([0.0], [0.5]) == [0.5]


def test_a_frozen_wall_is_a_defect_even_with_no_rate():
    readings = pacer_readings(30, Fraction(100), 1, frames=1800)[:1]
    report = wa.analyse(readings, 30, times=[0.0], blind=[math.inf], span=(0.0, 20.0))
    assert report["refreshes"] is None
    assert report["frozen"] is True and report["verdict"] == "defects"


def test_a_recording_s_pts_quantization_is_never_a_hidden_refresh():
    # A 60 fps recording of a 60 Hz wall whose pts sit on a 1/10000 s
    # timebase: intervals of 16.6 and 16.7 ms, never a refresh hidden;
    # a frame it dropped still is one.
    readings = pacer_readings(30, Fraction(0), 340, frames=1800)
    times = [round(n / 60, 4) for n in range(len(readings))]
    assert max(b - a for a, b in zip(times, times[1:])) > 1 / 60
    assert 0 < wa.pts_jitter(times) <= 0.0001, "two pts each off by 0.05 ms"
    report = wa.analyse(readings, 30, times=times, refresh_hz=60, locked=True)
    assert report["capture"]["blind_pictures"] == 0
    assert report["verdict"] == "clean"
    keep = readings[:100] + readings[101:]
    kept = times[:100] + times[101:]
    report = wa.analyse(keep, 30, times=kept, refresh_hz=60, locked=True)
    assert report["capture"]["blind_pictures"] == 1
    assert wa.pts_jitter([0.0, 1.0]) == wa.pts_jitter([0.0]) == 0.0


# Review round 6: real captures' timing is never exact.


def test_unreadable_pictures_after_the_last_boundary_do_not_stretch_it():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    report = dxcam(readings + [wa.Reading("no_strip")] * 2, min_boundaries=1)
    assert report["refreshes"]["uneven"] == 0, "the trailing presents are others"
    # Locked too (review round 7): the look before an unreadable picture is
    # the picture before it, the last boundary's own last one.
    report = locked(readings + [wa.Reading("no_strip")] * 2, min_boundaries=1)
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "clean"


def test_a_grab_preempted_before_its_acquire_never_reads_as_judder():
    # Review round 7: dxcam stamps a poll before its grab; a poll preempted
    # until 2-40 ms later takes the frame on screen THEN. Its acquire
    # window (``late``) keeps the picture's content within it.
    readings = pacer_readings(30, Fraction(0), 120, frames=1800)
    for delay in (0.002, 0.005, 0.010, 0.020, 0.040):
        for at in range(10, 200, 7):
            # The poll at ``at`` is stamped ``delay`` before its acquire,
            # which takes the refresh on screen then (``shown``); the
            # refreshes in between no poll saw.
            times = [n / 60 + 0.0005 for n in range(len(readings))]
            shown = at + math.floor(delay * 60)
            kept = readings[:at] + readings[shown:]
            times = times[:at] + [times[shown] - delay] + times[shown + 1 :]
            late = [0.0] * at + [delay] + [0.0] * (len(times) - at - 1)
            blind = [math.inf] + [0.001] * (len(times) - 1)
            report = wa.analyse(
                kept, 30, times=times, refresh_hz=60, blind=blind, late=late
            )
            assert report["verdict"] != "defects", (delay, at)
            assert report["refreshes"]["inconsistent"] == 0, (delay, at)


@pytest.mark.parametrize(
    ("cam_fps", "timebase"), [(120.02, 90000), (119.9, 1000), (240.05, 90000)]
)
def test_a_camera_s_quantized_pts_never_read_a_clean_wall_as_judder(cam_fps, timebase):
    wall = wall_refreshes([2] * 400, 25)
    readings, times = camera(wall, 60, cam_fps, offset=0.0037)
    times = [round(t * timebase) / timebase for t in times]
    report = wa.analyse(readings, 25, times=times, refresh_hz=60)
    assert report["refreshes"]["uneven"] == report["lost_boundaries"] == 0
    assert report["cadence_errors"] == 0 and report["verdict"] != "defects"


def test_a_slow_camera_s_quantized_pts_never_read_as_a_loss():
    wall = wall_refreshes([2] * 400, 25)
    readings, times = camera(wall, 60, 29.97, offset=0.0037)
    for timebase in (90000, 10000, 1000, 600):
        quantized = [round(t * timebase) / timebase for t in times]
        report = wa.analyse(readings, 25, times=quantized, refresh_hz=60)
        assert report["lost_boundaries"] == 0, timebase
        assert report["verdict"] != "defects", timebase


@pytest.mark.parametrize(("true_hz", "given"), [(60, 59.979), (60, 59.94), (59.94, 60)])
def test_a_rate_within_its_tolerance_never_reads_a_clean_wall_as_judder(true_hz, given):
    readings = pacer_readings(30, Fraction(0), 400, frames=1800)
    times = [n / true_hz + 0.0005 for n in range(len(readings))]
    blind = [math.inf] + [0.001] * (len(readings) - 1)
    wa.check_present_rate(times, given, blind)
    report = wa.analyse(readings, 30, times=times, refresh_hz=given, blind=blind)
    assert report["refreshes"]["uneven"] == 0 and report["verdict"] == "clean"


def test_judder_dxcam_was_blind_to_is_never_clean():
    # Review round 6: a stall of the box hid each judder from dxcam too
    # (49 ms with no poll), and the open boundaries beside it read clean.
    counts = judder(420, every=40)
    readings = wall_refreshes(counts, 30, start=Fraction(0))
    times = [n / 60 + 0.0005 for n in range(len(readings))]
    blind = [math.inf] + [0.001] * (len(readings) - 1)
    drop = set()
    refresh = 0
    for k, count in enumerate(counts):
        if count == 1 and 0 < k:
            drop |= {refresh, refresh + 1}  # the 1-refresh one, the 3's first
            blind[refresh + 2] = 0.049
        refresh += count
    keep = [n for n in range(len(readings)) if n not in drop]
    report = wa.analyse(
        [readings[n] for n in keep], 30, times=[times[n] for n in keep],
        refresh_hz=60, blind=[blind[n] for n in keep],
    )  # fmt: skip
    assert report["refreshes"]["unresolved"] > 0
    assert report["verdict"] != "clean"


def test_each_capture_s_instants_carry_their_own_error():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    report = dxcam(readings, min_boundaries=1)
    assert report["capture"]["time_error_ms"] == 0.5, "dxcam: POLL_TIME_ERROR"
    times = [round(n / 60, 4) for n in range(len(readings))]
    report = wa.analyse(readings, 30, times=times, refresh_hz=60, locked=True)
    assert 0.03 <= report["capture"]["time_error_ms"] <= 0.1, "the pts' jitter"
    report = wa.analyse(readings, 30, times=at_60_hz(readings), refresh_hz=60)
    assert report["capture"]["time_error_ms"] == 0.001, "exact pts: PTS_TIME_ERROR"


def test_a_camera_exposure_that_mixes_two_boundaries_is_no_cadence_error():
    # Review round 7: a 120 fps camera's exposure taken across a boundary
    # change reads between the two. Each boundary's first picture a mix.
    readings = []
    for k in range(100, 420):
        mix = wa.Reading("ok", k - 1, k, 102, 8)  # 0.4 of the way to k
        readings += [mix] + [pacer_readings(30, Fraction(k), 1, frames=1800)[0]] * 3
    times = [n / 120 + 0.0013 for n in range(len(readings))]
    report = wa.analyse(readings, 30, times=times, refresh_hz=60)
    assert report["cadence_errors"] == 0 and report["lost_boundaries"] == 0
    assert report["undecoded"] == {"mixed": 320}, "the first one too"
    assert report["verdict"] != "defects"
    # A mix seen twice is a boundary of its own: off the grid, a defect.
    doubled = []
    for n in range(0, len(readings), 4):
        doubled += [readings[n]] * 2 + readings[n + 1 : n + 4]
    at_150 = [n / 150 + 0.0013 for n in range(len(doubled))]  # 5 a boundary
    stuck = wa.analyse(doubled, 30, times=at_150, refresh_hz=60)
    assert stuck["cadence_errors"] > 0 and stuck["verdict"] == "defects"
    # dxcam's presents are never exposures.
    blind = [math.inf] + [0.001] * (len(times) - 1)
    presents = wa.analyse(readings, 30, times=times, refresh_hz=60, blind=blind)
    assert presents["cadence_errors"] > 0


def test_an_impossible_bracket_is_the_capture_s_fault_never_judder():
    # Review round 7's mechanism: a poll stamped 5 ms before its acquire
    # (no acquire window known) puts boundary 20 5 ms early: boundary 19
    # reads at most 1 refresh, boundary 20 at least 3 — no count fits
    # either: ``inconsistent``, incomplete, never judder.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    times = [n / 60 + 0.0005 for n in range(len(readings))]
    times[40] -= 0.005
    blind = [math.inf] + [0.001] * (len(times) - 1)
    report = wa.analyse(
        readings, 30, times=times, refresh_hz=60, blind=blind, min_boundaries=1
    )
    assert report["refreshes"]["inconsistent"] == 2
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete"
    # Its acquire window known, both are judged, and the wall is clean.
    late = [0.0] * len(times)
    late[40] = 0.005
    report = wa.analyse(
        readings,
        30,
        times=times,
        refresh_hz=60,
        blind=blind,
        late=late,
        min_boundaries=1,
    )
    assert report["refreshes"]["inconsistent"] == 0
    assert report["verdict"] == "clean"
