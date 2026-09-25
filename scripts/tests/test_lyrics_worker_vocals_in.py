"""#144 — preprocess-vocals consumes the stems worker's vocals sidecar.

The mtl aligner's vocals now come from the stems worker's vocals sidecar
(`{base}_audio_vocals.flac`, #184 G0). `preprocess-vocals --vocals-in <flac>`
runs anvuew dereverb + 16 kHz mono float32 resample ONLY — the second
BS-RoFormer vocal-isolation pass is deleted (`sep_mel` gone), and `preload`
warms only the anvuew dereverb model + the Qwen3 aligner.

Runs in the `eval-checks` CI job (numpy + soundfile only). `torch`,
`audio_separator`, `librosa` and `qwen_asr` are injected as fakes — a fake
dereverb separator avoids the heavy models entirely, so no GPU / torch / real
audio-separator is needed.

RED (against the pre-#144 script): `cmd_preprocess_vocals` still loads the
BS-RoFormer `sep_mel` model, so `loaded` carries a `bs_roformer` entry and both
assertions fail. GREEN: only the dereverb model loads.
"""

import importlib.util
import os
import sys
import types

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


def _install_fake_torch(monkeypatch):
    torch = types.ModuleType("torch")
    torch.bfloat16 = object()
    torch.cuda = types.SimpleNamespace(
        is_available=lambda: False,
        device_count=lambda: 0,
        empty_cache=lambda: None,
        set_per_process_memory_fraction=lambda _f: None,
        OutOfMemoryError=type("OutOfMemoryError", (Exception,), {}),
    )
    monkeypatch.setitem(sys.modules, "torch", torch)


def _install_fake_separator(monkeypatch, loaded):
    """A fake audio-separator whose `separate()` emits a `(noreverb)` stem —
    a straight copy of its input — so the anvuew dereverb path runs on numpy +
    soundfile alone. Records every `load_model` call so the test can assert
    WHICH models were warmed."""

    class FakeSeparator:
        def __init__(
            self,
            model_file_dir=None,
            output_format=None,
            output_dir=None,
            use_soundfile=None,
        ):
            self.output_dir = output_dir

        def load_model(self, name):
            loaded.append(name)

        def separate(self, in_path):
            data, sr = sf.read(in_path, dtype="float32", always_2d=True)
            base = os.path.splitext(os.path.basename(in_path))[0]
            out = os.path.join(self.output_dir, base + "_(noreverb).wav")
            sf.write(out, data, sr, format="WAV", subtype="FLOAT")
            return [out]

    mod = types.ModuleType("audio_separator")
    sep_mod = types.ModuleType("audio_separator.separator")
    sep_mod.Separator = FakeSeparator
    mod.separator = sep_mod
    monkeypatch.setitem(sys.modules, "audio_separator", mod)
    monkeypatch.setitem(sys.modules, "audio_separator.separator", sep_mod)


def _install_fake_librosa(monkeypatch):
    """A soundfile-backed `librosa.load` with a linear resample — enough to
    exercise the segmentation + dereverb + 16 kHz resample plumbing without the
    real librosa (absent from the eval-checks CI job)."""
    lib = types.ModuleType("librosa")

    def load(path, sr=None, mono=False):
        data, native = sf.read(path, dtype="float32", always_2d=True)  # (n, ch)
        if mono:
            data = data.mean(axis=1)  # (n,)
        else:
            data = data.T if data.shape[1] > 1 else data[:, 0]  # (ch, n) | (n,)
        if sr is not None and sr != native:
            n_in = data.shape[-1]
            n_out = max(1, int(round(n_in * sr / native)))
            xp = np.linspace(0.0, 1.0, n_in, endpoint=False)
            xq = np.linspace(0.0, 1.0, n_out, endpoint=False)
            if data.ndim == 1:
                data = np.interp(xq, xp, data).astype("float32")
            else:
                data = np.stack([np.interp(xq, xp, ch) for ch in data]).astype(
                    "float32"
                )
        return data, (sr if sr is not None else native)

    lib.load = load
    monkeypatch.setitem(sys.modules, "librosa", lib)


def _write_synthetic_vocals(path, sr_in=48000, dur_s=3.0):
    n = int(sr_in * dur_s)
    t = np.linspace(0.0, dur_s, n, endpoint=False)
    stereo = np.stack(
        [0.3 * np.sin(2 * np.pi * 220 * t), 0.3 * np.sin(2 * np.pi * 330 * t)],
        axis=1,
    ).astype("float32")
    sf.write(path, stereo, sr_in)  # FLAC (default PCM subtype)
    return dur_s


def test_source_has_no_isolation_model_symbol():
    """The BS-RoFormer isolation pass is deleted, so `sep_mel` is never
    referenced in the script any more (#144, owner delete-legacy doctrine)."""
    with open(os.path.join(_SCRIPTS_DIR, "lyrics_worker.py"), encoding="utf-8") as f:
        src = f.read()
    assert "sep_mel" not in src, "the BS-RoFormer isolation symbol must be gone"


def test_preprocess_vocals_dereverbs_the_stems_sidecar(tmp_path, monkeypatch):
    loaded = []
    _install_fake_torch(monkeypatch)
    _install_fake_separator(monkeypatch, loaded)
    _install_fake_librosa(monkeypatch)

    vocals_in = str(tmp_path / "song_artist_id_normalized_audio_vocals.flac")
    dur_s = _write_synthetic_vocals(vocals_in)
    out = str(tmp_path / "id_vocals16k.wav")
    args = types.SimpleNamespace(
        vocals_in=vocals_in,
        # `audio` is set only so the PRE-#144 script (which still reads --audio)
        # runs far enough to load its models — that is the RED failure. The
        # GREEN script reads `vocals_in` and never touches `audio`.
        audio=vocals_in,
        output=out,
        models_dir=str(tmp_path / "models"),
        work_dir=str(tmp_path / "id_isolation"),
        force_cpu=True,
    )
    lw.cmd_preprocess_vocals(args)

    data, sr = sf.read(out, dtype="float32")
    assert sr == 16000, "output is 16 kHz"
    assert data.ndim == 1, "output is mono"
    assert data.dtype == np.float32, "output is float32"
    expected = int(round(dur_s * 16000))
    assert abs(data.shape[0] - expected) <= 1600, "output ~= input duration"

    assert loaded == [lw.DEREVERB_MODEL], "only the anvuew dereverb model loads"
    assert not any(
        "bs_roformer" in m.lower() for m in loaded
    ), "the BS-RoFormer isolation model must NOT be instantiated"


def test_preload_warms_only_dereverb_and_the_aligner(tmp_path, monkeypatch):
    loaded = []
    _install_fake_torch(monkeypatch)
    _install_fake_separator(monkeypatch, loaded)

    aligner_calls = []
    qwen = types.ModuleType("qwen_asr")

    class FakeAligner:
        @staticmethod
        def from_pretrained(name, dtype=None, device_map=None):
            aligner_calls.append(name)
            return object()

    qwen.Qwen3ForcedAligner = FakeAligner
    monkeypatch.setitem(sys.modules, "qwen_asr", qwen)

    args = types.SimpleNamespace(models_dir=str(tmp_path / "models"))
    lw.cmd_preload(args)

    assert loaded == [lw.DEREVERB_MODEL], "preload warms only the dereverb model"
    assert aligner_calls == ["Qwen/Qwen3-ForcedAligner-0.6B"]
