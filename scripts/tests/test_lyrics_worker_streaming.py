"""#207 — `preprocess-vocals` streams the vocals sidecar and the stitch.

The heavy child runs under a 10 GiB per-process job cap, and
`cmd_preprocess_vocals` held whole-song arrays: the native-rate vocals
(`librosa.load(sr=None, mono=False)`, ~1.4 GB of float32 for a 60 min song)
and, at the end, every 16 kHz segment + the float64 overlap-add output + its
weight sum. These tests pin the streaming replacement in
`scripts/lyrics_worker.py`, the stems worker's design (`7ff38c73`):

- `_read_window` returns exactly the slice the old whole-file load gave, so
  each segment's dereverb input is the same;
- `_stitch_to_wav` writes the SAME samples as `_stitch_segments` + the global
  peak normalisation + `_atomic_write_wav`, in two passes that each hold one
  segment and the overlap tail;
- a failed write publishes nothing;
- `cmd_preprocess_vocals` never loads the whole vocals file and never builds
  the whole-song stitch.

numpy + soundfile only (the `eval-checks` CI env); torch / librosa /
audio_separator are fakes (lyrics-worker-tests.md).
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


def _load_lyrics_worker():
    path = os.path.join(_SCRIPTS_DIR, "lyrics_worker.py")
    spec = importlib.util.spec_from_file_location("lyrics_worker_streaming", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


lw = _load_lyrics_worker()


# ---- the window read ---------------------------------------------------------


@pytest.mark.parametrize("channels", [1, 2])
def test_read_window_is_the_old_whole_file_slice(tmp_path, channels):
    sr = 48000
    n = 7 * sr + 123
    rng = np.random.default_rng(channels)
    shape = (n,) if channels == 1 else (n, channels)
    path = str(tmp_path / "vocals.flac")
    sf.write(path, rng.uniform(-0.9, 0.9, shape), sr, format="FLAC", subtype="PCM_24")
    # What the old code sliced: `librosa.load(sr=None, mono=False)` is a
    # soundfile float32 read, (n,) mono or (ch, n), sliced and transposed.
    full, _ = sf.read(path, dtype="float32", always_2d=False)
    for start_s, end_s in [(0.0, 3.0), (2.5, 5.5), (5.0, 7.5), (6.9, 9.0), (8.0, 9.0)]:
        s0 = max(0, int(round(start_s * sr)))
        s1 = int(round(end_s * sr))
        want = full[s0:s1] if channels == 1 else full.T[:, s0:s1].T
        got = lw._read_window(path, sr, start_s, end_s)
        assert got.dtype == np.float32
        assert got.shape == want.shape, (start_s, end_s)
        assert np.array_equal(got, want), (start_s, end_s)


def test_audio_info_reads_the_header(tmp_path):
    path = str(tmp_path / "vocals.flac")
    sf.write(path, np.zeros((4321, 2)), 44100, format="FLAC", subtype="PCM_16")
    assert lw._audio_info(path) == (44100, 4321)


def _flac_with_total_samples(path, total):
    """Rewrite the 36-bit STREAMINFO `total samples` field of a FLAC file.
    0 means "unknown" (what a piped / streaming encoder writes)."""
    raw = bytearray(open(path, "rb").read())
    assert raw[:4] == b"fLaC" and (raw[4] & 0x7F) == 0  # STREAMINFO first
    off = 8 + 10
    packed = int.from_bytes(raw[off : off + 8], "big")
    packed = (packed & ~((1 << 36) - 1)) | (total & ((1 << 36) - 1))
    raw[off : off + 8] = packed.to_bytes(8, "big")
    open(path, "wb").write(bytes(raw))


def test_audio_info_refuses_an_unknown_frame_count(tmp_path):
    # The segment plan trusts the header: an unknown length (libsndfile's huge
    # sentinel) would plan a bogus number of windows, so it fails fast.
    path = str(tmp_path / "vocals.flac")
    sf.write(path, np.zeros((4000, 2)), 8000, format="FLAC", subtype="PCM_16")
    _flac_with_total_samples(path, 0)
    with pytest.raises(ValueError, match="frame count"):
        lw._audio_info(path)


# ---- the streamed stitch -----------------------------------------------------


def _segments(lengths, seed, peak):
    """Independent random mono segments (so the crossfade matters) whose
    largest magnitude is about `peak`."""
    rng = np.random.default_rng(seed)
    return [rng.uniform(-peak, peak, n).astype(np.float32) for n in lengths]


def _reference(segs, step, overlap, path):
    """The whole-array path the streaming one replaces (cmd_preprocess_vocals
    before #207): stitch, normalise by the global peak over 1.0, write."""
    stitched = lw._stitch_segments(segs, step, overlap)
    peak = float(np.max(np.abs(stitched))) if stitched.size else 0.0
    if peak > 1.0:
        stitched = stitched / peak
    lw._atomic_write_wav(path, stitched, 16000)


_STEP, _OV = 1000, 240
_SEG = _STEP + _OV
_CASES = [
    ("one-segment", [_SEG], _STEP, _OV),
    ("one-short-segment", [137], _STEP, _OV),
    ("two-segments", [_SEG, _SEG], _STEP, _OV),
    ("many-segments", [_SEG] * 9, _STEP, _OV),
    ("short-last-segment", [_SEG] * 4 + [_OV + 57], _STEP, _OV),
    ("jittered-lengths", [_SEG, _SEG - 3, _SEG - 1, _SEG, 700], _STEP, _OV),
    ("a-gap-before-the-next-segment", [_SEG, 900, _SEG, 500], _STEP, _OV),
    ("no-overlap", [_STEP] * 3 + [400], _STEP, 0),
]


@pytest.mark.parametrize("peak", [0.8, 1.3])
@pytest.mark.parametrize(
    "name,lengths,step,overlap", _CASES, ids=[c[0] for c in _CASES]
)
def test_streamed_stitch_matches_the_reference(
    tmp_path, name, lengths, step, overlap, peak
):
    segs = _segments(lengths, seed=len(lengths) * 31 + int(peak * 10), peak=peak)
    ref_path = str(tmp_path / "ref.wav")
    _reference(segs, step, overlap, ref_path)

    out_path = str(tmp_path / "stream.wav")
    lw._stitch_to_wav(lambda i: segs[i], len(segs), step, overlap, out_path)

    got, sr = sf.read(out_path, dtype="float32")
    want, _ = sf.read(ref_path, dtype="float32")
    assert sr == 16000
    assert got.shape == want.shape
    # FLOAT WAV holds the float32 samples exactly: bit-identical.
    assert np.array_equal(got, want)
    info, ref_info = sf.info(out_path), sf.info(ref_path)
    assert (
        (info.format, info.subtype, info.channels)
        == (
            ref_info.format,
            ref_info.subtype,
            ref_info.channels,
        )
        == ("WAV", "FLOAT", 1)
    )
    assert not os.path.exists(out_path + ".tmp")


def test_a_peak_over_1_normalises_the_whole_output_by_it(tmp_path):
    # One sample at 2.0 in the LAST segment: every earlier sample, written
    # long before it was read, is divided by it too.
    segs = _segments([_SEG] * 3, seed=7, peak=0.5)
    segs[2][-5] = 2.0
    out_path = str(tmp_path / "stream.wav")
    lw._stitch_to_wav(lambda i: segs[i], 3, _STEP, _OV, out_path)
    got, _ = sf.read(out_path, dtype="float32")
    assert np.array_equal(got[:100], segs[0][:100] / np.float32(2.0))
    assert got[-5] == np.float32(1.0)


def test_the_stitch_reads_one_segment_per_block_it_settles():
    # Never "read every segment, then stitch": each block is settled as soon
    # as the next segment's start is known, one block per segment read, and
    # each block but the last is exactly one step long (only the overlap
    # tail is kept back).
    lengths = [_SEG] * 5 + [_OV + 33]
    segs = _segments(lengths, seed=3, peak=0.9)
    events = []

    def read(i):
        events.append(("read", i))
        return segs[i]

    sizes = []
    for block in lw._stitched_blocks(read, len(segs), _STEP, _OV):
        events.append(("block", len(sizes)))
        sizes.append(block.shape[0])
    assert events == [e for i in range(len(segs)) for e in (("read", i), ("block", i))]
    assert sizes == [_STEP] * 5 + [_OV + 33]
    assert sum(sizes) == lw._stitch_segments(segs, _STEP, _OV).shape[0]


def test_an_earlier_segment_past_the_last_one_raises_like_the_reference(tmp_path):
    # The reference sizes its output by the LAST segment, so an earlier one
    # reaching past it does not fit (a numpy broadcast ValueError there).
    segs = _segments([_SEG, 100], seed=1, peak=0.5)
    with pytest.raises(ValueError):
        lw._stitch_segments(segs, _STEP, _OV)
    out_path = str(tmp_path / "stream.wav")
    with pytest.raises(ValueError):
        lw._stitch_to_wav(lambda i: segs[i], 2, _STEP, _OV, out_path)
    assert not os.path.exists(out_path)
    assert not os.path.exists(out_path + ".tmp")


def test_a_failed_write_publishes_nothing(tmp_path):
    segs = _segments([_SEG] * 4, seed=2, peak=0.5)
    out_path = str(tmp_path / "stream.wav")
    open(out_path, "wb").write(b"the previous output")
    calls = {"n": 0}

    def read(i):
        calls["n"] += 1
        if calls["n"] > len(segs) + 2:  # the second pass, mid-file
            raise OSError("disk gone")
        return segs[i]

    with pytest.raises(OSError, match="disk gone"):
        lw._stitch_to_wav(read, len(segs), _STEP, _OV, out_path)
    assert open(out_path, "rb").read() == b"the previous output"
    assert not os.path.exists(out_path + ".tmp")


# ---- cmd_preprocess_vocals end to end with a fake model stack ----------------


def _install_fakes(monkeypatch, vocals_path, dereverb_inputs):
    """Fake torch / audio_separator / librosa. The fake dereverb returns its
    input times 1.7 (so a segment's own peak clamp runs) and records the
    window it was given. The fake librosa.load REFUSES the vocals sidecar:
    the heavy child must never load it whole. It resamples 48 kHz → 16 kHz
    by taking every third sample of the channel mean."""
    torch = types.ModuleType("torch")
    torch.bfloat16 = object()
    torch.cuda = SimpleNamespace(
        is_available=lambda: False,
        device_count=lambda: 0,
        empty_cache=lambda: None,
        set_per_process_memory_fraction=lambda _f: None,
        OutOfMemoryError=type("OutOfMemoryError", (Exception,), {}),
    )
    monkeypatch.setitem(sys.modules, "torch", torch)

    def fake_load(path, sr=None, mono=False):
        if os.path.abspath(path) == os.path.abspath(vocals_path):
            raise AssertionError(
                "#207: the whole vocals sidecar was loaded into memory"
            )
        data, native = sf.read(path, dtype="float32", always_2d=True)
        assert (sr, mono, native) == (16000, True, 48000), "only the 16 kHz downsample"
        return data.mean(axis=1)[::3].astype(np.float32), sr

    librosa = types.ModuleType("librosa")
    librosa.load = fake_load
    monkeypatch.setitem(sys.modules, "librosa", librosa)

    class FakeSeparator:
        def __init__(
            self, model_file_dir=None, output_format=None, output_dir=None, **_k
        ):
            self.output_dir = output_dir

        def load_model(self, name):
            assert name == lw.DEREVERB_MODEL

        def separate(self, in_path):
            data, sr = sf.read(in_path, dtype="float32", always_2d=False)
            dereverb_inputs.append(data)
            out = os.path.join(self.output_dir, "seg_(noreverb).wav")
            sf.write(out, data * np.float32(1.7), sr, format="WAV", subtype="FLOAT")
            return [out]

    mod = types.ModuleType("audio_separator")
    sep_mod = types.ModuleType("audio_separator.separator")
    sep_mod.Separator = FakeSeparator
    mod.separator = sep_mod
    monkeypatch.setitem(sys.modules, "audio_separator", mod)
    monkeypatch.setitem(sys.modules, "audio_separator.separator", sep_mod)


def test_preprocess_vocals_streams_the_vocals_and_the_stitch(tmp_path, monkeypatch):
    # Shrink the window geometry so the test is fast; the logic is identical.
    monkeypatch.setattr(lw, "ISOLATION_SEGMENT_SECONDS", 3.0)
    monkeypatch.setattr(lw, "ISOLATION_OVERLAP_SECONDS", 0.5)
    sr = 48000
    n = int(round(11.7 * sr))  # 5 windows, the last one short
    rng = np.random.default_rng(207)
    vocals = str(tmp_path / "song_artist_id_normalized_audio_vocals.flac")
    sf.write(
        vocals, rng.uniform(-0.8, 0.8, (n, 2)), sr, format="FLAC", subtype="PCM_24"
    )
    full, _ = sf.read(vocals, dtype="float32", always_2d=True)

    dereverb_inputs = []
    _install_fakes(monkeypatch, vocals, dereverb_inputs)
    # Every 16 kHz segment the run writes, captured on its way to disk.
    work_dir = str(tmp_path / "id_isolation")
    segments = {}
    atomic_write = lw._atomic_write_wav

    def spy_write(path, audio, rate):
        if os.path.dirname(path) == work_dir:
            segments[os.path.basename(path)] = np.array(audio, copy=True)
        atomic_write(path, audio, rate)

    monkeypatch.setattr(lw, "_atomic_write_wav", spy_write)
    reference_stitch = lw._stitch_segments

    def no_whole_stitch(*_a, **_k):
        raise AssertionError("#207: the whole-song stitch was built in memory")

    monkeypatch.setattr(lw, "_stitch_segments", no_whole_stitch)

    out = str(tmp_path / "id_vocals16k.wav")
    args = SimpleNamespace(
        vocals_in=vocals,
        output=out,
        models_dir=str(tmp_path / "models"),
        work_dir=work_dir,
        force_cpu=True,
    )
    lw.cmd_preprocess_vocals(args)

    # Each segment's dereverb input is exactly the old whole-file slice.
    bounds = lw._segment_bounds(n / sr, 3.0, 0.5)
    assert len(bounds) == len(dereverb_inputs) == 5
    for (start_s, end_s), got in zip(bounds, dereverb_inputs):
        s0 = max(0, int(round(start_s * sr)))
        s1 = int(round(end_s * sr))
        assert got.dtype == np.float32
        assert np.array_equal(got, full[s0:s1]), (start_s, end_s)

    # The output is the reference stitch of the very segments written.
    segs = [segments[name] for name in sorted(segments)]
    assert len(segs) == 5
    step = int(round((3.0 - 0.5) * 16000))
    overlap = int(round(0.5 * 16000))
    want = reference_stitch(segs, step, overlap)
    peak = float(np.max(np.abs(want)))
    if peak > 1.0:
        want = want / peak
    got, rate = sf.read(out, dtype="float32")
    assert rate == 16000
    assert np.array_equal(got, want)
    assert not os.path.exists(work_dir), "the work dir is removed"
    assert not os.path.exists(out + ".tmp")
