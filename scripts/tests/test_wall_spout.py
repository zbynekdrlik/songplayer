"""Tests for scripts/wall_spout.py and ``wall_frames``' Spout capture (#272):
the wall is off the desktop, so the gate reads Arena's Spout output. A fake
sender stands in for SpoutGL (Windows only); the capture runs through the
real ``find_clip``, ``capture_region`` and ``cell_sums``.
"""

from __future__ import annotations

import json

import numpy as np
import pytest

import wall_frames as wf
import wall_locate as wl
import wall_spout as ws
from wall_fakes import dxcam_capture, frame


class FakeSender:
    """A sender showing ``pictures`` in turn, one per read; the last stays."""

    def __init__(self, pictures: list[np.ndarray]) -> None:
        self.pictures = pictures
        self.size = (pictures[0].shape[1], pictures[0].shape[0])
        self.boxes: list[tuple[int, int, int, int]] = []
        self.received = 0

    def receive(self) -> None:
        self.received += 1

    def read(self, region: tuple[int, int, int, int]) -> bytes:
        assert self.received == len(self.boxes) + 1, "a read without a receive"
        self.boxes.append(region)
        left, top, right, bottom = region
        picture = self.pictures[min(len(self.boxes) - 1, len(self.pictures) - 1)]
        return np.stack([picture[top:bottom, left:right]] * 4, axis=2).tobytes()


def test_the_acquire_window_ends_at_the_receive_not_at_a_slow_read(monkeypatch):
    # CI 41083008317: each new picture's read-back waits ~4 ms on the GPU, and
    # stamping after it left 0 intervals timed.
    clock = [0.0]
    monkeypatch.setattr(wf.time, "perf_counter", lambda: clock[0])

    class SlowRead(FakeSender):
        def receive(self) -> None:
            clock[0] += 0.0003
            super().receive()

        def read(self, region):
            clock[0] += 0.004
            return super().read(region)

    camera = ws.NewPictures(SlowRead(plain(1)))
    acquired = wf.time_acquire(camera)
    camera.grab()
    assert acquired[0] == pytest.approx(0.0003)
    assert clock[0] == pytest.approx(0.0043)


def plain(*levels: int) -> list[np.ndarray]:
    return [np.full((4, 6), v, dtype=np.uint8) for v in levels]


def test_a_read_of_the_same_bytes_is_a_poll_with_nothing_new():
    camera = ws.NewPictures(FakeSender(plain(1, 1, 2, 2, 1)))
    first = camera.grab()
    assert first.shape == (4, 6, 4)
    assert camera.grab() is None
    assert camera.grab() is not None
    assert camera.grab() is None
    # Back to an earlier picture is new: only the last read counts.
    assert camera.grab() is not None


def test_a_region_is_read_as_rows_top_first_and_a_new_region_is_never_held():
    picture = np.arange(24, dtype=np.uint8).reshape(4, 6)
    sender = FakeSender([picture])
    camera = ws.NewPictures(sender)
    whole = camera.grab()
    part = camera.grab(region=(1, 2, 4, 4))
    assert sender.boxes == [(0, 0, 6, 4), (1, 2, 4, 4)]
    assert np.array_equal(part[..., 0], picture[2:4, 1:4])
    assert np.array_equal(whole[..., 0], picture)


@pytest.mark.parametrize(
    "region", [(-1, 0, 3, 3), (0, -1, 3, 3), (0, 0, 7, 3), (0, 0, 3, 5), (3, 0, 3, 3)]
)
def test_a_region_outside_the_sender_is_refused(region):
    with pytest.raises(ValueError, match="not inside 6x4"):
        ws.NewPictures(FakeSender(plain(1))).grab(region=region)


def test_a_copy_cut_by_a_seam_is_left_out_by_its_narrower_cell():
    # SNV 30.10.2026: 60, 60 and 58.33 px; the last read a blend at each change
    # of a bit its samples sat next to.
    whole = [wl.Copy(float(x), 60.0, ()) for x in (60, 1260)]
    cut = wl.Copy(3631.0, 58.33, ())
    assert wf.same_scale([*whole, cut]) == whole
    assert wf.same_scale([cut, *whole]) == whole
    # A tie: the larger cells, whose reading no seam shortened.
    assert wf.same_scale([whole[0], cut, wl.Copy(4800.0, 58.33, ()), whole[1]]) == [
        whole[0],
        whole[1],
    ]
    assert wf.same_scale([]) == []
    # Within the share: one scale.
    near = [wl.Copy(0.0, 60.0, ()), wl.Copy(900.0, 59.5, ())]
    assert wf.same_scale(near) == near


def test_a_capture_reads_only_the_middle_rows_of_the_strips():
    picture = frame(100, 30)
    copies = wf.clip_copies(picture)
    full, _ = wl.capture_region(copies, picture.shape)
    box, inside = wl.capture_region(copies, picture.shape, 6)
    assert (box[0], box[2]) == (full[0], full[2])
    assert box[3] - box[1] == 6
    assert full[1] < box[1] and box[3] < full[3]
    left, top, right, bottom = box
    seen = picture[top:bottom, left:right]
    assert wf.read_sums(wl.cell_sums(seen, inside), inside).index == 100


def test_rows_the_strips_do_not_share_leave_the_region_as_it_was():
    picture = frame(100, 30)
    copies = wf.clip_copies(picture)
    assert (
        wl.capture_region(copies, picture.shape, 2)[0][3]
        - (wl.capture_region(copies, picture.shape, 2)[0][1])
        == 2
    )
    moved = []
    for k, copy in enumerate(copies[:2]):
        s0, s1 = copy.strips[0]
        moved.append(wl.replace(copy, strips=((s0 + k * (s1 - s0 - 1), s1 + k),)))
    assert (
        wl.capture_region(moved, picture.shape, 2)[0]
        == (wl.capture_region(moved, picture.shape)[0])
    )


def test_a_spout_capture_finds_the_clip_then_polls_only_its_rows(monkeypatch):
    closed.clear()
    pictures = [frame(n, 30) for n in range(100, 130)]
    sender = FakeSender(pictures)
    monkeypatch.setattr(wf, "SPOUT_ROWS", 6)
    monkeypatch.setattr(
        wf, "GlSender", lambda name: Closing(sender) if name == "Arena" else None
    )
    times, sums, _polls, ended, located = wf.capture_spout("Arena", None, 0.2)
    assert located is not None and located.report()["copies"] == wl.MAX_COPIES
    box = located.region
    assert box[3] - box[1] == 6
    assert sender.boxes[0] == (0, 0, sender.size[0], sender.size[1])
    assert set(sender.boxes[1:]) == {box}
    assert len(times) == len(sums) == len(ended) >= 2
    copies = list(located.copies)
    assert [wf.read_sums(s, copies).ok for s in sums] == [True] * len(sums)
    assert all(a <= b for a, b in zip(times, ended))
    assert closed == [sender]


closed: list[FakeSender] = []


class Closing:
    def __init__(self, sender: FakeSender) -> None:
        self.sender = sender
        self.size = sender.size

    def receive(self):
        self.sender.receive()

    def read(self, region):
        return self.sender.read(region)

    def close(self):
        closed.append(self.sender)


def test_a_failed_capture_still_releases_the_receiver(monkeypatch):
    closed.clear()
    sender = FakeSender([frame(100, 30)])
    monkeypatch.setattr(wf, "GlSender", lambda name: Closing(sender))

    def broken(*args, **kwargs):
        raise RuntimeError("sender changed")

    monkeypatch.setattr(wf, "capture_camera", broken)
    with pytest.raises(RuntimeError, match="sender changed"):
        wf.capture_spout("Arena", None, 0.1)
    assert closed == [sender]


def test_a_spout_poll_that_finds_nothing_does_not_sleep_a_millisecond(monkeypatch):
    slept: list[float] = []
    monkeypatch.setattr(wf.time, "sleep", slept.append)
    sender = FakeSender([frame(100, 30)])
    monkeypatch.setattr(wf, "LOCATE_S", 1.0)
    monkeypatch.setattr(wf, "GlSender", lambda name: Closing(sender))
    wf.capture_spout("Arena", None, 0.01)
    assert slept and set(slept) == {0.0}


def spout_run(monkeypatch, capsys, result, *extra) -> tuple[int, str, str]:
    seen = []

    def fake(sender, region, seconds):
        seen.append((sender, region, seconds))
        return result

    monkeypatch.setattr(wf, "capture_spout", fake)
    code = wf.main(
        ["--fps", "30", "--refresh-hz", "60", "--seconds", "9"]
        + ["--spout", "Arena - Screen 2", *extra]
    )
    assert seen == [("Arena - Screen 2", None, 9.0)]
    out, err = capsys.readouterr()
    return code, out, err


def test_main_judges_a_spout_capture_as_a_dxcam_one(monkeypatch, capsys):
    code, out, _ = spout_run(
        monkeypatch, capsys, dxcam_capture(), "--min-boundaries", "5"
    )
    assert code == 0
    assert json.loads(out)["verdict"] == "clean"


def test_main_names_the_sender_when_no_copy_shows(monkeypatch, capsys):
    code, out, err = spout_run(monkeypatch, capsys, ([], [], [], [], None))
    assert code == wf.EXIT_NOTHING_DECODED
    assert "found on sender 'Arena - Screen 2' in" in err
    assert json.loads(out)["located"] is None


def test_a_spout_capture_failure_is_a_capture_failure(monkeypatch, capsys):
    def fake(sender, region, seconds):
        raise RuntimeError("no Spout sender 'Arena - Screen 2'; listed: []")

    monkeypatch.setattr(wf, "capture_spout", fake)
    code = wf.main(["--fps", "30", "--refresh-hz", "60", "--spout", "Arena - Screen 2"])
    assert code == wf.EXIT_FAILED
    assert "no Spout sender" in capsys.readouterr().err


def test_locked_is_refused_with_spout(capsys):
    with pytest.raises(SystemExit) as stop:
        wf.main(["--fps", "30", "--refresh-hz", "60", "--locked", "--spout", "A"])
    assert stop.value.code == 2
    assert "--locked" in capsys.readouterr().err
