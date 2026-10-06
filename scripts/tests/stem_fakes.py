"""Test doubles for `scripts/stem_worker.py cmd_separate` (#207).

The heavy model stack (torch / librosa / audio_separator) is not installed in
the `eval-checks` CI env (numpy + soundfile only, lyrics-worker-tests.md), so
`install_fakes` puts stand-ins into `sys.modules` for the duration of one test.
Shared by `test_stem_streaming.py` (memory is O(segment)) and
`test_stem_publish.py` (the stems are published with the POSIX rename).
"""

import os
import sys
import types
from types import SimpleNamespace

import soundfile as sf


def fake_torch():
    torch = types.ModuleType("torch")
    torch.bfloat16 = object()

    class _OOM(Exception):
        pass

    torch.cuda = SimpleNamespace(
        is_available=lambda: False,
        device_count=lambda: 0,
        empty_cache=lambda: None,
        set_per_process_memory_fraction=lambda *_a, **_k: None,
        OutOfMemoryError=_OOM,
    )
    return torch


def install_fakes(monkeypatch, mix_path):
    """Fake torch / audio_separator / librosa. The fake separator splits its
    input into vocals = 0.3 * x and other = 0.7 * x (so v + i == x). The fake
    librosa.load REFUSES the mix path: the heavy child must never load it."""
    loads = []
    windows = []  # (frames, channels) of every window the separator received

    def fake_load(path, sr=None, mono=True):
        if os.path.abspath(path) == os.path.abspath(mix_path):
            raise AssertionError("#207: the whole mix was loaded into memory")
        y, native = sf.read(path, dtype="float32", always_2d=False)
        assert sr in (None, native), "fake librosa does not resample"
        loads.append(path)
        return (y.T if y.ndim == 2 else y), native

    librosa = types.ModuleType("librosa")
    librosa.load = fake_load

    class FakeSeparator:
        def __init__(
            self, model_file_dir=None, output_format=None, output_dir=None, **_k
        ):
            self.output_dir = output_dir

        def load_model(self, name):
            self.model = name

        def separate(self, path):
            x, sr = sf.read(path, dtype="float32", always_2d=False)
            windows.append((x.shape[0], 1 if x.ndim == 1 else x.shape[1]))
            base = os.path.splitext(os.path.basename(path))[0]
            names = []
            for token, gain in (("Vocals", 0.3), ("Other", 0.7)):
                name = f"{base}_({token})_kim.wav"
                sf.write(
                    os.path.join(self.output_dir, name), x * gain, sr, subtype="FLOAT"
                )
                names.append(name)
            return names

    sep_pkg = types.ModuleType("audio_separator")
    sep_mod = types.ModuleType("audio_separator.separator")
    sep_mod.Separator = FakeSeparator
    sep_pkg.separator = sep_mod

    monkeypatch.setitem(sys.modules, "torch", fake_torch())
    monkeypatch.setitem(sys.modules, "librosa", librosa)
    monkeypatch.setitem(sys.modules, "audio_separator", sep_pkg)
    monkeypatch.setitem(sys.modules, "audio_separator.separator", sep_mod)
    return loads, windows
