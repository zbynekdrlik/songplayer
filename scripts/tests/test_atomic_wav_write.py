"""#171 — regression test for the atomic WAV write in lyrics_worker.py.

The resumable isolation writes each segment (and the final stitch) atomically via
a `<name>.wav.tmp` scratch + os.replace. soundfile infers the format from the
extension, and `.tmp` is unknown — so the write MUST pass format="WAV" or it
raises `TypeError: ... unable to get format from file extension` and every
isolation run dies before its first segment lands. This is the win-resolume
0.53.0-dev.2 exit-1 (`_isolate_one_segment` -> `sf.write(...seg_0000_of_0013.wav.tmp...)`).
Needs soundfile (bundled libsndfile wheel; installed in the eval-checks job).
"""

import importlib.util
import os

import numpy as np
import soundfile as sf

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_lyrics_worker():
    path = os.path.join(_SCRIPTS_DIR, "lyrics_worker.py")
    spec = importlib.util.spec_from_file_location("lyrics_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


lw = _load_lyrics_worker()


def test_atomic_write_wav_lands_a_float_wav_from_a_tmp_scratch(tmp_path):
    # The scratch path is `<out>.tmp` (unknown extension). RED reproduces the box
    # exit-1: _atomic_write_wav without format="WAV" raises TypeError here. GREEN
    # passes format="WAV" and the segment lands.
    out = str(tmp_path / "seg_0000_of_0013.wav")
    audio = np.linspace(-0.5, 0.5, 1600, dtype=np.float32)
    lw._atomic_write_wav(out, audio, 16000)
    assert os.path.exists(out), "the final segment WAV must be written"
    assert not os.path.exists(out + ".tmp"), "the .tmp scratch must be renamed away"
    data, sr = sf.read(out, dtype="float32")
    assert sr == 16000
    assert data.shape[0] == 1600
    assert np.max(np.abs(data - audio)) < 1e-4


def test_atomic_write_wav_is_atomic_replace(tmp_path):
    # Writing over an existing output replaces it wholesale (os.replace is atomic).
    out = str(tmp_path / "seg.wav")
    lw._atomic_write_wav(out, np.zeros(800, dtype=np.float32), 16000)
    lw._atomic_write_wav(out, np.ones(1600, dtype=np.float32), 16000)
    data, sr = sf.read(out, dtype="float32")
    assert data.shape[0] == 1600
    assert np.allclose(data, 1.0, atol=1e-4)
