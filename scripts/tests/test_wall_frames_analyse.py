"""Tests for scripts/wall_analysis.py (#272): what a run of readings means,
one sample per wall refresh (no capture times; ``test_wall_timing.py``
covers a capture's own timing).

* the pacer's output blended by the program's own 8-bit arithmetic reads
  clean at every grid phase (512 per frame) at 24 / 25 / 30 fps;
* a clean 25-on-30 run through a loop of the clip, then a lost boundary, a
  lost frame, a jump back, a wrong weight and judder, each counted where it
  belongs; a loop seam excuses only its own irregularity and a hold up to
  ``SEAM_HOLD_S``;
* what the capture did not read excuses at most one boundary per refresh a
  boundary takes, and such a capture, or one that read or judged too
  little, is incomplete.
"""

from __future__ import annotations

from fractions import Fraction

import pytest

import wall_analysis as wa
from wall_fakes import pacer_readings, reading_at


def test_every_grid_phase_of_the_program_s_own_blend_reads_clean():
    # Review rounds 1-2: the pacer's exact output read as cadence errors at
    # phases inside its dead band, then as weight mismatches where the
    # program's 8-bit blend rounds a weight near 256 across the reader's
    # edge. Every phase, in 512ths of a frame, reads clean.
    for fps in (24, 25):
        for k in range(512):
            readings = pacer_readings(fps, 100 + Fraction(k, 512), 30, frames=1500)
            report = wa.analyse(readings, fps, refresh_hz=60, min_boundaries=1)
            assert report["verdict"] == "clean", (fps, k, report)
    for k in range(0, 512, 37):
        readings = pacer_readings(30, 100 + Fraction(k, 512), 30, frames=1500)
        report = wa.analyse(readings, 30, refresh_hz=60, min_boundaries=1)
        assert report["verdict"] == "clean", k


def test_a_clean_25_on_30_capture_through_a_loop_is_clean():
    readings = pacer_readings(25, Fraction(40), 60, frames=60)
    report = wa.analyse(readings, 25, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["samples"] == report["decoded"] == 120
    assert report["boundaries"] == 60
    assert report["pictures_per_boundary"] == {"2": 60}
    assert report["loops"] == 1
    assert report["lost_frames"] == 0
    assert report["lost_boundaries"] == 0
    assert report["jumps_back"] == 0
    assert report["cadence_errors"] == 0
    weights = report["weights"]
    # The pacer's 170 and 128 come out of the program's byte blend (16 / 235,
    # rounded) as levels the reader reads as 169 and 127: within the Q8
    # tolerance of the rule.
    assert weights["observed_first_cycle"] == [0, 213, 169, 127, 85, 42]
    assert weights["expected_cycle_fitted"] == [0, 213, 170, 128, 85, 42]
    assert weights["phase_frames"] == [pytest.approx(0, abs=0.002)] * 2
    assert weights["mismatches"] == 0
    # At the seam: the song's last boundary holds frame 59 plain (no next
    # frame to blend: a short step into it, and no weight), then the new
    # play starts frame 0 one step later (a 1-frame step on a 5/6 grid):
    # the clamp, so no loss and no cadence error.
    assert report["loop_seams"] == {"steps": 3, "weight_mismatches": 1, "hold_s": 0.033}
    assert report["verdict"] == "clean"


def test_a_clean_30_on_30_capture_counts_every_frame():
    readings = pacer_readings(30, Fraction(10), 40, frames=1800, refreshes=2)
    report = wa.analyse(readings, 30, refresh_hz=60, min_boundaries=1)
    assert report["frames_in_clip"] == 1800
    assert report["frames_seen"] == 40
    assert report["verdict"] == "clean"


def test_a_30_on_30_loop_is_a_seamless_wrap():
    # 55..59, 0..59, 0..14: two loops, every frame numbered on across them.
    readings = pacer_readings(30, Fraction(55), 80, frames=60)
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 2
    assert report["frames_seen"] == 80
    assert report["loop_seams"] == {"steps": 6, "weight_mismatches": 0, "hold_s": 0.033}
    assert report["verdict"] == "clean"


def test_a_jump_to_the_start_from_beyond_the_last_frames_is_a_jump_back():
    readings = pacer_readings(30, Fraction(46), 5, frames=60)  # 46..50
    readings += pacer_readings(30, Fraction(0), 5, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 0, "50 is 10 frames before the end: no loop"
    assert report["jumps_back"] == 1
    # The last WRAP_FRAMES frames (57..59) to the first: a loop (58 and 59
    # never shown: at the seam); from 56: a jump back.
    for last, loops in ((57, 1), (56, 0)):
        readings = pacer_readings(30, Fraction(last - 4), 5, frames=60)
        readings += pacer_readings(30, Fraction(0), 5, frames=60)
        report = wa.analyse(readings, 30, frames=60)
        assert (report["loops"], report["jumps_back"]) == (loops, 1 - loops), last


def test_a_few_wrong_boundaries_do_not_move_the_fitted_phase():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    for n in (3, 9, 15):
        readings[n] = wa.Reading("ok", n, n + 1, 102, 8)
    report = wa.analyse(readings, 30)
    assert report["weights"]["phase_frames"] == [pytest.approx(0, abs=0.003)]
    assert report["weights"]["mismatches"] == 3


def test_a_new_play_after_a_jump_back_is_fitted_on_its_own_phase():
    readings = pacer_readings(25, Fraction(300), 18, frames=1500)
    readings += pacer_readings(25, Fraction(100) + Fraction(1, 2), 18, frames=1500)
    report = wa.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["weights"]["phase_frames"] == [
        pytest.approx(0, abs=0.003),
        pytest.approx(0.5, abs=0.003),
    ]
    assert report["weights"]["mismatches"] == 0


def test_a_lost_boundary_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[10] == wa.Reading("ok", 4, 5, 42, 8)
    del readings[10:12]  # that boundary never shown
    report = wa.analyse(readings, 25)
    assert report["lost_boundaries"] == 1
    assert report["lost_frames"] == 0, "its frames show in its neighbours"
    assert report["verdict"] == "defects"


def test_a_lost_frame_is_counted():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    del readings[14:16]  # frame 7
    report = wa.analyse(readings, 30)
    assert report["lost_frames"] == 1 and report["lost_frame_list"] == [7]
    assert report["lost_boundaries"] == 1
    assert report["verdict"] == "defects"


def test_a_jump_back_is_counted_with_its_sample():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    readings[30:32] = [wa.Reading("ok", 3, 4, 85, 8)] * 2
    report = wa.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["jump_back_samples"] == [30]
    assert report["verdict"] == "defects"


def test_a_small_step_back_within_the_slack_is_a_cadence_error_not_a_jump():
    readings = pacer_readings(30, Fraction(0), 12, frames=1800)
    # After boundary 11 (11.0), one reading 10.922: 0.078 back, under the
    # 30 fps slack (0.091), over SAME_PICTURE (0.071).
    readings += [wa.Reading("ok", 10, 11, 236, 8)] * 2
    report = wa.analyse(readings, 30)
    assert report["jumps_back"] == 0
    assert report["cadence_errors"] == 1


def test_a_wrong_weight_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[8] == wa.Reading("ok", 3, 4, 85, 8)
    readings[8:10] = [wa.Reading("ok", 3, 4, 120, 8)] * 2
    report = wa.analyse(readings, 25)
    assert report["weights"]["mismatches"] == 1
    assert report["verdict"] == "defects"


def test_a_repeated_boundary_is_judder_at_a_known_refresh_rate():
    readings = pacer_readings(30, Fraction(0), 10, frames=1800)
    readings[4:4] = readings[4:6]  # boundary 2 shown for 4 refreshes
    report = wa.analyse(readings, 30)
    assert report["pictures_per_boundary"] == {"2": 9, "4": 1}
    assert report["boundaries"] == 10
    assert report["refreshes"] is None, "no refresh rate, no refresh check"
    assert report["verdict"] == "incomplete", "clean needs the judder check"
    # At 60 Hz every boundary must be shown twice: a 4-refresh one is judder.
    # The capture's first and last may have begun before it or gone on
    # after it: not judged.
    report = wa.analyse(readings, 30, refresh_hz=60)
    assert report["refreshes"] == {
        "refresh_hz": 60,
        "expected": [2],
        "judged": 8,
        "uneven": 1,
        "uneven_samples": [4],
        "uneven_boundaries": [{"boundary": 2, "refreshes": 4}],
        "unresolved": 0,
        "inconsistent": 0,
    }
    assert report["verdict"] == "defects"


def test_uneven_refreshes_are_judder():
    # Every frame shown, in order, but 3 / 1 refreshes instead of 2 / 2.
    readings = []
    for b in range(20):
        readings += [reading_at(Fraction(b), 30, 1800)] * (3 if b % 2 == 0 else 1)
    report = wa.analyse(readings, 30, refresh_hz=60)
    assert report["lost_frames"] == report["cadence_errors"] == 0
    assert report["pictures_per_boundary"] == {"1": 10, "3": 10}
    # Every boundary, the capture's first one held 3 (too long) included;
    # its last one, shown 1, may have gone on after the capture.
    assert report["refreshes"]["uneven"] == 19
    assert report["refreshes"]["uneven_samples"][:3] == [0, 3, 4]
    assert report["verdict"] == "defects"
    # A capture rate the grid does not divide allows both counts around it.
    report = wa.analyse(readings, 30, refresh_hz=50)
    assert report["refreshes"]["expected"] == [1, 2]
    assert report["refreshes"]["uneven"] == 10, "the 3-refresh ones"


def test_one_unreadable_picture_can_hide_one_boundary_and_no_more():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    # Boundary 7 on screen for one refresh, unreadable: excused.
    one = readings[:14] + [wa.Reading("no_strip")] + readings[16:]
    report = wa.analyse(one, 30)
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_boundaries"] == report["lost_frames"] == 0
    assert report["verdict"] == "incomplete"
    # A 1000-frame skip across that one picture: one boundary excused.
    skip = readings[:14] + [wa.Reading("no_strip")]
    skip += pacer_readings(30, Fraction(1010), 10, frames=1800)
    report = wa.analyse(skip, 30)
    assert report["lost_boundaries"] == 1002
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_frames"] == 1002
    assert report["verdict"] == "defects"


def test_an_off_grid_step_is_a_cadence_error_even_across_an_unreadable_picture():
    readings = pacer_readings(25, Fraction(0), 20, frames=1500)
    readings[14:16] = [wa.Reading("no_strip")] * 2
    readings[16:18] = [wa.Reading("ok", 6, 7, 140, 8)] * 2  # 6.547, not 6.667
    report = wa.analyse(readings, 25)
    assert report["cadence_errors"] >= 1
    assert report["verdict"] == "defects"


def test_undecoded_pictures_are_counted_and_lower_the_coverage():
    readings = pacer_readings(30, Fraction(0), 5, frames=1800)
    readings[1] = wa.Reading("no_strip")  # boundary 0's second refresh
    readings.append(wa.Reading("multi", strips=8))
    report = wa.analyse(readings, 30, refresh_hz=60, min_boundaries=1)
    assert report["undecoded"] == {"no_strip": 1, "multi": 1}
    assert report["boundaries"] == 5
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete", "9 of 11 pictures read"


def test_a_capture_that_read_too_little_is_incomplete_never_clean():
    readings = pacer_readings(30, Fraction(0), 3, frames=1800)
    readings += [wa.Reading("no_strip")] * 300
    report = wa.analyse(readings, 30)
    assert report["decoded"] == 6
    assert report["lost_frames"] == report["lost_boundaries"] == 0
    assert report["verdict"] == "incomplete"


def test_the_loop_seam_spares_only_the_new_play_s_open():
    readings = pacer_readings(30, Fraction(55), 60, frames=60)
    # The wrap is from 59 (boundary 4) to 0 (boundary 5). The song's end
    # holds 59 and the new play's first boundary comes late: the new play's
    # open, spared up to SEAM_HOLD_S.
    readings[8:10] = readings[8:10] * 3  # 59 for 6 refreshes
    readings[14:16] = readings[14:16] * 2  # 0 (boundary 5) for 4
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 1
    assert report["refreshes"]["uneven"] == 0
    assert report["loop_seams"]["hold_s"] == 0.1, "59 held 6 refreshes"
    assert report["verdict"] == "clean"
    # The new play's second boundary is the cadence again (review round 6:
    # a long stall there read clean), and so is the one before the song's
    # last.
    readings[18:20] = readings[18:20] * 2  # 1 (boundary 6) for 4
    readings[6:8] = readings[6:8] * 2  # 58 (boundary 3) for 4
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60)
    assert report["refreshes"]["uneven_samples"] == [6, 20]
    # A hold at the seam over SEAM_HOLD_S is a stall (review round 5: a 5 s
    # freeze on the song's last boundary read clean).
    assert wa.SEAM_HOLD_S == 1.0
    base = pacer_readings(30, Fraction(55), 40, frames=60)
    for held, uneven in ((62, 0), (63, 1), (300, 1)):
        frozen = base[:8] + [base[8]] * held + base[10:]  # 59, the song's last
        report = wa.analyse(frozen, 30, frames=60, refresh_hz=60)
        assert report["refreshes"]["uneven"] == uneven, held


def test_a_frame_missing_at_the_loop_seam_is_a_loss():
    # Review round 6: the seam excused one lost boundary. At 30 fps the seam
    # is regular (59, then 0): a 59 never shown is lost like any frame.
    readings = pacer_readings(30, Fraction(54), 5, frames=60)  # 54..58
    readings += pacer_readings(30, Fraction(0), 50, frames=60)  # 59 never shown
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 1
    assert report["lost_frame_list"] == [59] and report["lost_boundaries"] == 1
    assert report["verdict"] == "defects"
    # At 25 fps the new play's second boundary lost: lost, not the seam's.
    readings = pacer_readings(25, Fraction(40), 30, frames=60)
    del readings[50:52]  # 0.833, boundary 25
    report = wa.analyse(readings, 25, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 1 and report["lost_boundaries"] == 1
    assert report["verdict"] == "defects"


def test_the_song_end_clamp_is_no_loss_and_no_cadence_error():
    # Review round 6: at 24 / 25 fps the song's last boundary shows the last
    # frame plain wherever it is in it; one boundary the capture could not
    # read beside the seam then read as a cadence error.
    for fps in (24, 25):
        for sixth in range(36):  # every phase a sixth of a frame apart
            readings = pacer_readings(fps, Fraction(180 + sixth, 6), 60, frames=60)
            report = wa.analyse(
                readings, fps, frames=60, refresh_hz=60, min_boundaries=1
            )
            assert report["loops"] == 1, (fps, sixth)
            assert report["lost_boundaries"] == report["cadence_errors"] == 0
            assert report["jumps_back"] == 0 and report["verdict"] == "clean"
            seam = next(n for n, r in enumerate(readings) if r.index == 0)
            for gone in (seam - 2, seam):  # the song's last / the new play's first
                unread = [wa.Reading("no_strip")] * 2
                hidden = readings[:gone] + unread + readings[gone + 2 :]
                report = wa.analyse(hidden, fps, frames=60, refresh_hz=60)
                assert report["cadence_errors"] == report["lost_boundaries"] == 0
                assert report["jumps_back"] == 0, (fps, sixth, gone)
                assert report["verdict"] != "defects", (fps, sixth, gone)


def test_an_empty_capture_is_not_clean():
    report = wa.analyse([wa.Reading("no_strip")] * 3, 25)
    assert report["decoded"] == 0 and report["boundaries"] == 0
    assert report["verdict"] == "defects"


def test_excused_frames_follow_the_clip_s_own_step_at_25_fps():
    readings = pacer_readings(25, Fraction(0), 20, frames=1500)
    # Boundary 3 (2.5) is read, then 5 unreadable pictures, then boundary 12
    # (10.0): 9 grid steps, 5 boundaries excused, 3 lost. The excused ones
    # reach 2.5 + 5 x 5/6 = 6.67: frames 4..7 unseen, 8 and 9 lost.
    hidden = readings[:8] + [wa.Reading("no_strip")] * 5 + readings[24:]
    report = wa.analyse(hidden, 25)
    assert report["unverified"] == {"steps": 1, "boundaries": 5, "frames": 4}
    assert report["lost_boundaries"] == 3
    assert report["lost_frame_list"] == [8, 9]


def test_losses_after_a_loop_are_numbered_in_the_clip():
    readings = pacer_readings(30, Fraction(55), 15, frames=60)  # 55..59, 0..9
    # After the loop: boundary 9 (frame 4) one unreadable picture, boundary
    # 10 (frame 5) never shown.
    hidden = readings[:18] + [wa.Reading("no_strip")] + readings[22:]
    report = wa.analyse(hidden, 30, frames=60)
    assert report["loops"] == 1
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_frame_list"] == [5]


def test_one_boundary_read_grey_then_plain_is_one_boundary():
    # A weight near 256 rounds either way of the level edge from refresh to
    # refresh: still one picture.
    readings = pacer_readings(30, Fraction(0), 10, frames=1800)
    readings[8:10] = [wa.Reading("ok", 3, 4, 245, 8), wa.Reading("ok", 4, None, 0, 8)]
    report = wa.analyse(readings, 30, refresh_hz=60)
    assert report["boundaries"] == 10
    assert report["cadence_errors"] == 0


def test_a_jump_from_the_clip_s_end_to_its_middle_is_a_jump_back():
    readings = pacer_readings(30, Fraction(55), 5, frames=60)  # 55..59
    readings += pacer_readings(30, Fraction(30), 5, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 0
    assert report["jumps_back"] == 1
    assert report["verdict"] == "defects"


def test_at_50_hz_a_boundary_can_hide_in_one_refresh():
    # 50 Hz on the 30 fps grid: a boundary is shown 1 or 2 refreshes, so
    # each unseen refresh can hide one.
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    readings[8:10] = [wa.Reading("no_strip")] * 2
    report = wa.analyse(readings, 30, refresh_hz=50)
    assert report["unverified"]["boundaries"] == 2
    assert report["lost_boundaries"] == 0


def test_a_capture_that_decoded_too_little_is_incomplete_on_its_own():
    # The capture began before the clip was on the wall: 14 pictures with no
    # strip, then 60 clean boundaries. Nothing unverified, every inner
    # boundary judged, coverage 0.896.
    readings = [wa.Reading("no_strip")] * 14
    readings += pacer_readings(30, Fraction(0), 60, frames=1800)
    report = wa.analyse(readings, 30, refresh_hz=60, min_boundaries=58)
    assert report["unverified"]["steps"] == 0
    assert report["refreshes"]["judged"] == 58
    assert report["verdict"] == "incomplete"
    # From the clip's first picture on: clean.
    report = wa.analyse(readings[14:], 30, refresh_hz=60, min_boundaries=58)
    assert report["verdict"] == "clean"


def test_an_excused_boundary_reaches_from_the_blend_it_follows():
    # 25 fps from 0: boundary 1 is 0.833 (frames 0 / 1 at 213), then one
    # unreadable picture where boundaries 2-4 should be, then boundary 5
    # (4.167). 3 skipped, 1 excused: it could have shown up to 0.833 +
    # 0.833 = 1.67, so frame 2 is unverified and frame 3 lost.
    readings = pacer_readings(25, Fraction(0), 12, frames=1500)
    hidden = readings[:4] + [wa.Reading("no_strip")] + readings[10:]
    report = wa.analyse(hidden, 25)
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_boundaries"] == 2
    assert report["lost_frame_list"] == [3]


def test_the_new_play_s_phase_is_fitted_away_from_the_seam():
    # 25 fps, a 60-frame clip from 40: 24 boundaries to the song's end,
    # then the new play's 0, 0.833, 1.667. The first two (beside the seam)
    # read wrong: the phase is fitted on the third alone, which then
    # matches it. Fitted on all three, the two wrong ones move it and the
    # third reads as a mismatch.
    readings = pacer_readings(25, Fraction(40), 27, frames=60)
    readings[48:52] = [wa.Reading("ok", 0, 1, 100, 8)] * 2 + [
        wa.Reading("ok", 0, 1, 150, 8)
    ] * 2
    report = wa.analyse(readings, 25, frames=60)
    assert report["loops"] == 1
    assert len(report["weights"]["phase_frames"]) == 2
    assert report["weights"]["mismatches"] == 0


def test_a_clean_capture_needs_its_minimum_of_judged_boundaries():
    # Review round 4: one judged boundary out of one read clean; a wall
    # that loses a boundary every second read clean over 0.3 s.
    assert wa.MIN_BOUNDARIES == 300, "10 s on the 30 fps grid"
    readings = pacer_readings(30, Fraction(0), 302, frames=1800)
    report = wa.analyse(readings, 30, refresh_hz=60)
    assert report["refreshes"]["judged"] == 300 and report["verdict"] == "clean"
    report = wa.analyse(readings[:-2], 30, refresh_hz=60)
    assert report["refreshes"]["judged"] == 299
    assert report["verdict"] == "incomplete"
    assert wa.analyse(readings[:4], 30, refresh_hz=60)["verdict"] == "incomplete"


def test_a_seam_never_hides_a_skip_into_or_out_of_it():
    # 30 fps, a 60-frame clip: 40..45, then straight to 59 (13 boundaries
    # skipped into the song's end), then the loop.
    readings = pacer_readings(30, Fraction(40), 6, frames=60)
    readings += pacer_readings(30, Fraction(59), 30, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["lost_boundaries"] == 13 and report["lost_frames"] == 13
    assert report["verdict"] == "defects"
    # The new play reaches 0, then jumps on to 30.
    readings = pacer_readings(30, Fraction(55), 6, frames=60)  # 55..59, 0
    readings += pacer_readings(30, Fraction(30), 30, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["lost_boundaries"] == 29
    assert report["verdict"] == "defects"

    # The seam: the wrap and the step on each side.
    steps = [wa.Step(0, 1.0, 1, False, 0, None) for _ in range(5)]
    steps[2] = wa.Step(0, 1.0, 1, True, 0, None)
    assert wa.seam_steps(steps) == {1, 2, 3}
    assert wa.seam_steps(steps[2:]) == {0, 1}


def test_a_refresh_rate_below_the_grid_is_refused():
    with pytest.raises(ValueError, match="below the 30 fps grid"):
        wa.expected_refreshes(25, 30)
    assert wa.expected_refreshes(29.97, 30) == [1]
    assert wa.expected_refreshes(50, 30) == [1, 2]
    with pytest.raises(ValueError, match="give the wall's --refresh-hz"):
        wa.analyse(pacer_readings(30, Fraction(0), 5, frames=1800), 30, refresh_hz=25)


def test_a_seam_boundary_shown_too_short_is_judder():
    # Review round 6: a 1 + 3 pair on the new play's first boundary read
    # clean; the seam may hold a boundary longer, never show it shorter.
    readings = pacer_readings(30, Fraction(55), 60, frames=60)
    first = next(n for n, r in enumerate(readings) if r.index == 0)
    short = (
        readings[: first + 1]
        + readings[first + 2 : first + 4] * 2
        + readings[first + 4 :]
    )
    report = wa.analyse(short, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["refreshes"]["uneven_samples"][:1] == [first]
    assert report["verdict"] == "defects"


def test_a_capture_blind_across_the_loop_still_sees_the_loop():
    # 30 fps: 50..56 read, then 4 boundaries (57, 58, 59 and the new play's
    # 0) unreadable, then 1..: further from the end than WRAP_FRAMES, but
    # within what the capture could have missed: a loop, those 4 unverified.
    # Long enough that coverage passes, and both sides of the blind step are
    # seam boundaries (never unresolved): only the unverified step itself
    # keeps the verdict from clean.
    readings = pacer_readings(30, Fraction(0), 57, frames=60)
    readings += [wa.Reading("no_strip")] * 8
    readings += pacer_readings(30, Fraction(1), 40, frames=60)
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 1 and report["jumps_back"] == 0
    assert report["unverified"] == {"steps": 1, "boundaries": 4, "frames": 4}
    assert report["decoded"] / report["samples"] >= wa.MIN_COVERAGE
    assert report["refreshes"]["unresolved"] == 0
    assert report["verdict"] == "incomplete"


def test_the_new_play_must_sit_on_its_own_grid():
    # The whole new play 0.4 of a frame off its grid: every step inside it
    # is a whole step, so only the loop's own step can name the cadence.
    readings = pacer_readings(30, Fraction(55), 30, frames=60)
    first = next(n for n, r in enumerate(readings) if r.index == 0)
    readings[first:] = [
        wa.Reading("ok", r.index, r.index + 1, 102, 8) for r in readings[first:]
    ]
    report = wa.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 1 and report["cadence_errors"] == 1
    assert report["verdict"] == "defects"


def test_a_capture_ending_on_the_song_s_last_frame_has_no_weight_there():
    # 25 fps: the capture ends on the song's last boundary (59 plain, the
    # clamp), with no loop after it: no weight is expected of it.
    readings = pacer_readings(25, Fraction(40), 24, frames=60)
    assert readings[-1] == wa.Reading("ok", 59, None, 0, 8)
    report = wa.analyse(readings, 25, frames=60, refresh_hz=60, min_boundaries=1)
    assert report["loops"] == 0 and report["weights"]["mismatches"] == 0
    assert report["verdict"] == "clean"


def test_distinct_boundaries_count_a_re_shown_picture_once():
    # #279's A/B counts the missing pictures as the reference minus the
    # distinct pictures shown. A one-refresh jump back re-shows a picture:
    # one more run, no new picture, while lost_boundaries counts the step
    # out of it (19 -> 21) as a skip.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    readings[41] = readings[38]  # boundary 20's second refresh shows 19
    report = wa.analyse(readings, 30)
    assert report["jumps_back"] == 1
    assert report["boundaries"] == 41
    assert report["distinct_boundaries"] == 40
    assert report["lost_boundaries"] == 1
    # A back that re-shows two pictures, each after a newer one.
    readings = pacer_readings(30, Fraction(0), 21, frames=1800)  # 0..20
    readings += pacer_readings(30, Fraction(18), 2, frames=1800)  # 18, 19
    readings += pacer_readings(30, Fraction(21), 19, frames=1800)  # 21..39
    report = wa.analyse(readings, 30)
    assert report["boundaries"] == 42
    assert report["distinct_boundaries"] == 40


def test_distinct_boundaries_take_one_picture_read_two_ways_as_one():
    # Boundary 20 read near its next frame's weight (19.9375: within
    # SAME_PICTURE, past half of it), then re-shown read plain (20.0), and
    # the other way round: one picture each time.
    plain = wa.Reading("ok", 20, None, 0, 8)
    near = wa.Reading("ok", 19, 20, 240, 8)
    for first, again in ((near, plain), (plain, near)):
        readings = pacer_readings(30, Fraction(0), 40, frames=1800)
        readings[40:42] = [first] * 2
        readings[44] = again  # boundary 22's first refresh re-shows 20
        report = wa.analyse(readings, 30)
        assert report["boundaries"] == 41, first
        assert report["distinct_boundaries"] == 40, first


def test_distinct_boundaries_count_no_picture_a_frozen_or_dead_capture_missed():
    # Frozen for its second half: no skip between two readings, so
    # lost_boundaries reads 0; the distinct pictures stop at 20.
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    readings += [readings[-1]] * 40
    report = wa.analyse(readings, 30)
    assert report["boundaries"] == report["distinct_boundaries"] == 20
    assert report["lost_boundaries"] == 0
    # One picture throughout (one run, no step), then nothing decoded.
    assert wa.analyse([readings[0]] * 80, 30)["distinct_boundaries"] == 1
    dead = wa.analyse([wa.Reading("no_strip")] * 80, 30)
    assert dead["distinct_boundaries"] == 0


def test_distinct_boundaries_number_pictures_on_across_loops_and_miss_a_skip():
    # 55..59, 0..59, 0..14: the same frame in another loop is a new picture.
    readings = pacer_readings(30, Fraction(55), 80, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 2 and report["distinct_boundaries"] == 80
    # 25 on 30: a skipped boundary loses no clip frame, but one picture.
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    del readings[10:12]
    report = wa.analyse(readings, 25)
    assert report["lost_frames"] == 0
    assert report["distinct_boundaries"] == 29


def test_distinct_boundaries_group_a_re_show_by_the_run_s_first_reading():
    # As boundaries() groups: a picture is the same only within
    # SAME_PICTURE of the run's FIRST reading. 20.0 and 20.07 are one run;
    # 20.14, after 21, is another picture (0.14 from 20.0), though within
    # SAME_PICTURE of that run's last reading.
    readings = [
        wa.Reading("ok", 20, None, 0, 8),
        wa.Reading("ok", 20, 21, 18, 8),
        wa.Reading("ok", 21, None, 0, 8),
        wa.Reading("ok", 20, 21, 36, 8),
    ]
    report = wa.analyse(readings, 30)
    assert report["boundaries"] == 3
    assert report["distinct_boundaries"] == 3


def test_distinct_boundaries_split_one_picture_from_two_at_same_picture():
    # The edge as the reader reads it: weight 238 is 18/256 from the plain
    # next frame, within SAME_PICTURE (18.24/256): one picture; 237 is
    # 19/256, past it: two.
    for weight, distinct in ((238, 40), (237, 41)):
        readings = pacer_readings(30, Fraction(0), 40, frames=1800)
        readings[40:42] = [wa.Reading("ok", 19, 20, weight, 8)] * 2
        readings[44] = wa.Reading("ok", 20, None, 0, 8)
        report = wa.analyse(readings, 30)
        assert report["boundaries"] == 41, weight
        assert report["distinct_boundaries"] == distinct, weight


def test_distinct_boundaries_count_a_loop_from_the_step_that_wraps():
    # A capture whose first step is the wrap: its second boundary is
    # already the next loop's (59, 0..59, 0: 62 pictures).
    readings = pacer_readings(30, Fraction(59), 62, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["loops"] == 2 and report["distinct_boundaries"] == 62
    # The loop starts at the wrap's own boundary: 55..59, 0..19, a jump back
    # to 0 (a re-show in the same loop), then 20..29.
    readings = pacer_readings(30, Fraction(55), 25, frames=60)
    readings += pacer_readings(30, Fraction(0), 1, frames=60)
    readings += pacer_readings(30, Fraction(20), 10, frames=60)
    report = wa.analyse(readings, 30, frames=60)
    assert report["jumps_back"] == 1 and report["distinct_boundaries"] == 35
