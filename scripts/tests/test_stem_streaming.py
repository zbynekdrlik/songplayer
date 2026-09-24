"""#207 — stem separation streams the mix and the stitched stems.

Every video longer than ~35 min failed separation because the heavy child
(10 GiB per-process job cap) held whole-video arrays: the whole native-rate mix
(`librosa.load`), every segment at once plus the full stitched output
(`_stitch_segments`), plus another full copy in the FLAC write. These tests pin
the streaming replacement in `scripts/stem_worker.py`:

- `_read_window` returns exactly the slice the old whole-mix load produced;
- `_StreamingStitchWriter` writes the SAME samples as `_stitch_segments` + the
  existing atomic PCM_24 writer, while holding only the overlap tail;
- a failed write publishes nothing (no `.tmp`, no final file);
- `cmd_separate` never loads the whole mix (fake model stack, real soundfile).

numpy + soundfile only (the `eval-checks` CI env) — torch / librosa /
audio_separator are faked via `sys.modules` (lyrics-worker-tests.md).
"""

import importlib.util
import os
import sys
import types
from types import SimpleNamespace

import numpy as np
import pytest
import soundfile as sf

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# One 24-bit LSB, as read back by libsndfile (int24 / 2**23).
LSB24 = 1.0 / (1 << 23)


def _load_stem_worker():
    """Import scripts/stem_worker.py by path. Its module body is import-safe:
    every heavy import lives inside the command functions."""
    path = os.path.join(_SCRIPTS_DIR, "stem_worker.py")
    spec = importlib.util.spec_from_file_location("stem_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sw = _load_stem_worker()


def _random_segments(lengths, channels, seed):
    """Independent random segments (so the crossfade actually matters), with a
    peak above 1.0 so the per-block clip is exercised too."""
    rng = np.random.default_rng(seed)
    segs = []
    for n in lengths:
        shape = (n,) if channels == 1 else (n, channels)
        segs.append(rng.uniform(-1.3, 1.3, size=shape).astype(np.float32))
    return segs


def _stream(segments, out_path, step, overlap, channels):
    with sw._StreamingStitchWriter(
        out_path, len(segments), step, overlap, channels=channels
    ) as w:
        for seg in segments:
            w.add_segment(seg)
    return w


# (name, segment lengths, step, overlap). seg = step + overlap.
_STEP, _OV = 1000, 240
_SEG = _STEP + _OV
_CASES = [
    ("one-segment", [_SEG], _STEP, _OV),
    ("one-short-segment", [137], _STEP, _OV),
    ("two-segments", [_SEG, _SEG], _STEP, _OV),
    ("many-segments", [_SEG] * 9, _STEP, _OV),
    ("short-last-segment", [_SEG] * 4 + [_OV + 57], _STEP, _OV),
    ("jittered-lengths", [_SEG, _SEG - 3, _SEG - 1, _SEG, 700], _STEP, _OV),
    ("no-overlap", [_STEP] * 3 + [400], _STEP, 0),
]


@pytest.mark.parametrize("channels", [1, 2])
@pytest.mark.parametrize(
    "name,lengths,step,overlap", _CASES, ids=[c[0] for c in _CASES]
)
def test_streaming_writer_matches_reference_stitch(
    tmp_path, name, lengths, step, overlap, channels
):
    segs = _random_segments(lengths, channels, seed=len(lengths) * 31 + channels)

    ref = sw._stitch_segments(segs, step, overlap)
    ref_path = str(tmp_path / "ref.flac")
    sw._write_array_48k_stereo(ref, ref_path)

    out_path = str(tmp_path / "stream.flac")
    _stream(segs, out_path, step, overlap, channels)

    got, sr = sf.read(out_path, dtype="float64", always_2d=False)
    want, _ = sf.read(ref_path, dtype="float64", always_2d=False)
    assert sr == sw.OUTPUT_SAMPLE_RATE
    assert got.shape == want.shape == ref.shape
    # Same samples as the reference stitch written by the existing writer:
    # within 1 LSB of 24-bit (it is in fact bit-identical).
    assert np.max(np.abs(got - want)) <= LSB24
    # And against the float reference itself: PCM_24 quantisation only.
    assert np.max(np.abs(got - np.clip(ref, -1.0, 1.0))) <= 2 * LSB24
    assert sf.info(out_path).subtype == "PCM_24"


def test_streaming_writer_real_stem_constants_stereo(tmp_path):
    # The production geometry: 30 s windows, 2 s overlap, 48 kHz stereo, a
    # short last window.
    sr = sw.OUTPUT_SAMPLE_RATE
    step = int(round((sw.STEM_SEGMENT_SECONDS - sw.STEM_OVERLAP_SECONDS) * sr))
    overlap = int(round(sw.STEM_OVERLAP_SECONDS * sr))
    segs = _random_segments([step + overlap] * 2 + [overlap + 4321], 2, seed=207)

    ref_path = str(tmp_path / "ref.flac")
    sw._write_array_48k_stereo(sw._stitch_segments(segs, step, overlap), ref_path)
    out_path = str(tmp_path / "stream.flac")
    w = _stream(segs, out_path, step, overlap, 2)

    got, _ = sf.read(out_path, dtype="float64")
    want, _ = sf.read(ref_path, dtype="float64")
    assert got.shape == want.shape == (2 * step + overlap + 4321, 2)
    assert np.max(np.abs(got - want)) <= LSB24
    assert w.max_retained_samples <= overlap


def test_streaming_writer_preserves_stem_additivity(tmp_path):
    # vocals + instrumental == mix survives the streaming stitch (identical
    # crossfade weights on both stems, karaoke-stems invariant).
    step, overlap = 900, 300
    lengths = [step + overlap] * 6 + [overlap + 11]
    rng = np.random.default_rng(5)
    vocals = [rng.uniform(-0.4, 0.4, (n, 2)).astype(np.float32) for n in lengths]
    instr = [rng.uniform(-0.4, 0.4, (n, 2)).astype(np.float32) for n in lengths]
    mix = [v + i for v, i in zip(vocals, instr)]
    paths = {}
    for name, segs in (("v", vocals), ("i", instr), ("m", mix)):
        paths[name] = str(tmp_path / f"{name}.flac")
        _stream(segs, paths[name], step, overlap, 2)
    v, _ = sf.read(paths["v"], dtype="float64")
    i, _ = sf.read(paths["i"], dtype="float64")
    m, _ = sf.read(paths["m"], dtype="float64")
    assert np.max(np.abs((v + i) - m)) <= 3 * LSB24


@pytest.mark.parametrize("n_segments", [3, 12, 80])
def test_streaming_writer_retains_at_most_the_overlap(tmp_path, n_segments):
    step, overlap = 500, 120
    segs = _random_segments([step + overlap] * n_segments, 2, seed=n_segments)
    w = _stream(segs, str(tmp_path / "o.flac"), step, overlap, 2)
    # The held buffer is the previous segment's overlap tail — independent of
    # how many segments (how long the video) there are.
    assert w.max_retained_samples == overlap
    assert w.retained_samples == 0  # everything flushed on close


def test_streaming_writer_leaves_no_tmp_on_success(tmp_path):
    out_path = str(tmp_path / "stem.flac")
    _stream(_random_segments([700, 700, 300], 2, 1), out_path, 500, 200, 2)
    assert os.path.exists(out_path)
    assert not os.path.exists(out_path + ".tmp")
    assert sorted(os.listdir(tmp_path)) == ["stem.flac"]


def test_streaming_writer_failure_midway_publishes_nothing(tmp_path):
    out_path = str(tmp_path / "stem.flac")
    segs = _random_segments([700, 700, 300], 2, 2)
    with pytest.raises(RuntimeError, match="boom"):
        with sw._StreamingStitchWriter(out_path, 3, 500, 200, channels=2) as w:
            w.add_segment(segs[0])
            raise RuntimeError("boom")
    assert not os.path.exists(out_path)
    assert not os.path.exists(out_path + ".tmp")


def test_streaming_writer_failure_keeps_the_previous_final_file(tmp_path):
    # Atomic publish: a failed rewrite never touches an existing sidecar.
    out_path = str(tmp_path / "stem.flac")
    _stream(_random_segments([700, 300], 2, 3), out_path, 500, 200, 2)
    before = open(out_path, "rb").read()
    with pytest.raises(RuntimeError, match="boom"):
        with sw._StreamingStitchWriter(out_path, 2, 500, 200, channels=2) as w:
            w.add_segment(_random_segments([700], 2, 4)[0])
            raise RuntimeError("boom")
    assert open(out_path, "rb").read() == before
    assert not os.path.exists(out_path + ".tmp")


def test_streaming_writer_missing_segments_publish_nothing(tmp_path):
    # Declared 3 segments, only 2 added: a truncated stem must never publish.
    out_path = str(tmp_path / "stem.flac")
    segs = _random_segments([700, 700], 2, 6)
    with pytest.raises(RuntimeError, match="2 of 3"):
        with sw._StreamingStitchWriter(out_path, 3, 500, 200, channels=2) as w:
            for seg in segs:
                w.add_segment(seg)
    assert not os.path.exists(out_path)
    assert not os.path.exists(out_path + ".tmp")


def test_streaming_writer_rejects_overlap_longer_than_step(tmp_path):
    with pytest.raises(ValueError):
        sw._StreamingStitchWriter(str(tmp_path / "x.flac"), 2, 100, 101, channels=2)
    assert os.listdir(tmp_path) == []


# ---- windowed input read ---------------------------------------------------


@pytest.mark.parametrize(
    "sr,channels,subtype,total_s",
    [
        (8000, 2, "PCM_24", 71.3),
        (8000, 1, "PCM_16", 64.0),
        (11025, 2, "PCM_16", 29.0),
    ],
)
def test_windowed_read_equals_slice_of_full_read(
    tmp_path, sr, channels, subtype, total_s
):
    n = int(round(total_s * sr))
    rng = np.random.default_rng(n)
    shape = (n,) if channels == 1 else (n, channels)
    path = str(tmp_path / "mix.flac")
    sf.write(path, rng.uniform(-0.9, 0.9, shape), sr, format="FLAC", subtype=subtype)

    in_sr, frames = sw._audio_info(path)
    assert (in_sr, frames) == (sr, n)

    # What the old code sliced: librosa.load(sr=None, mono=False) is a
    # soundfile float32 read, transposed to (ch, n) for multi-channel.
    full, _ = sf.read(path, dtype="float32", always_2d=False)
    full = full.T
    bounds = sw._segment_bounds(
        frames / float(in_sr), sw.STEM_SEGMENT_SECONDS, sw.STEM_OVERLAP_SECONDS
    )
    assert len(bounds) >= 1
    for start_s, end_s in bounds:
        s0 = max(0, int(round(start_s * in_sr)))
        s1 = int(round(end_s * in_sr))
        old = full[s0:s1] if full.ndim == 1 else full[:, s0:s1].T
        got = sw._read_window(path, in_sr, start_s, end_s)
        assert got.dtype == np.float32
        assert got.shape == old.shape
        assert np.array_equal(got, old)


# ---- cmd_separate end to end with a fake model stack -----------------------


def _fake_torch():
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


def _install_fakes(monkeypatch, mix_path):
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

    monkeypatch.setitem(sys.modules, "torch", _fake_torch())
    monkeypatch.setitem(sys.modules, "librosa", librosa)
    monkeypatch.setitem(sys.modules, "audio_separator", sep_pkg)
    monkeypatch.setitem(sys.modules, "audio_separator.separator", sep_mod)
    return loads, windows


def test_cmd_separate_streams_without_loading_the_whole_mix(tmp_path, monkeypatch):
    # Shrink the window geometry so the test is fast; the logic is identical.
    monkeypatch.setattr(sw, "STEM_SEGMENT_SECONDS", 3.0)
    monkeypatch.setattr(sw, "STEM_OVERLAP_SECONDS", 0.5)
    sr = sw.OUTPUT_SAMPLE_RATE
    n = int(round(11.7 * sr))  # 5 windows, the last one short
    rng = np.random.default_rng(207)
    mix = rng.uniform(-0.8, 0.8, (n, 2)).astype(np.float32)
    mix_path = str(tmp_path / "song_audio.flac")
    sf.write(mix_path, mix, sr, format="FLAC", subtype="PCM_24")
    mix, _ = sf.read(mix_path, dtype="float32")  # the quantised mix

    loads, windows = _install_fakes(monkeypatch, mix_path)
    work_dir = str(tmp_path / "work")
    vocals_out = str(tmp_path / "song_audio_vocals.flac")
    instr_out = str(tmp_path / "song_audio_instrumental.flac")
    args = SimpleNamespace(
        audio=mix_path,
        vocals_out=vocals_out,
        instrumental_out=instr_out,
        models_dir=str(tmp_path / "models"),
        work_dir=work_dir,
        force_cpu=False,
    )
    sw.cmd_separate(args)

    assert loads, "the separated segment stems were loaded"
    # The separator got exactly the old window bounds, one window at a time.
    expected = [
        (int(round(e * sr)) - int(round(s * sr)), 2)
        for s, e in sw._segment_bounds(n / sr, 3.0, 0.5)
    ]
    assert len(expected) == 5
    assert windows == expected
    assert not os.path.exists(work_dir)
    for p in (vocals_out, instr_out):
        assert not os.path.exists(p + ".tmp")
        info = sf.info(p)
        assert (info.samplerate, info.channels, info.subtype) == (sr, 2, "PCM_24")
        assert info.frames == n
    v, _ = sf.read(vocals_out, dtype="float64")
    i, _ = sf.read(instr_out, dtype="float64")
    # Segment-wise gain stitched back = the gain on the whole mix.
    assert np.max(np.abs(v - 0.3 * mix)) <= 2 * LSB24
    assert np.max(np.abs(i - 0.7 * mix)) <= 2 * LSB24
    assert np.max(np.abs((v + i) - mix)) <= 3 * LSB24
