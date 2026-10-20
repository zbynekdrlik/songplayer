"""Tests for scripts/wall_frames.py's capture glue and CLI (#272): the
video read through ``av_sync_check``'s command line and readers, a stand-in
ffmpeg, dxcam's polls (``poll_blind``, ``check_present_rate``), and
``main``'s exit codes for a video and a simulated dxcam capture (v2: the
copies found once per capture, and a capture that finds none).
"""

from __future__ import annotations

import json
import math
import time
from pathlib import Path

import numpy as np
import pytest

import av_sync_check
import wall_analysis as wa
import wall_clip as clip
import wall_frames as wf
from wall_fakes import dxcam_capture, frame


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
    clean = [wf.read_frame(frame(n, 30)) for n in range(30)]
    times = [n / 30 for n in range(30)]
    videos = {
        "clean.mp4": (clean, times),
        "lost.mp4": (clean[:2] + clean[3:], times[:29]),
        "dropped.mp4": (clean[:2] + clean[3:], times[:2] + times[3:]),
        "blank.mp4": ([wa.Reading("no_strip")] * 3, times[:3]),
        "pal.mp4": (clean[:25], [n / 25 for n in range(25)]),
    }

    def fake_video(path, width):
        assert width == 480
        if path.name == "broken.mp4":
            raise RuntimeError("ffmpeg exited 1 reading broken.mp4: Invalid data")
        return (*videos[path.name], None)

    monkeypatch.setattr(wf, "read_video", fake_video)
    base = ["--fps", "30", "--grid-fps", "30", "--width", "480"]
    # The wall's rate is required: a rate read from the capture is the
    # capture's (review round 5: a 30 fps camera called judder clean).
    with pytest.raises(SystemExit) as refused:
        wf.main([*base, "--video", "clean.mp4"])
    assert refused.value.code == 2
    assert "--refresh-hz" in capsys.readouterr().err
    base += ["--refresh-hz", "30"]
    # 30 pictures are too short for clean: 300 judged boundaries by default.
    assert wf.main([*base, "--video", "clean.mp4"]) == 3
    assert json.loads(capsys.readouterr().out)["min_boundaries"] == 300
    args = [*base, "--min-boundaries", "28", "--video"]
    # Each picture one refresh (--locked): every count proven. Unlocked, a
    # camera at the wall's rate known to 0.1 % cannot tell 1 from 2.
    for extra, code, judged in (([], 3, 0), (["--locked"], 0, 28)):
        assert wf.main([*extra, *args, "clean.mp4"]) == code, extra
        report = json.loads(capsys.readouterr().out)
        assert report["boundaries"] == 30
        assert report["refreshes"]["judged"] == judged, extra
    # The same 30 fps recording of a 60 Hz wall saw every other refresh: a
    # refresh hidden before each picture, so nothing can be judged.
    slow = ["--refresh-hz", "60", "--min-boundaries", "28", "--video"]
    assert wf.main([*base[:-2], *slow, "clean.mp4"]) == 3
    report = json.loads(capsys.readouterr().out)
    assert report["capture"]["blind_pictures"] == 29
    assert report["refreshes"]["judged"] == 0
    # Locked: a frame missing while the pts run on: the wall lost it.
    args = ["--locked", *args]
    assert wf.main([*args, "lost.mp4"]) == 1
    assert json.loads(capsys.readouterr().out)["lost_frame_list"] == [2]
    # A frame missing with its pts: the recorder dropped it (unverified).
    assert wf.main([*args, "dropped.mp4"]) == 3
    assert json.loads(capsys.readouterr().out)["unverified"]["frames"] == 1
    assert wf.main([*args, "blank.mp4"]) == 4
    assert json.loads(capsys.readouterr().out)["decoded"] == 0
    # A rate below the 30 fps grid (a 25 Hz wall?) or a reading that fails
    # is exit 5, never a wall's defect.
    assert wf.main([*base[:-1], "25", "--video", "pal.mp4"]) == 5
    assert "below the 30 fps grid" in capsys.readouterr().err
    assert wf.main([*args, "broken.mp4"]) == 5
    assert "ffmpeg exited 1" in capsys.readouterr().err
    with pytest.raises(SystemExit):
        wf.main(["--fps", "15", "--refresh-hz", "60", "--video", "clean.mp4"])
    capsys.readouterr()


def test_showinfo_pts_are_read_the_av_gate_s_way():
    stderr = "\n".join(
        [
            "[Parsed_showinfo_0 @ 0x1] n:   0 pts:      0 pts_time:0       ",
            "something else pts_time:9",
            "[Parsed_showinfo_0 @ 0x1] n:   1 pts:   1001 pts_time:0.0333667",
        ]
    )
    assert av_sync_check.showinfo_pts(stderr, "rec.mp4") == [0.0, 0.0333667]


def fake_ffmpeg(directory: Path, frames: bytes, pts: list[str], tail: str) -> str:
    """A stand-in ffmpeg: the size probe (``-f null``) answers 960x540; the
    read writes ``frames`` to stdout and a showinfo line per ``pts`` to
    stderr, then runs ``tail``."""
    (directory / "frames.raw").write_bytes(frames)
    info = "".join(
        f"[Parsed_showinfo_0 @ 0x1] n:{n} pts:{n} pts_time:{p} s:960x540\n"
        for n, p in enumerate(pts)
    )
    (directory / "pts.txt").write_text(info)
    script = directory / "ffmpeg"
    script.write_text(
        "#!/bin/sh\n"
        'case " $* " in *" -f null "*)\n'
        "  echo '[Parsed_showinfo_0 @ 0x1] n:0 pts:0 pts_time:0 s:960x540' >&2\n"
        "  exit 0;;\n"
        "esac\n"
        f"cat '{directory}/pts.txt' >&2\n"
        f"cat '{directory}/frames.raw'\n" + tail
    )
    script.chmod(0o755)
    return str(script)


def test_read_video_reads_each_stored_frame_with_its_pts(tmp_path):
    pictures = [frame(n, 30) for n in (7, 8, 9)]
    raw = b"".join(p.tobytes() for p in pictures)
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0", "0.0333", "0.0667"], "exit 0\n")
    readings, times, located = wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    assert [r.index for r in readings] == [7, 8, 9]
    assert [r.strips for r in readings] == [clip.BANDS * clip.COPIES] * 3
    assert times == [0.0, 0.0333, 0.0667]
    assert located.report() == {"copies": 9, "cell_px": 5.0, "region": None}
    # A half frame at the end is not a picture; the pts then disagree.
    ffmpeg = fake_ffmpeg(tmp_path, raw[:-100], ["0", "0.0333", "0.0667"], "")
    with pytest.raises(RuntimeError, match="read 2 pictures but 3 pts"):
        wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0", "0.0333", "0.0667"], "exit 1\n")
    with pytest.raises(RuntimeError, match="ffmpeg exited 1"):
        wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)


def test_read_video_finds_the_copies_on_the_first_picture_that_shows_them(tmp_path):
    blank = np.full((540, 960), clip.Y_BACKGROUND, dtype=np.uint8)
    pictures = [blank, frame(7, 30), blank, frame(9, 30)]
    raw = b"".join(p.tobytes() for p in pictures)
    pts = ["0", "0.0333", "0.0667", "0.1"]
    ffmpeg = fake_ffmpeg(tmp_path, raw, pts, "exit 0\n")
    readings, _, located = wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    assert [(r.status, r.index) for r in readings] == [
        ("no_strip", None),
        ("ok", 7),
        ("no_strip", None),
        ("ok", 9),
    ]
    assert located.report()["copies"] == 9
    # Located ONCE: a later picture is read at the copies found on the first
    # (its rulers covered, its strips still read), never looked for again.
    lay = clip.layout(960, 540)
    covered = frame(8, 30).copy()
    for y0, y1 in lay.rulers:
        covered[y0:y1] = clip.Y_BACKGROUND
    assert wf.read_frame(covered).status == "no_strip"
    raw = frame(7, 30).tobytes() + covered.tobytes()
    ffmpeg = fake_ffmpeg(tmp_path, raw, pts[:2], "exit 0\n")
    readings, _, _ = wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    assert [r.index for r in readings] == [7, 8]
    # A recording that never shows the clip: nothing located.
    ffmpeg = fake_ffmpeg(tmp_path, blank.tobytes() * 2, pts[:2], "exit 0\n")
    readings, _, located = wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    assert [r.status for r in readings] == ["no_strip"] * 2 and located is None


def test_read_video_kills_ffmpeg_when_the_read_fails(tmp_path, monkeypatch):
    raw = frame(7, 30).tobytes()
    ffmpeg = fake_ffmpeg(tmp_path, raw, ["0"], "exec sleep 30\n")

    def broken(picture):
        raise ValueError("a read that fails")

    monkeypatch.setattr(wf, "clip_copies", broken)
    began = time.monotonic()
    with pytest.raises(ValueError, match="a read that fails"):
        wf.read_video(tmp_path / "rec.mp4", None, ffmpeg)
    assert time.monotonic() - began < 10, "ffmpeg was waited for, not killed"


def test_main_reads_a_dxcam_capture_by_its_polls(monkeypatch, capsys):
    captures = {
        "clean": dxcam_capture(),
        # No poll for 45 ms over boundary 15: a refresh hidden, unverified.
        "late": dxcam_capture(late=(0.49, 0.535)),
        # The wall stopped after the clip's last present, the polls ran on.
        "stopped": dxcam_capture(after=1.0),
        # Every grab costs 20 ms (> a refresh): no picture is timed, so the
        # given rate cannot be checked, and an unchecked rate would read as
        # judder (review round 7): the capture fails, never the wall.
        "slow": dxcam_capture(grab=0.020),
        # Grabs of 10 ms: the poll after a picture is late but finds
        # nothing; the next picture comes on a timely poll: all judged.
        "half": dxcam_capture(grab=0.010),
        # No poll for 12 ms before boundary 15's first present is caught: its
        # start is known only to 12 ms, still within one count.
        "lag": dxcam_capture(late=(0.4995, 0.5115)),
        # A grab preempted between its stamp and its acquire (review round
        # 7): the frame was on screen within the window, never at the stamp.
        "held 5": dxcam_capture(held=(0.4995, 0.005)),
        "held 40": dxcam_capture(held=(0.4995, 0.040)),
    }
    asked = []

    def fake_dxcam(output, region, seconds):
        assert (output, seconds) == (1, 2.0)
        assert region in (None, (0, 0, 64, 32))
        return captures[asked[-1]]

    monkeypatch.setattr(wf, "capture_dxcam", fake_dxcam)
    args = ["--fps", "30", "--dxcam", "1", "--seconds", "2", "--min-boundaries", "28"]
    with pytest.raises(SystemExit) as refused:
        wf.main(args)  # dxcam's 1 ms polls cannot give the wall's rate
    assert refused.value.code == 2
    assert "--refresh-hz" in capsys.readouterr().err
    args += ["--refresh-hz", "60"]
    for bad in (["--locked"], ["--region", "0,0"], ["--region", "a,b,c,d"]):
        with pytest.raises(SystemExit) as refused:
            wf.main([*args, *bad])
        assert refused.value.code == 2, bad
        capsys.readouterr()
    # A present rate that is no whole number of the given refreshes: the
    # given rate is not the wall's.
    asked.append("clean")
    assert wf.main([*args[:-1], "45"]) == 5
    assert "not in whole 45 Hz refreshes" in capsys.readouterr().err
    assert wf.main([*args, "--region", "0,0,64,32"]) == 0
    capsys.readouterr()
    asked.append("slow")
    assert wf.main(args) == 5
    assert "60 Hz refresh rate cannot be checked" in capsys.readouterr().err
    # Every acquire window 2 ms: no picture is timed to a tenth of a refresh.
    times, sampled, polls, _, located = dxcam_capture()
    windows = [t + 0.002 for t in times]
    captures["windows"] = (times, sampled, polls, windows, located)
    asked.append("windows")
    assert wf.main(args) == 5
    assert "cannot be checked" in capsys.readouterr().err
    expected = {"clean": 0, "late": 3, "stopped": 1, "half": 0, "lag": 0}
    expected |= {"held 5": 0, "held 40": 3}
    reports = {}
    for name, code in expected.items():
        asked.append(name)
        assert wf.main(args) == code, name
        reports[name] = json.loads(capsys.readouterr().out)
        whole = 29 if name in ("late", "held 40") else 30
        assert reports[name]["boundaries"] == whole, name
    for name in ("held 5", "held 40"):
        assert reports[name]["refreshes"]["uneven"] == 0, name
        assert reports[name]["lost_boundaries"] == 0, name
    for name, judged in (("half", 28), ("lag", 28)):
        assert reports[name]["refreshes"]["uneven"] == 0, name
        assert reports[name]["refreshes"]["judged"] == judged, name
        assert reports[name]["lost_boundaries"] == 0, name
    assert reports["late"]["unverified"]["boundaries"] == 1
    assert reports["stopped"]["refreshes"]["uneven_samples"] == [58]
    assert reports["clean"]["located"] == {"copies": 9, "cell_px": 5.0, "region": None}
    # No copy of the clip found on the output: nothing captured, nothing
    # decoded (exit 4, the reason on stderr), never a failed capture.
    captures["none"] = ([], [], [], [], None)
    asked.append("none")
    assert wf.main([*args, "--region", "0,0,64,32"]) == 4
    out = capsys.readouterr()
    assert (
        json.loads(out.out)["decoded"] == 0 and json.loads(out.out)["located"] is None
    )
    assert "no copy of the clip's strip found on output 1 in 10 s" in out.err
    assert "(region (0, 0, 64, 32))" in out.err


def test_a_dxcam_picture_s_blind_span_is_since_the_poll_before_it():
    polls = [0.0, 0.001, 0.002, 0.012, 0.013]
    blind = wf.poll_blind([0.0, 0.002, 0.013], polls)
    assert blind[0] == math.inf
    assert blind[1:] == [pytest.approx(0.001), pytest.approx(0.001)]
    assert wf.poll_blind([0.012], polls) == [pytest.approx(0.010)]


def test_a_dxcam_present_rate_must_be_whole_refreshes():
    every = [n / 60 for n in range(30)]
    timed = [math.inf] + [0.0005] * 29  # each poll 0.5 ms after the last
    wa.check_present_rate(every, 60, timed)
    wa.check_present_rate(every[::2], 60, timed[:15])  # Arena at 30 Hz
    with pytest.raises(ValueError, match="presents at 60.000 Hz"):
        wa.check_present_rate(every, 45, timed)
    with pytest.raises(ValueError, match="not in whole 120 Hz"):
        wa.check_present_rate([n / 80 for n in range(30)], 120, timed)
    # The rate measured over the whole capture must match to RATE_TOLERANCE:
    # 0.2 % off reads as whole numbers interval by interval, not overall.
    with pytest.raises(ValueError, match="59.880 Hz"):
        wa.check_present_rate([n / 59.88 for n in range(300)], 60, [0.0005] * 300)
    wa.check_present_rate([n / 59.94 for n in range(300)], 60, [0.0005] * 300)
    # Only pictures timed within TIMED_SHARE of a refresh count (successive
    # ones, untimed between), the acquire window included; too few cannot
    # check the rate, and an unchecked rate is refused too (review round 7).
    for untimed in ([0.02] * 30, [0.003] * 30):  # 0.003 = 0.18 of a refresh
        with pytest.raises(ValueError, match="cannot be checked"):
            wa.check_present_rate(every, 45, untimed)
    with pytest.raises(ValueError, match="cannot be checked"):
        wa.check_present_rate(every, 60, timed, [0.002] * 30)
    wa.check_present_rate(every, 60, timed, [0.001] * 30)
    wa.check_present_rate(every, 60, [0.02, 0.0005] * 15)
    with pytest.raises(ValueError, match="not in whole"):
        wa.check_present_rate(every, 45, [0.02, 0.0005] * 15)
    with pytest.raises(ValueError, match="cannot be checked"):
        wa.check_present_rate(every[: wa.MIN_TIMED], 45, timed[: wa.MIN_TIMED])
    wa.check_present_rate(every[: wa.MIN_TIMED + 2], 60, timed[: wa.MIN_TIMED + 2])


def test_a_locked_recording_must_be_the_wall_s_own_refreshes():
    # Review round 7: --locked was taken on trust. Each interval must be a
    # whole number of the wall's refreshes, and the rate the wall's.
    wa.check_locked([n / 60 for n in range(600)], 60)
    wa.check_locked([round(n / 60, 3) for n in range(600)], 60)  # 1 ms pts
    wa.check_locked([n / 60 for n in range(600) if n % 7], 60)  # dropped ones
    with pytest.raises(ValueError, match="no whole 60 Hz refreshes"):
        wa.check_locked([n / 120 for n in range(600)], 60)  # a 120 fps camera
    with pytest.raises(ValueError, match="one is 0.67"):
        wa.check_locked([n / 90 for n in range(600)], 60)  # a third off
    with pytest.raises(ValueError, match="runs at 59.880 Hz"):
        wa.check_locked([n / 59.88 for n in range(600)], 60)
    wa.check_locked([0.0], 60)


def test_main_refuses_a_locked_video_that_is_no_recording_of_the_wall(
    monkeypatch, capsys
):
    readings = [wa.Reading("ok", 0, None, 0, 8)] * 240
    times = [n / 120 for n in range(240)]
    monkeypatch.setattr(wf, "read_video", lambda *_: (readings, times, None))
    args = ["--fps", "30", "--video", "rec.mp4", "--refresh-hz", "60", "--locked"]
    assert wf.main(args) == 5
    assert "not a recording of the wall's own signal" in capsys.readouterr().err
