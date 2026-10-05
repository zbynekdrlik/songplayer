"""#207 — the stem sidecars are published with the POSIX-semantics rename.

On Windows `os.replace` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`. It fails with
`[WinError 5] Access is denied` while ANY other handle has the target open, even
one opened with `FILE_SHARE_DELETE` (#184 round F2, box comment 5795075881: the
dub promotion failed exactly this way). SongPlayer holds BOTH stem sidecars open
whenever the song is loaded: `stems/reader.rs::open_audio_stream` opens
`{base}_audio_vocals.flac` + `_instrumental.flac` with Rust std, which shares
READ|WRITE|DELETE. So a re-separation of a loaded song cannot publish its stems
with `os.replace`. `scripts/win_replace.py::replace_file` (`FileRenameInfoEx` +
POSIX semantics) can: the reader keeps the old data, the next open gets the new
stem.

The `eval-checks` job is Linux, where both calls are a plain rename. The
`windows_rename` fixture therefore emulates the Windows semantics: `os.replace`
onto a HELD path raises the WinError 5 `PermissionError`, while
`win_replace.replace_file` is the POSIX rename that succeeds over it (and is
recorded). numpy + soundfile only; the model stack of `cmd_separate` is faked
(`stem_fakes.py`).
"""

import importlib.util
import os
from types import SimpleNamespace

import numpy as np
import pytest
import soundfile as sf
import win_replace  # the module object `stem_worker`'s `import win_replace` binds
from stem_fakes import install_fakes

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_stem_worker():
    path = os.path.join(_SCRIPTS_DIR, "stem_worker.py")
    spec = importlib.util.spec_from_file_location("stem_worker_publish", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sw = _load_stem_worker()


@pytest.fixture
def windows_rename(monkeypatch):
    """Windows rename semantics over the paths in `.held` (a reader has them
    open): `os.replace` onto one raises WinError 5, the POSIX rename
    (`win_replace.replace_file`) succeeds and is recorded in `.posix`."""
    held = set()
    posix = []
    real_replace = os.replace

    def move_file_ex(src, dst):
        if os.path.abspath(dst) in held:
            raise PermissionError(13, "Access is denied", src, None, dst)
        real_replace(src, dst)

    def posix_rename(src, dst):
        posix.append((src, dst))
        real_replace(src, dst)

    monkeypatch.setattr(os, "replace", move_file_ex)
    monkeypatch.setattr(win_replace, "replace_file", posix_rename)
    return SimpleNamespace(held=held, posix=posix)


def _old_stem(path, held):
    """An existing published stem that SongPlayer's reader holds open."""
    with open(path, "wb") as f:
        f.write(b"OLD")
    held.add(os.path.abspath(path))


def _segments():
    rng = np.random.default_rng(207)
    return [rng.uniform(-0.9, 0.9, (n, 2)).astype(np.float32) for n in (700, 300)]


def test_streaming_writer_publishes_over_a_held_stem(tmp_path, windows_rename):
    out = str(tmp_path / "song_audio_vocals.flac")
    _old_stem(out, windows_rename.held)

    with sw._StreamingStitchWriter(out, 2, 500, 200, channels=2) as w:
        for seg in _segments():
            w.add_segment(seg)

    assert windows_rename.posix == [(out + ".tmp", out)]
    info = sf.info(out)
    assert (info.frames, info.channels, info.subtype) == (800, 2, "PCM_24")
    assert sorted(os.listdir(tmp_path)) == ["song_audio_vocals.flac"]


def test_reference_writer_publishes_over_a_held_stem(tmp_path, windows_rename):
    out = str(tmp_path / "song_audio_instrumental.flac")
    _old_stem(out, windows_rename.held)

    sw._write_array_48k_stereo(np.zeros((480, 2), dtype=np.float32), out)

    assert windows_rename.posix == [(out + ".tmp", out)]
    assert sf.info(out).frames == 480
    assert sorted(os.listdir(tmp_path)) == ["song_audio_instrumental.flac"]


@pytest.mark.parametrize("writer", ["streaming", "reference"])
def test_a_refused_posix_rename_keeps_the_previous_stem(tmp_path, monkeypatch, writer):
    # The POSIX rename itself fails (a reader opened WITHOUT FILE_SHARE_DELETE,
    # or a volume without FileRenameInfoEx): the publish fails loudly, the
    # previous stem stays byte-identical and the .tmp is removed.
    out = str(tmp_path / "song_audio_vocals.flac")
    with open(out, "wb") as f:
        f.write(b"OLD")

    def denied(src, dst):
        raise PermissionError(13, "Access is denied", src, None, dst)

    monkeypatch.setattr(win_replace, "replace_file", denied)
    with pytest.raises(PermissionError):
        if writer == "streaming":
            with sw._StreamingStitchWriter(out, 2, 500, 200, channels=2) as w:
                for seg in _segments():
                    w.add_segment(seg)
        else:
            sw._write_array_48k_stereo(np.zeros((480, 2), dtype=np.float32), out)

    with open(out, "rb") as f:
        assert f.read() == b"OLD"
    assert sorted(os.listdir(tmp_path)) == ["song_audio_vocals.flac"]


def test_cmd_separate_publishes_both_stems_over_held_ones(
    tmp_path, monkeypatch, windows_rename
):
    # A re-separation of a song the wall has loaded: both stems exist and are
    # held. The run publishes both with the POSIX rename (its per-segment
    # scratch WAVs in the work dir are nobody's open file and may use
    # os.replace).
    monkeypatch.setattr(sw, "STEM_SEGMENT_SECONDS", 3.0)
    monkeypatch.setattr(sw, "STEM_OVERLAP_SECONDS", 0.5)
    sr = sw.OUTPUT_SAMPLE_RATE
    n = int(round(7.3 * sr))
    mix = np.random.default_rng(1207).uniform(-0.8, 0.8, (n, 2)).astype(np.float32)
    mix_path = str(tmp_path / "song_audio.flac")
    sf.write(mix_path, mix, sr, format="FLAC", subtype="PCM_24")
    install_fakes(monkeypatch, mix_path)
    vocals_out = str(tmp_path / "song_audio_vocals.flac")
    instr_out = str(tmp_path / "song_audio_instrumental.flac")
    _old_stem(vocals_out, windows_rename.held)
    _old_stem(instr_out, windows_rename.held)

    sw.cmd_separate(
        SimpleNamespace(
            audio=mix_path,
            vocals_out=vocals_out,
            instrumental_out=instr_out,
            models_dir=str(tmp_path / "models"),
            work_dir=str(tmp_path / "work"),
            force_cpu=False,
        )
    )

    assert windows_rename.posix == [
        (vocals_out + ".tmp", vocals_out),
        (instr_out + ".tmp", instr_out),
    ]
    for p in (vocals_out, instr_out):
        assert sf.info(p).frames == n
    assert sorted(os.listdir(tmp_path)) == [
        "song_audio.flac",
        "song_audio_instrumental.flac",
        "song_audio_vocals.flac",
    ]
