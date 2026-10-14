"""Tests for scripts/wall_frames.py (#272): ``analyse`` (what a run of
readings, one per captured refresh, means), the capture glue and ``main``.

* the pacer's output blended by the program's own 8-bit arithmetic reads
  clean at every grid phase (512 per frame) at 24 / 25 / 30 fps;
* a clean 25-on-30 run through a loop of the clip, then a lost boundary, a
  lost frame, a jump back, a wrong weight, judder and a stall (also at the
  capture's edges) and a frozen wall, each counted where it belongs;
* what the capture did not see (an unreadable picture, a blind span)
  excuses at most one boundary per refresh a boundary takes, and such a
  capture, or one that read or judged too little, is incomplete;
* the capture glue: pts gaps and late polls (each at its threshold), the
  video read's arguments, ``main`` on a video and on a simulated dxcam
  capture.
"""

from __future__ import annotations

import json
import time
from fractions import Fraction
from pathlib import Path

import pytest

import av_sync_check
import wall_frames as wf
from wall_fakes import (
    at_60_hz,
    dxcam_capture,
    frame,
    pacer_readings,
    reading_at,
    two_boundaries,
)


def test_every_grid_phase_of_the_program_s_own_blend_reads_clean():
    # Review rounds 1-2: the pacer's exact output read as cadence errors at
    # phases inside its dead band, then as weight mismatches where the
    # program's 8-bit blend rounds a weight near 256 across the reader's
    # edge. Every phase, in 512ths of a frame, reads clean.
    for fps in (24, 25):
        for k in range(512):
            readings = pacer_readings(fps, 100 + Fraction(k, 512), 30, frames=1500)
            times = at_60_hz(readings)
            report = wf.analyse(readings, fps, times=times, min_boundaries=30)
            assert report["verdict"] == "clean", (fps, k, report)
    for k in range(0, 512, 37):
        readings = pacer_readings(30, 100 + Fraction(k, 512), 30, frames=1500)
        report = wf.analyse(readings, 30, refresh_hz=60, min_boundaries=30)
        assert report["verdict"] == "clean", k


def test_a_clean_25_on_30_capture_through_a_loop_is_clean():
    readings = pacer_readings(25, Fraction(40), 60, frames=60)
    report = wf.analyse(readings, 25, frames=60, refresh_hz=60, min_boundaries=50)
    assert report["samples"] == report["decoded"] == 120
    assert report["boundaries"] == 60
    assert report["refreshes_per_boundary"] == {"2": 60}
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
    # At the seam: the song's last boundary holds frame 59 (no next frame
    # to blend: a short step, and the 42 it would show), then the new play
    # starts frame 0 one step later (a 1-frame step on a 5/6 grid).
    assert report["loop_seams"] == {"steps": 3, "errors": 3}
    assert report["verdict"] == "clean"


def test_a_clean_30_on_30_capture_counts_every_frame():
    readings = pacer_readings(30, Fraction(10), 40, frames=1800, refreshes=2)
    report = wf.analyse(readings, 30, refresh_hz=60, min_boundaries=40)
    assert report["frames_in_clip"] == 1800
    assert report["frames_seen"] == 40
    assert report["verdict"] == "clean"


def test_a_30_on_30_loop_is_a_seamless_wrap():
    # 55..59, 0..59, 0..14: two loops, every frame numbered on across them.
    readings = pacer_readings(30, Fraction(55), 80, frames=60)
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=70)
    assert report["loops"] == 2
    assert report["frames_seen"] == 80
    assert report["loop_seams"] == {"steps": 6, "errors": 0}
    assert report["verdict"] == "clean"


def test_a_jump_to_the_start_from_beyond_the_last_frames_is_a_jump_back():
    readings = pacer_readings(30, Fraction(46), 5, frames=60)  # 46..50
    readings += pacer_readings(30, Fraction(0), 5, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 0, "50 is 10 frames before the end: no loop"
    assert report["jumps_back"] == 1
    # The last WRAP_FRAMES frames (57..59) to the first: a loop (58 and 59
    # never shown: at the seam); from 56: a jump back.
    for last, loops in ((57, 1), (56, 0)):
        readings = pacer_readings(30, Fraction(last - 4), 5, frames=60)
        readings += pacer_readings(30, Fraction(0), 5, frames=60)
        report = wf.analyse(readings, 30, frames=60)
        assert (report["loops"], report["jumps_back"]) == (loops, 1 - loops), last


def test_a_few_wrong_boundaries_do_not_move_the_fitted_phase():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    for n in (3, 9, 15):
        readings[n] = wf.Reading("ok", n, n + 1, 102, 8)
    report = wf.analyse(readings, 30)
    assert report["weights"]["phase_frames"] == [pytest.approx(0, abs=0.003)]
    assert report["weights"]["mismatches"] == 3


def test_a_new_play_after_a_jump_back_is_fitted_on_its_own_phase():
    readings = pacer_readings(25, Fraction(300), 18, frames=1500)
    readings += pacer_readings(25, Fraction(100) + Fraction(1, 2), 18, frames=1500)
    report = wf.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["weights"]["phase_frames"] == [
        pytest.approx(0, abs=0.003),
        pytest.approx(0.5, abs=0.003),
    ]
    assert report["weights"]["mismatches"] == 0


def test_a_lost_boundary_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[10] == wf.Reading("ok", 4, 5, 42, 8)
    del readings[10:12]  # that boundary never shown
    report = wf.analyse(readings, 25)
    assert report["lost_boundaries"] == 1
    assert report["lost_frames"] == 0, "its frames show in its neighbours"
    assert report["verdict"] == "defects"


def test_a_lost_frame_is_counted():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    del readings[14:16]  # frame 7
    report = wf.analyse(readings, 30)
    assert report["lost_frames"] == 1 and report["lost_frame_list"] == [7]
    assert report["lost_boundaries"] == 1
    assert report["verdict"] == "defects"


def test_a_jump_back_is_counted_with_its_sample():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    readings[30:32] = [wf.Reading("ok", 3, 4, 85, 8)] * 2
    report = wf.analyse(readings, 25)
    assert report["jumps_back"] == 1
    assert report["jump_back_samples"] == [30]
    assert report["verdict"] == "defects"


def test_a_small_step_back_within_the_slack_is_a_cadence_error_not_a_jump():
    readings = pacer_readings(30, Fraction(0), 12, frames=1800)
    # After boundary 11 (11.0), one reading 10.922: 0.078 back, under the
    # 30 fps slack (0.091), over SAME_PICTURE (0.071).
    readings += [wf.Reading("ok", 10, 11, 236, 8)] * 2
    report = wf.analyse(readings, 30)
    assert report["jumps_back"] == 0
    assert report["cadence_errors"] == 1


def test_a_wrong_weight_is_counted():
    readings = pacer_readings(25, Fraction(0), 30, frames=1500)
    assert readings[8] == wf.Reading("ok", 3, 4, 85, 8)
    readings[8:10] = [wf.Reading("ok", 3, 4, 120, 8)] * 2
    report = wf.analyse(readings, 25)
    assert report["weights"]["mismatches"] == 1
    assert report["verdict"] == "defects"


def test_a_repeated_boundary_is_judder_at_a_known_refresh_rate():
    readings = pacer_readings(30, Fraction(0), 10, frames=1800)
    readings[4:4] = readings[4:6]  # boundary 2 shown for 4 refreshes
    report = wf.analyse(readings, 30)
    assert report["refreshes_per_boundary"] == {"2": 9, "4": 1}
    assert report["boundaries"] == 10
    assert report["refreshes"] is None, "no refresh rate, no refresh check"
    assert report["verdict"] == "incomplete", "clean needs the judder check"
    # At 60 Hz every boundary must be shown twice: a 4-refresh one is judder.
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["refreshes"] == {
        "refresh_hz": 60,
        "expected": [2],
        "judged": 10,
        "uneven": 1,
        "uneven_samples": [4],
        "refresh_from": "given",
    }
    assert report["verdict"] == "defects"


def test_uneven_refreshes_are_judder():
    # Every frame shown, in order, but 3 / 1 refreshes instead of 2 / 2.
    readings = []
    for b in range(20):
        readings += [reading_at(Fraction(b), 30, 1800)] * (3 if b % 2 == 0 else 1)
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["lost_frames"] == report["cadence_errors"] == 0
    assert report["refreshes_per_boundary"] == {"1": 10, "3": 10}
    # Every boundary, the capture's first one held 3 (too long) included;
    # its last one, shown 1, may have gone on after the capture.
    assert report["refreshes"]["uneven"] == 19
    assert report["refreshes"]["uneven_samples"][:3] == [0, 3, 4]
    assert report["verdict"] == "defects"
    # A capture rate the grid does not divide allows both counts around it.
    report = wf.analyse(readings, 30, refresh_hz=50)
    assert report["refreshes"]["expected"] == [1, 2]
    assert report["refreshes"]["uneven"] == 10, "the 3-refresh ones"


def test_a_wall_holding_a_picture_while_the_capture_polls_is_a_stall():
    # Desktop duplication delivers no new picture while the wall holds one:
    # the capture times jump 200 ms, with every poll on time (no blind span).
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    times = at_60_hz(readings)
    keep = readings[:14] + readings[26:]  # boundaries 7..12 never presented
    kept_times = times[:14] + times[26:]
    report = wf.analyse(keep, 30, times=kept_times)
    assert report["lost_boundaries"] == 6
    assert report["lost_frames"] == 6
    assert report["refreshes"]["uneven"] == 1, "boundary 6 held 200+ ms"
    assert report["unverified"] == {"steps": 0, "boundaries": 0, "frames": 0}
    assert report["verdict"] == "defects"
    # The same gap where the capture itself was blind: unverified, as far as
    # the unseen refreshes reach. Boundary 13's first refresh fell in the
    # gap too: the boundaries on both sides of it are not judged.
    keep = readings[:14] + readings[27:]
    kept_times = times[:14] + times[27:]
    blind = [(kept_times[13], kept_times[14])]
    report = wf.analyse(keep, 30, times=kept_times, blind_spans=blind)
    assert report["lost_boundaries"] == 0 and report["lost_frames"] == 0
    assert report["unverified"] == {"steps": 1, "boundaries": 6, "frames": 6}
    assert report["refreshes"]["uneven"] == 0, "the boundaries beside the gap"
    assert report["capture"]["blind_spans"] == 1
    assert report["verdict"] == "incomplete"


def test_one_unreadable_picture_can_hide_one_boundary_and_no_more():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    # Boundary 7 on screen for one refresh, unreadable: excused.
    one = readings[:14] + [wf.Reading("no_strip")] + readings[16:]
    report = wf.analyse(one, 30)
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_boundaries"] == report["lost_frames"] == 0
    assert report["verdict"] == "incomplete"
    # A 1000-frame skip across that one picture: one boundary excused.
    skip = readings[:14] + [wf.Reading("no_strip")]
    skip += pacer_readings(30, Fraction(1010), 10, frames=1800)
    report = wf.analyse(skip, 30)
    assert report["lost_boundaries"] == 1002
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_frames"] == 1002
    assert report["verdict"] == "defects"


def test_an_off_grid_step_is_a_cadence_error_even_across_an_unreadable_picture():
    readings = pacer_readings(25, Fraction(0), 20, frames=1500)
    readings[14:16] = [wf.Reading("no_strip")] * 2
    readings[16:18] = [wf.Reading("ok", 6, 7, 140, 8)] * 2  # 6.547, not 6.667
    report = wf.analyse(readings, 25)
    assert report["cadence_errors"] >= 1
    assert report["verdict"] == "defects"


def test_undecoded_pictures_are_counted_and_lower_the_coverage():
    readings = pacer_readings(30, Fraction(0), 5, frames=1800)
    readings[1] = wf.Reading("no_strip")  # boundary 0's second refresh
    readings.append(wf.Reading("multi", strips=8))
    report = wf.analyse(readings, 30, times=at_60_hz(readings))
    assert report["undecoded"] == {"no_strip": 1, "multi": 1}
    assert report["boundaries"] == 5
    assert report["capture"]["interval_ms_p50"] == pytest.approx(16.667, abs=0.01)
    assert report["capture"]["blind_spans"] == 0
    assert report["refreshes"]["refresh_hz"] == pytest.approx(60)
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete", "9 of 11 pictures read"


def test_a_capture_that_read_too_little_is_incomplete_never_clean():
    readings = pacer_readings(30, Fraction(0), 3, frames=1800)
    readings += [wf.Reading("no_strip")] * 300
    report = wf.analyse(readings, 30)
    assert report["decoded"] == 6
    assert report["lost_frames"] == report["lost_boundaries"] == 0
    assert report["verdict"] == "incomplete"


def test_a_partly_read_boundary_is_judged_only_when_already_too_long():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    # Boundary 5's second refresh unreadable: it may have shown either
    # count, so it is not judged.
    readings[11] = wf.Reading("no_strip")
    report = wf.analyse(readings, 30, times=at_60_hz(readings), min_boundaries=38)
    assert report["refreshes"]["uneven"] == 0
    assert report["refreshes"]["judged"] == 38, "boundaries 5 and 6 not"
    assert report["unverified"]["steps"] == 0
    assert report["verdict"] == "clean", "one unread picture in 80"
    # Boundary 5 read over 3 refresh periods with an unreadable picture
    # between: on screen at least 3 refreshes whatever that one showed
    # (review round 4: one unreadable picture inside a 200 ms hold hid the
    # stall). A dxcam picture may come up to half a refresh after its
    # present, so 2 periods between the reads are not yet proof of 3.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    unread = wf.Reading("no_strip")
    for held, uneven in (([unread], 0), ([unread] * 2, 1)):
        hold = readings[:10] + [readings[10], *held, readings[11]] + readings[12:]
        report = wf.analyse(hold, 30, times=at_60_hz(hold), min_boundaries=38)
        assert report["refreshes"]["uneven"] == uneven, held
    assert report["refreshes"]["uneven_samples"] == [10]
    assert report["verdict"] == "defects"
    hold = pacer_readings(30, Fraction(0), 40, frames=1800)
    hold[10:12] = [hold[10]] * 6 + [wf.Reading("no_strip")] + [hold[10]] * 5
    report = wf.analyse(hold, 30, refresh_hz=60)
    assert report["refreshes"]["uneven_samples"] == [10]


def test_the_loop_seam_s_boundaries_are_not_judged_for_judder():
    readings = pacer_readings(30, Fraction(55), 40, frames=60)
    # The wrap is from 59 (boundary 4) to 0 (boundary 5): seam steps 3, 4
    # and 5 lead to boundaries 4, 5 and 6. The song's end holds 59 longer and
    # the new play's second boundary comes late: neither is judged.
    readings[8:10] = readings[8:10] * 3  # 59 for 6 refreshes
    readings[16:18] = readings[16:18] * 2  # 1 (boundary 6) for 4
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=37)
    assert report["loops"] == 1
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "clean"
    # Judder on the boundary BEFORE the song's last one is judged.
    readings[6:8] = readings[6:8] * 2  # 58 (boundary 3) for 4
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60)
    assert report["refreshes"]["uneven"] == 1
    assert report["refreshes"]["uneven_samples"] == [6]


def test_a_frame_missing_at_the_loop_seam_is_a_seam_error_not_a_loss():
    readings = pacer_readings(30, Fraction(54), 5, frames=60)  # 54..58
    readings += pacer_readings(30, Fraction(0), 30, frames=60)  # 59 never shown
    report = wf.analyse(readings, 30, frames=60, refresh_hz=60, min_boundaries=32)
    assert report["loops"] == 1
    assert report["lost_frames"] == 0
    assert report["loop_seams"]["errors"] == 2, "the 2-step wrap + frame 59"
    assert report["verdict"] == "clean"


def test_video_recording_gaps_are_blind_spans():
    times = [0, 1 / 30, 2 / 30, 4 / 30, 5 / 30, 6 / 30]
    assert wf.pts_gaps(times) == [(2 / 30, 4 / 30)]
    assert wf.pts_gaps([0.0]) == []
    # Exactly CAPTURE_GAP times the median is still on time.
    assert wf.pts_gaps([0.0, 1.0, 2.0, 3.5]) == []


def test_a_poll_later_than_a_refresh_is_a_blind_span_and_one_on_time_is_not():
    polls = [0.0, 0.001, 0.002, 0.012, 0.013, 0.014, 0.0315]
    # At 60 Hz a 10 ms gap holds at most one present: the next poll gets it.
    assert wf.late_polls(polls, 1 / 60) == [(0.014, 0.0315)]
    assert wf.late_polls(polls, 0.008) == [(0.002, 0.012), (0.014, 0.0315)]
    assert wf.late_polls(polls, None) == []
    assert wf.late_polls([0.0], 1 / 60) == []
    # Exactly one period apart: on time.
    assert wf.late_polls([0.0, 0.25], 0.25) == []


def test_an_empty_capture_is_not_clean():
    report = wf.analyse([wf.Reading("no_strip")] * 3, 25)
    assert report["decoded"] == 0 and report["boundaries"] == 0
    assert report["verdict"] == "defects"


def test_a_video_is_read_frame_by_frame_as_stored_with_its_pts():
    # One command line for the A/V gate's decode_video and the wall reader.
    args = av_sync_check.gray_video_args("ffmpeg", "rec.mp4", 960, 540)
    assert args[0] == "ffmpeg" and args[args.index("-i") + 1] == "rec.mp4"
    assert args[args.index("-fps_mode") + 1] == "passthrough"
    graph = args[args.index("-vf") + 1]
    assert graph.startswith("showinfo,scale=960:540"), graph
    assert args[args.index("-pix_fmt") + 1] == "gray"
    assert "-v" not in args and "-loglevel" not in args, "showinfo logs at info"
    assert args[-1] == "pipe:1"
    assert not any("ffprobe" in a for a in args)
    assert "-ss" not in args and "-t" not in args
    # The A/V gate's trim and crop: input options, the crop before the scale.
    args = av_sync_check.gray_video_args(
        "ffmpeg", "rec.mp4", 960, 540, start=1.5, duration=2.0, crop=(8, 4, 640, 360)
    )
    assert (
        args.index("-ss") < args.index("-i") and args[args.index("-ss") + 1] == "1.500"
    )
    assert args.index("-t") < args.index("-i") and args[args.index("-t") + 1] == "2.000"
    graph = args[args.index("-vf") + 1]
    assert graph.startswith("showinfo,crop=640:360:8:4:exact=1,scale=960:540"), graph


def test_main_reads_a_video_and_exits_by_the_verdict(monkeypatch, capsys):
    clean = [wf.read_frame(frame(n, 30, 480, 270)) for n in range(6)]
    times = [n / 30 for n in range(6)]
    videos = {
        "clean.mp4": (clean, times),
        "lost.mp4": (clean[:2] + clean[3:], times[:5]),
        "dropped.mp4": (clean[:2] + clean[3:], times[:2] + times[3:]),
        "blank.mp4": ([wf.Reading("no_strip")] * 3, times[:3]),
        "pal.mp4": (clean[:5], [n / 25 for n in range(5)]),
    }

    def fake_video(path, width, x_range):
        assert width == 480 and x_range == (0.0, 1.0)
        if path.name == "broken.mp4":
            raise RuntimeError("ffmpeg exited 1 reading broken.mp4: Invalid data")
        return videos[path.name]

    monkeypatch.setattr(wf, "read_video", fake_video)
    base = ["--fps", "30", "--grid-fps", "30", "--width", "480"]
    # Six pictures are too short for clean: 300 judged boundaries by default.
    assert wf.main([*base, "--video", "clean.mp4"]) == 3
    assert json.loads(capsys.readouterr().out)["min_boundaries"] == 300
    args = [*base, "--min-boundaries", "6", "--video"]
    assert wf.main([*args, "clean.mp4"]) == 0
    report = json.loads(capsys.readouterr().out)
    assert report["boundaries"] == 6 and report["verdict"] == "clean"
    assert report["refreshes"]["refresh_hz"] == pytest.approx(30)
    assert report["refreshes"]["refresh_from"] == "capture"
    # The same 30 fps recording of a 60 Hz wall saw every other refresh:
    # blind everywhere, so it cannot be clean.
    assert wf.main([*args[:-1], "--refresh-hz", "60", "--video", "clean.mp4"]) == 3
    report = json.loads(capsys.readouterr().out)
    assert report["capture"]["blind_spans"] == 5
    assert report["refreshes"]["judged"] == 0
    # A frame missing while the pts run on: the wall lost it.
    assert wf.main([*args, "lost.mp4"]) == 1
    assert json.loads(capsys.readouterr().out)["lost_frame_list"] == [2]
    # A frame missing with its pts: the recorder dropped it (blind span).
    assert wf.main([*args, "dropped.mp4"]) == 3
    assert json.loads(capsys.readouterr().out)["unverified"]["frames"] == 1
    assert wf.main([*args, "blank.mp4"]) == 4
    assert json.loads(capsys.readouterr().out)["decoded"] == 0
    # A capture slower than the 30 fps grid (a 25 fps camera, no wall rate
    # given) or a reading that fails is exit 5, never a wall's defect.
    assert wf.main([*args, "pal.mp4"]) == 5
    assert "below the 30 fps grid" in capsys.readouterr().err
    assert wf.main([*args, "broken.mp4"]) == 5
    assert "ffmpeg exited 1" in capsys.readouterr().err
    with pytest.raises(SystemExit):
        wf.main(["--fps", "15", "--video", "clean.mp4"])
    capsys.readouterr()


# Review round 3: positive evidence before "clean", edges, the excuse
# arithmetic, grouping, the band consensus and the loop guard.


def test_a_frozen_wall_is_never_clean():
    # One frame presented every refresh for 20 s: the capture's only
    # boundary, held far too long.
    readings = [reading_at(Fraction(100), 30, 1800)] * 1200
    report = wf.analyse(readings, 30, times=at_60_hz(readings))
    assert report["refreshes"]["uneven"] == 1 and report["verdict"] == "defects"
    # Arena stopped presenting: one picture in a 20 s capture.
    report = wf.analyse(readings[:1], 30, times=[0.0], span=(0.0, 20.0))
    assert report["frozen"] is True and report["refreshes"] is None
    assert report["verdict"] == "defects"
    # Two pictures 10 s apart.
    two = [reading_at(Fraction(100), 30, 1800), reading_at(Fraction(101), 30, 1800)]
    report = wf.analyse(two, 30, times=[0.0, 10.0], refresh_hz=60, span=(0.0, 20.0))
    assert report["frozen"] is True and report["verdict"] == "defects"


def test_a_stall_at_the_capture_s_start_or_end_is_judder():
    readings = pacer_readings(30, Fraction(0), 20, frames=1800)
    held = [readings[0]] * 600 + readings[2:]  # boundary 0 held 10 s
    report = wf.analyse(held, 30, times=at_60_hz(held))
    assert report["refreshes"]["uneven"] == 1
    assert report["refreshes"]["uneven_samples"] == [0]
    assert report["verdict"] == "defects"
    tail = readings[:-2] + [readings[-1]] * 600  # boundary 19 held 10 s
    report = wf.analyse(tail, 30, times=at_60_hz(tail))
    assert report["refreshes"]["uneven"] == 1 and report["verdict"] == "defects"
    # The last boundary runs to the capture's end even with no new picture.
    times = at_60_hz(readings)
    report = wf.analyse(readings, 30, times=times, span=(0.0, times[-1] + 10.0))
    assert report["refreshes"]["uneven"] == 1 and report["verdict"] == "defects"


def test_too_few_boundaries_judged_for_judder_is_incomplete():
    # The recording kept one picture of each boundary's two and was blind
    # in between: no boundary's refreshes can be judged.
    readings = pacer_readings(30, Fraction(0), 30, frames=1800)[::2]
    times = at_60_hz(readings * 2)[::2]
    blind = list(zip(times, times[1:]))
    report = wf.analyse(readings, 30, times=times, refresh_hz=60, blind_spans=blind)
    assert report["lost_boundaries"] == 0 and report["refreshes"]["uneven"] == 0
    assert report["refreshes"]["judged"] < 0.9 * report["boundaries"]
    assert report["verdict"] == "incomplete"


def test_a_blind_span_excuses_a_boundary_per_two_unseen_refreshes_at_60_hz():
    # Long enough that the two boundaries beside the gap leave 90 % judged:
    # only the excused step makes the second capture incomplete.
    readings = pacer_readings(30, Fraction(0), 60, frames=1800)
    times = at_60_hz(readings)
    # The recording dropped ONE picture and the wall lost boundary 8:
    # nothing can hide a boundary in one refresh.
    keep = readings[:16] + readings[18:]
    kept = times[:16] + [t - 1 / 60 for t in times[18:]]
    gap = [(kept[15], kept[16])]
    report = wf.analyse(keep, 30, times=kept, blind_spans=gap)
    assert report["lost_boundaries"] == 1
    assert report["unverified"]["boundaries"] == 0
    assert report["verdict"] == "defects"
    # Two dropped pictures (boundary 8 whole): one boundary excused.
    keep = readings[:16] + readings[18:]
    kept = times[:16] + times[18:]
    gap = [(kept[15], kept[16])]
    report = wf.analyse(keep, 30, times=kept, blind_spans=gap, min_boundaries=50)
    assert report["lost_boundaries"] == 0
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["decoded"] == report["samples"]
    assert report["refreshes"]["judged"] >= 50
    assert report["verdict"] == "incomplete"


def test_excused_frames_follow_the_clip_s_own_step_at_25_fps():
    readings = pacer_readings(25, Fraction(0), 20, frames=1500)
    # Boundary 3 (2.5) is read, then 5 unreadable pictures, then boundary 12
    # (10.0): 9 grid steps, 5 boundaries excused, 3 lost. The excused ones
    # reach 2.5 + 5 x 5/6 = 6.67: frames 4..7 unseen, 8 and 9 lost.
    hidden = readings[:8] + [wf.Reading("no_strip")] * 5 + readings[24:]
    report = wf.analyse(hidden, 25)
    assert report["unverified"] == {"steps": 1, "boundaries": 5, "frames": 4}
    assert report["lost_boundaries"] == 3
    assert report["lost_frame_list"] == [8, 9]


def test_losses_after_a_loop_are_numbered_in_the_clip():
    readings = pacer_readings(30, Fraction(55), 15, frames=60)  # 55..59, 0..9
    # After the loop: boundary 9 (frame 4) one unreadable picture, boundary
    # 10 (frame 5) never shown.
    hidden = readings[:18] + [wf.Reading("no_strip")] + readings[22:]
    report = wf.analyse(hidden, 30, frames=60)
    assert report["loops"] == 1
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_frame_list"] == [5]


def test_one_boundary_read_grey_then_plain_is_one_boundary():
    # A weight near 256 rounds either way of the level edge from refresh to
    # refresh: still one picture.
    readings = pacer_readings(30, Fraction(0), 10, frames=1800)
    readings[8:10] = [wf.Reading("ok", 3, 4, 245, 8), wf.Reading("ok", 4, None, 0, 8)]
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["boundaries"] == 10
    assert report["cadence_errors"] == 0


def test_a_jump_from_the_clip_s_end_to_its_middle_is_a_jump_back():
    readings = pacer_readings(30, Fraction(55), 5, frames=60)  # 55..59
    readings += pacer_readings(30, Fraction(30), 5, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 0
    assert report["jumps_back"] == 1
    assert report["verdict"] == "defects"


def test_showinfo_pts_are_read_the_av_gate_s_way():
    stderr = "\n".join(
        [
            "[Parsed_showinfo_0 @ 0x1] n:   0 pts:      0 pts_time:0       ",
            "something else pts_time:9",
            "[Parsed_showinfo_0 @ 0x1] n:   1 pts:   1001 pts_time:0.0333667",
        ]
    )
    assert av_sync_check.showinfo_pts(stderr, "rec.mp4") == [0.0, 0.0333667]


def test_only_a_blind_span_between_the_two_pictures_counts_their_refreshes():
    prev, run, times = two_boundaries(10)
    t1 = times[-1]
    # A stall (10 refreshes, polls on time): nothing unseen.
    assert wf.unseen_refreshes(prev, run, times, 1 / 60, []) == 0
    # A blind span that ends at the first picture or starts at the second,
    # or lies elsewhere, is not between them.
    for span in ((-0.05, 0.0), (t1, t1 + 0.05), (t1 + 1, t1 + 2)):
        assert wf.unseen_refreshes(prev, run, times, 1 / 60, [span]) == 0, span
    assert wf.unseen_refreshes(prev, run, times, 1 / 60, [(0.0, t1)]) == 9


def test_a_blind_gap_hides_every_present_it_can_hold():
    def unseen(gap: float, unread: int = 0) -> int:
        prev, run, times = two_boundaries(gap, unread)
        return wf.unseen_refreshes(prev, run, times, 1 / 60, [(0.0, times[-1])])

    # (t0, t0 + 2.2 periods] can hold three presents, the newest read.
    assert unseen(2.2) == 2
    # A recording that dropped one frame (2 periods, +-5 % jitter).
    assert (unseen(1.96), unseen(2.0), unseen(2.04)) == (1, 1, 1)
    # Just over a period: two presents, the older one never read.
    assert unseen(1.02) == 1
    # Blind (a dxcam poll over half a refresh late) but under a period:
    # no present hidden, only the timing lost.
    assert unseen(0.8) == 0
    # Never fewer than the unreadable pictures between them.
    assert unseen(2.04, unread=2) == 2


def test_at_50_hz_a_boundary_can_hide_in_one_refresh():
    # 50 Hz on the 30 fps grid: a boundary is shown 1 or 2 refreshes, so
    # each unseen refresh can hide one.
    readings = pacer_readings(30, Fraction(0), 20, frames=1800, refreshes=1)
    readings[8:10] = [wf.Reading("no_strip")] * 2
    report = wf.analyse(readings, 30, refresh_hz=50)
    assert report["unverified"]["boundaries"] == 2
    assert report["lost_boundaries"] == 0


def test_under_half_a_picture_per_grid_slot_is_frozen():
    readings = pacer_readings(30, Fraction(0), 320, frames=1800, refreshes=1)
    times = [n / 14 for n in range(280)]
    report = wf.analyse(readings[:280], 30, times=times, refresh_hz=60)
    assert report["frozen"] is True
    times = [n / 16 for n in range(320)]
    assert wf.analyse(readings, 30, times=times, refresh_hz=60)["frozen"] is False


def test_a_capture_that_decoded_too_little_is_incomplete_on_its_own():
    # The capture began before the clip was on the wall: 14 pictures with no
    # strip, then 60 clean boundaries. Nothing unverified, every boundary
    # judged, coverage 0.896.
    readings = [wf.Reading("no_strip")] * 14
    readings += pacer_readings(30, Fraction(0), 60, frames=1800)
    times = at_60_hz(readings)
    report = wf.analyse(readings, 30, times=times, min_boundaries=60)
    assert report["unverified"]["steps"] == 0
    assert report["refreshes"]["judged"] == 60
    assert report["verdict"] == "incomplete"
    # From the clip's first picture on: clean.
    report = wf.analyse(readings[14:], 30, times=times[14:], min_boundaries=60)
    assert report["verdict"] == "clean"


def test_three_quarters_of_the_boundaries_judged_is_incomplete():
    # The recording dropped one picture of 5 boundaries of 40 (a blind
    # span each): the boundary and the next one are not judged.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    times = at_60_hz(readings)
    drop = {2 * k + 1 for k in (4, 12, 20, 28, 36)}
    keep = [r for n, r in enumerate(readings) if n not in drop]
    kept = [t for n, t in enumerate(times) if n not in drop]
    blind = [(times[n - 1], times[n + 1]) for n in sorted(drop)]
    report = wf.analyse(
        keep, 30, times=kept, refresh_hz=60, blind_spans=blind, min_boundaries=30
    )
    assert report["unverified"]["steps"] == 0 and report["lost_boundaries"] == 0
    assert report["refreshes"]["judged"] == 30 and report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "incomplete"


def test_an_excused_boundary_reaches_from_the_blend_it_follows():
    # 25 fps from 0: boundary 1 is 0.833 (frames 0 / 1 at 213), then one
    # unreadable picture where boundaries 2-4 should be, then boundary 5
    # (4.167). 3 skipped, 1 excused: it could have shown up to 0.833 +
    # 0.833 = 1.67, so frame 2 is unverified and frame 3 lost.
    readings = pacer_readings(25, Fraction(0), 12, frames=1500)
    hidden = readings[:4] + [wf.Reading("no_strip")] + readings[10:]
    report = wf.analyse(hidden, 25)
    assert report["unverified"] == {"steps": 1, "boundaries": 1, "frames": 1}
    assert report["lost_boundaries"] == 2
    assert report["lost_frame_list"] == [3]


def test_the_capture_s_first_boundary_may_have_begun_before_it():
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)[1:]
    report = wf.analyse(readings, 30, times=at_60_hz(readings), min_boundaries=40)
    assert report["refreshes"]["uneven"] == 0
    assert report["verdict"] == "clean"


def test_the_new_play_s_phase_is_fitted_away_from_the_seam():
    # 25 fps, a 60-frame clip from 40: 24 boundaries to the song's end,
    # then the new play's 0, 0.833, 1.667. The first two (beside the seam)
    # read wrong: the phase is fitted on the third alone, which then
    # matches it. Fitted on all three, the two wrong ones move it and the
    # third reads as a mismatch.
    readings = pacer_readings(25, Fraction(40), 27, frames=60)
    readings[48:52] = [wf.Reading("ok", 0, 1, 100, 8)] * 2 + [
        wf.Reading("ok", 0, 1, 150, 8)
    ] * 2
    report = wf.analyse(readings, 25, frames=60)
    assert report["loops"] == 1
    assert len(report["weights"]["phase_frames"]) == 2
    assert report["weights"]["mismatches"] == 0


def test_main_reads_a_dxcam_capture_by_its_polls(monkeypatch, capsys):
    captures = {
        "clean": dxcam_capture(),
        # No poll for 45 ms over boundary 15: blind, so it is unverified.
        "late": dxcam_capture(late=(0.49, 0.535)),
        # The wall stopped after the clip's last present, the polls ran on.
        "stopped": dxcam_capture(after=1.0),
        # Every grab costs 20 ms (> a refresh): blind after every picture,
        # so a clean wall is never judged, never read as judder (review
        # round 4: 17-22 ms grabs read a clean wall as defects).
        "slow": dxcam_capture(grab=0.020),
        # Grabs of 10 ms: the poll after a picture is late but finds
        # nothing; the next picture comes on a timely poll: all judged.
        "half": dxcam_capture(grab=0.010),
        # No poll for 12 ms before boundary 15's first present is caught: its
        # time is 11 ms after its present, too far to count refreshes by
        # (judged, boundaries 14 and 15 would read 3 and 1 refreshes).
        "lag": dxcam_capture(late=(0.4995, 0.5115)),
    }
    asked = []

    def fake_dxcam(output, region, seconds, x_range):
        assert (output, region, seconds, x_range) == (1, None, 2.0, (0.0, 1.0))
        return captures[asked[-1]]

    monkeypatch.setattr(wf, "capture_dxcam", fake_dxcam)
    args = ["--fps", "30", "--dxcam", "1", "--seconds", "2", "--min-boundaries", "28"]
    with pytest.raises(SystemExit) as refused:
        wf.main(args)  # dxcam's 1 ms polls cannot give the wall's rate
    assert refused.value.code == 2
    assert "--refresh-hz" in capsys.readouterr().err
    args += ["--refresh-hz", "60"]
    # A present rate that is no whole number of the given refreshes: the
    # given rate is not the wall's.
    asked.append("clean")
    assert wf.main([*args[:-1], "45"]) == 5
    assert "no whole number of 45 Hz refreshes" in capsys.readouterr().err
    expected = {"clean": 0, "late": 3, "slow": 3, "stopped": 1, "half": 0, "lag": 0}
    reports = {}
    for name, code in expected.items():
        asked.append(name)
        assert wf.main(args) == code, name
        reports[name] = json.loads(capsys.readouterr().out)
        assert reports[name]["boundaries"] == (29 if name == "late" else 30), name
    for name, judged in (("slow", 0), ("half", 30), ("lag", 28)):
        assert reports[name]["refreshes"]["uneven"] == 0, name
        assert reports[name]["refreshes"]["judged"] == judged, name
        assert reports[name]["lost_boundaries"] == 0, name
    assert reports["stopped"]["refreshes"]["uneven_samples"] == [58]


def test_a_clean_capture_needs_its_minimum_of_judged_boundaries():
    # Review round 4: one judged boundary out of one read clean; a wall
    # that loses a boundary every second read clean over 0.3 s.
    assert wf.MIN_BOUNDARIES == 300, "10 s on the 30 fps grid"
    readings = pacer_readings(30, Fraction(0), 300, frames=1800)
    report = wf.analyse(readings, 30, refresh_hz=60)
    assert report["refreshes"]["judged"] == 300 and report["verdict"] == "clean"
    report = wf.analyse(readings[:-2], 30, refresh_hz=60)
    assert report["refreshes"]["judged"] == 299
    assert report["verdict"] == "incomplete"
    assert wf.analyse(readings[:2], 30, refresh_hz=60)["verdict"] == "incomplete"


def test_a_seam_never_hides_a_skip_into_or_out_of_it():
    # 30 fps, a 60-frame clip: 40..45, then straight to 59 (13 boundaries
    # skipped into the song's end), then the loop.
    readings = pacer_readings(30, Fraction(40), 6, frames=60)
    readings += pacer_readings(30, Fraction(59), 30, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["lost_boundaries"] == 13 and report["lost_frames"] == 13
    assert report["verdict"] == "defects"
    # The new play reaches 0, then jumps on to 30.
    readings = pacer_readings(30, Fraction(55), 6, frames=60)  # 55..59, 0
    readings += pacer_readings(30, Fraction(30), 30, frames=60)
    report = wf.analyse(readings, 30, frames=60)
    assert report["loops"] == 1
    assert report["lost_boundaries"] == 29
    assert report["verdict"] == "defects"

    # The seam's own steps: forward, at most two grid steps.
    def seam(before: float, after: float) -> set[int]:
        steps = [wf.Step(0, d, 1, False, 0, None) for d in (1.0, before, after, 1.0)]
        steps.insert(2, wf.Step(0, 1.0, 1, True, 0, None))
        return wf.seam_steps(steps, 30)

    assert seam(0.5, 2.0) == {1, 2, 3}
    assert seam(-0.2, 2.2) == {2}


def test_a_recording_slower_than_the_wall_refresh_is_blind_everywhere():
    times = [n / 30 for n in range(10)]
    assert wf.video_blind_spans(times, 60) == list(zip(times, times[1:]))
    assert wf.video_blind_spans(times, 30) == []
    assert wf.video_blind_spans(times, None) == []
    # Exactly CAPTURE_GAP refreshes per picture is still every refresh.
    assert wf.video_blind_spans([0.0, 0.025, 0.05], 60) == []
    # A 60 Hz recording keeps only its pts gaps.
    times = [0, 1 / 60, 2 / 60, 4 / 60, 5 / 60]
    assert wf.video_blind_spans(times, 60) == [(2 / 60, 4 / 60)]
    # A 30 fps camera on a 60 Hz wall sees one picture per boundary, a
    # 1 + 3 judder too: it can judge no boundary, so it is never clean.
    readings = pacer_readings(30, Fraction(0), 400, frames=1800)[::2]
    times = [n / 30 for n in range(len(readings))]
    blind = wf.video_blind_spans(times, 60)
    report = wf.analyse(readings, 30, times=times, refresh_hz=60, blind_spans=blind)
    assert report["lost_boundaries"] == 0 and report["refreshes"]["judged"] == 0
    assert report["verdict"] == "incomplete"


def test_a_refresh_rate_below_the_grid_is_refused():
    with pytest.raises(ValueError, match="below the 30 fps grid"):
        wf.expected_refreshes(25, 30)
    assert wf.expected_refreshes(29.97, 30) == [1]
    assert wf.expected_refreshes(50, 30) == [1, 2]
    with pytest.raises(ValueError, match="give the wall's --refresh-hz"):
        wf.analyse(pacer_readings(30, Fraction(0), 5, frames=1800), 30, refresh_hz=25)


def fake_ffmpeg(directory: Path, frames: bytes, pts: list[str], tail: str) -> str:
    """A stand-in ffmpeg: the size probe (``-f null``) answers 480x270; the
    read writes ``frames`` to stdout and a showinfo line per ``pts`` to
    stderr, then runs ``tail``."""
    (directory / "frames.raw").write_bytes(frames)
    info = "".join(
        f"[Parsed_showinfo_0 @ 0x1] n:{n} pts:{n} pts_time:{p} s:480x270\n"
        for n, p in enumerate(pts)
    )
    (directory / "pts.txt").write_text(info)
    script = directory / "ffmpeg"
    script.write_text(
        "#!/bin/sh\n"
        'case " $* " in *" -f null "*)\n'
        "  echo '[Parsed_showinfo_0 @ 0x1] n:0 pts:0 pts_time:0 s:480x270' >&2\n"
        "  exit 0;;\n"
        "esac\n"
        f"cat '{directory}/pts.txt' >&2\n"
        f"cat '{directory}/frames.raw'\n" + tail
    )
    script.chmod(0o755)
    return str(script)


def test_read_video_reads_each_stored_frame_with_its_pts(tmp_path):
    pictures = [frame(n, 30, 480, 270) for n in (7, 8, 9)]
    raw = b"".join(p.tobytes() for p in pictures)
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0", "0.0333", "0.0667"], "exit 0\n")
    readings, times = wf.read_video(tmp_path / "rec.mp4", None, (0.0, 1.0), ffmpeg)
    assert [r.index for r in readings] == [7, 8, 9]
    assert times == [0.0, 0.0333, 0.0667]
    # A half frame at the end is not a picture; the pts then disagree.
    ffmpeg = fake_ffmpeg(tmp_path, raw[:-100], ["0", "0.0333", "0.0667"], "")
    with pytest.raises(RuntimeError, match="read 2 pictures but 3 pts"):
        wf.read_video(tmp_path / "rec.mp4", None, (0.0, 1.0), ffmpeg)
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0", "0.0333", "0.0667"], "exit 1\n")
    with pytest.raises(RuntimeError, match="ffmpeg exited 1"):
        wf.read_video(tmp_path / "rec.mp4", None, (0.0, 1.0), ffmpeg)


def test_read_video_kills_ffmpeg_when_the_read_fails(tmp_path):
    raw = frame(7, 30, 480, 270).tobytes()
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0"], "exec sleep 30\n")
    began = time.monotonic()
    with pytest.raises(ValueError, match="outside the capture"):
        wf.read_video(tmp_path / "rec.mp4", None, (0.5, 1.5), ffmpeg)
    assert time.monotonic() - began < 10, "ffmpeg was waited for, not killed"


def test_a_blind_step_beside_a_boundary_stops_its_judging():
    # Every picture read, but boundary 5's first one came on a poll 10 ms
    # late: nothing hidden, but its time is the late poll's, so boundaries
    # 4 and 5 are not timed.
    readings = pacer_readings(30, Fraction(0), 40, frames=1800)
    times = at_60_hz(readings)
    late = [(times[10] - 0.010, times[10])]
    report = wf.analyse(readings, 30, times=times, refresh_hz=60, blind_spans=late)
    assert report["unverified"]["steps"] == 0
    assert report["refreshes"]["judged"] == 38
    runs = wf.boundaries(readings)
    steps = wf.steps_between(runs, 1800, 30, 30, times, 1 / 60, late)
    assert [n for n, s in enumerate(steps) if s.blind] == [4]
    assert [s.unseen for s in steps] == [0] * 39
    assert wf.late_timed(times, 10, late) and not wf.late_timed(times, 9, late)
    assert wf.hidden_presents(0.010, 1 / 60) == 0


def test_a_dxcam_present_rate_must_be_whole_refreshes():
    every = [n / 60 for n in range(30)]
    wf.check_present_rate(every, 60, [])
    wf.check_present_rate(every[::2], 60, [])  # Arena presenting at 30 Hz
    with pytest.raises(ValueError, match="every 16.667 ms"):
        wf.check_present_rate(every, 45, [])
    with pytest.raises(ValueError, match="no whole number of 120 Hz"):
        wf.check_present_rate([n / 80 for n in range(30)], 120, [])
    # Only timed intervals count, and too few are no check.
    blind = list(zip(every, every[1:]))
    wf.check_present_rate(every, 45, blind)
    wf.check_present_rate(every[: wf.MIN_TIMED], 45, [])


def test_a_boundary_is_held_at_least_from_its_first_to_its_last_timed_picture():
    reading = wf.Reading("ok", 7, None, 0, 8)
    run = wf.Boundary(0, 2, reading, 2)  # read at 0 and 2, 1 unreadable
    times = [0.0, 1 / 60, 2 / 60 + 0.03]
    # Timed: 63 ms less half a refresh = 3.3 refreshes apart, so in 4.
    assert wf.held_at_least(run, times, 60, []) == 4
    # The last picture came on a late poll: from 0 to the unreadable one.
    assert wf.held_at_least(run, times, 60, [(1 / 60, times[2])]) == 1
    assert wf.held_at_least(wf.Boundary(1, 1, reading), times, 60, []) == 1
    assert wf.held_at_least(run, None, 60, []) == 3, "one per sample"
