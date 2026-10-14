"""Tests for scripts/wall_frames.py's capture glue and CLI (#272): the
video read through ``av_sync_check``'s command line and readers, a stand-in
ffmpeg, dxcam's polls (``poll_blind``, ``check_present_rate``), and
``main``'s exit codes for a video and a simulated dxcam capture.
"""

from __future__ import annotations

import json
import math
import time
from pathlib import Path

import pytest

import av_sync_check
import wall_analysis as wa
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
    clean = [wf.read_frame(frame(n, 30, 480, 270)) for n in range(30)]
    times = [n / 30 for n in range(30)]
    videos = {
        "clean.mp4": (clean, times),
        "lost.mp4": (clean[:2] + clean[3:], times[:29]),
        "dropped.mp4": (clean[:2] + clean[3:], times[:2] + times[3:]),
        "blank.mp4": ([wa.Reading("no_strip")] * 3, times[:3]),
        "pal.mp4": (clean[:25], [n / 25 for n in range(25)]),
    }

    def fake_video(path, width, x_range):
        assert width == 480 and x_range == (0.0, 1.0)
        if path.name == "broken.mp4":
            raise RuntimeError("ffmpeg exited 1 reading broken.mp4: Invalid data")
        return videos[path.name]

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
    for extra in ([], ["--locked"]):
        assert wf.main([*extra, *args, "clean.mp4"]) == 0, extra
        report = json.loads(capsys.readouterr().out)
        assert report["boundaries"] == 30 and report["refreshes"]["judged"] == 28
    # The same 30 fps recording of a 60 Hz wall saw every other refresh: a
    # refresh hidden before each picture, so nothing can be judged.
    slow = ["--refresh-hz", "60", "--min-boundaries", "28", "--video"]
    assert wf.main([*base[:-2], *slow, "clean.mp4"]) == 3
    report = json.loads(capsys.readouterr().out)
    assert report["capture"]["blind_pictures"] == 29
    assert report["refreshes"]["judged"] == 0
    # A frame missing while the pts run on: the wall lost it.
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


def test_main_reads_a_dxcam_capture_by_its_polls(monkeypatch, capsys):
    captures = {
        "clean": dxcam_capture(),
        # No poll for 45 ms over boundary 15: a refresh hidden, unverified.
        "late": dxcam_capture(late=(0.49, 0.535)),
        # The wall stopped after the clip's last present, the polls ran on.
        "stopped": dxcam_capture(after=1.0),
        # Every grab costs 20 ms (> a refresh): a refresh hidden before
        # every picture, so a clean wall is never judged, never read as
        # judder (review round 4: 17-22 ms grabs read it as defects).
        "slow": dxcam_capture(grab=0.020),
        # Grabs of 10 ms: the poll after a picture is late but finds
        # nothing; the next picture comes on a timely poll: all judged.
        "half": dxcam_capture(grab=0.010),
        # No poll for 12 ms before boundary 15's first present is caught: its
        # start is known only to 12 ms, still within one count.
        "lag": dxcam_capture(late=(0.4995, 0.5115)),
    }
    asked = []

    def fake_dxcam(output, region, seconds, x_range):
        assert (output, seconds, x_range) == (1, 2.0, (0.0, 1.0))
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
    assert "no whole number of 45 Hz refreshes" in capsys.readouterr().err
    assert wf.main([*args, "--region", "0,0,64,32"]) == 0
    capsys.readouterr()
    expected = {"clean": 0, "late": 3, "slow": 3, "stopped": 1, "half": 0, "lag": 0}
    reports = {}
    for name, code in expected.items():
        asked.append(name)
        assert wf.main(args) == code, name
        reports[name] = json.loads(capsys.readouterr().out)
        assert reports[name]["boundaries"] == (29 if name == "late" else 30), name
    for name, judged in (("slow", 0), ("half", 28), ("lag", 28)):
        assert reports[name]["refreshes"]["uneven"] == 0, name
        assert reports[name]["refreshes"]["judged"] == judged, name
        assert reports[name]["lost_boundaries"] == 0, name
    assert reports["late"]["unverified"]["boundaries"] == 1
    assert reports["stopped"]["refreshes"]["uneven_samples"] == [58]


def test_a_dxcam_picture_s_blind_span_is_since_the_poll_before_it():
    polls = [0.0, 0.001, 0.002, 0.012, 0.013]
    blind = wf.poll_blind([0.0, 0.002, 0.013], polls)
    assert blind[0] == math.inf
    assert blind[1:] == [pytest.approx(0.001), pytest.approx(0.001)]
    assert wf.poll_blind([0.012], polls) == [pytest.approx(0.010)]


def test_a_dxcam_present_rate_must_be_whole_refreshes():
    every = [n / 60 for n in range(30)]
    timed = [math.inf] + [0.001] * 29
    wa.check_present_rate(every, 60, timed)
    wa.check_present_rate(every[::2], 60, timed[:15])  # Arena at 30 Hz
    with pytest.raises(ValueError, match="every 16.667 ms"):
        wa.check_present_rate(every, 45, timed)
    with pytest.raises(ValueError, match="no whole number of 120 Hz"):
        wa.check_present_rate([n / 80 for n in range(30)], 120, timed)
    # Only intervals between pictures timed within half a refresh count,
    # and too few are no check.
    wa.check_present_rate(every, 45, [0.02] * 30)
    wa.check_present_rate(every, 45, [0.02, 0.001] * 15)  # no pair timed
    wa.check_present_rate(every[: wa.MIN_TIMED], 45, timed[: wa.MIN_TIMED])
