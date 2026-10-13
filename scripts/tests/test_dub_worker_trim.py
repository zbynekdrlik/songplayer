"""Tests for `dub_worker.py trim-tail` (#257 review).

A dub made before #257 ends with the Live session's drain silence (SNV: one
dub, video 344, 7.8 s past its video). `trim-tail` cuts such a dub at the
ORIGINAL audio's length or its last sample above -60 dBFS + 50 ms, whichever
is later, keeps the untouched dub next to it as `<dub>.prev`, and promotes the
cut file with the same POSIX rename the dub worker uses. numpy + soundfile
only (the eval-checks CI job has both).
"""

import importlib.util
import json
import os
import sys

import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)


def _load_dub_worker():
    path = os.path.join(_SCRIPTS_DIR, "dub_worker.py")
    spec = importlib.util.spec_from_file_location("dub_worker_trim", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dw = _load_dub_worker()

RATE = 48_000


def _flac(path, frames, loud_until, tail=0.0):
    """A 48 kHz stereo PCM_24 FLAC of `frames` frames: +-0.25 (about -12 dBFS)
    on every frame before `loud_until`, then `tail` on both channels."""
    import numpy as np
    import soundfile as sf

    data = np.full((frames, 2), tail, dtype=np.float64)
    data[:loud_until, 0] = 0.25
    data[:loud_until, 1] = -0.25
    sf.write(str(path), data, RATE, subtype="PCM_24", format="FLAC")


def _frames(path):
    import soundfile as sf

    data, _ = sf.read(str(path), dtype="int32", always_2d=True)
    return data


def _run(capsys, dub, audio):
    dw.main(["trim-tail", "--dub", str(dub), "--audio", str(audio)])
    return json.loads(capsys.readouterr().out.strip())


# ── the pure decisions ─────────────────────────────────────────────────────────


def test_the_threshold_is_minus_60_dbfs_in_int32():
    # 10^(-60/20) of full scale, int32 samples (soundfile's int32 read).
    assert dw.audible_threshold_int32() == 2_147_483


def test_audible_end_is_the_frame_after_the_last_one_above_the_threshold():
    import numpy as np

    t = 1_000
    block = np.array(
        [[0, 0], [t + 1, 0], [0, -(t + 1)], [t, -t], [0, 0]], dtype=np.int32
    )
    # Frame 2 is the last with a sample past +-t in ANY channel (frame 3 sits
    # exactly on it: not above); the block starts at frame 100 of the file.
    assert dw.audible_end(block, 100, t) == 103
    assert dw.audible_end(np.zeros((4, 2), dtype=np.int32), 100, t) is None


def test_the_cut_is_the_original_length_or_the_last_audible_plus_50_ms():
    # The last audible frame's end + 2400 (50 ms at 48 kHz), never under the
    # original's length, never past the dub's own end.
    assert dw.trim_frames(72_000, 48_000, 52_800, RATE) == 55_200
    assert dw.trim_frames(72_000, 60_000, 52_800, RATE) == 60_000
    assert dw.trim_frames(72_000, 48_000, None, RATE) == 48_000
    assert dw.trim_frames(72_000, 48_000, 71_000, RATE) == 72_000
    assert dw.trim_frames(72_000, 80_000, 10_000, RATE) == 72_000


# ── the subcommand on real FLAC files ──────────────────────────────────────────


def test_trim_tail_cuts_the_drain_silence_and_keeps_the_original_as_prev(
    tmp_path, capsys, monkeypatch
):
    # The original audio is 1.0 s. The dub speaks until 1.1 s, then 0.4 s of
    # near silence at -70 dBFS (under the -60 threshold): it is cut at
    # 1.1 s + 50 ms, the rest (350 ms) goes, the original dub stays as .prev
    # and the cut file is promoted with the POSIX rename.
    audio = tmp_path / "x_normalized_audio.flac"
    dub = tmp_path / "x_normalized_dub.flac"
    _flac(audio, RATE, RATE)
    _flac(dub, 72_000, 52_800, tail=10 ** (-70 / 20))
    before = _frames(dub)
    renames = []

    def replace(src, dst):
        renames.append((os.path.basename(src), os.path.basename(dst)))
        os.replace(src, dst)

    monkeypatch.setattr(dw.wr, "replace_file", replace)
    out = _run(capsys, dub, audio)
    assert out == {"before_samples": 72_000, "after_samples": 55_200, "cut_ms": 350}
    assert renames == [("x_normalized_dub.part.flac", "x_normalized_dub.flac")]
    after = _frames(dub)
    assert after.shape == (55_200, 2)
    assert (after == before[:55_200]).all(), "every kept sample bit for bit"
    prev = tmp_path / "x_normalized_dub.flac.prev"
    assert (_frames(prev) == before).all(), "the original dub, untouched"
    assert not (tmp_path / "x_normalized_dub.part.flac").exists()


def test_trim_tail_keeps_the_original_length_when_the_speech_ends_inside_it(
    tmp_path, capsys
):
    # Speech ends at 0.5 s of a 1.0 s original: the dub keeps the original's
    # whole length (the player plays the audio to its end), and only the
    # drain past it goes.
    audio = tmp_path / "a.flac"
    dub = tmp_path / "a_dub.flac"
    _flac(audio, RATE, RATE)
    _flac(dub, 60_000, 24_000)
    out = _run(capsys, dub, audio)
    assert out == {"before_samples": 60_000, "after_samples": 48_000, "cut_ms": 250}
    assert _frames(dub).shape == (48_000, 2)


def test_trim_tail_with_nothing_to_cut_writes_nothing(tmp_path, capsys):
    # A dub already no longer than the original (a dub made after #257):
    # nothing is written, no .prev.
    audio = tmp_path / "a.flac"
    dub = tmp_path / "a_dub.flac"
    _flac(audio, RATE, RATE)
    _flac(dub, RATE, 30_000)
    data = dub.read_bytes()
    out = _run(capsys, dub, audio)
    assert out == {"before_samples": RATE, "after_samples": RATE, "cut_ms": 0}
    assert dub.read_bytes() == data
    assert not (tmp_path / "a_dub.flac.prev").exists()


def test_trim_tail_never_overwrites_an_earlier_prev(tmp_path, capsys):
    # A .prev from an earlier trim may be the only copy of the original dub:
    # the run fails loudly and touches nothing.
    audio = tmp_path / "a.flac"
    dub = tmp_path / "a_dub.flac"
    prev = tmp_path / "a_dub.flac.prev"
    _flac(audio, RATE, RATE)
    _flac(dub, 72_000, 52_800)
    prev.write_bytes(b"the original")
    data = dub.read_bytes()
    with pytest.raises(SystemExit) as exit_info:
        dw.main(["trim-tail", "--dub", str(dub), "--audio", str(audio)])
    assert exit_info.value.code == 1
    err = json.loads(capsys.readouterr().err.strip())
    assert "a_dub.flac.prev" in err["error"]
    assert prev.read_bytes() == b"the original"
    assert dub.read_bytes() == data
    assert not (tmp_path / "a_dub.part.flac").exists()
