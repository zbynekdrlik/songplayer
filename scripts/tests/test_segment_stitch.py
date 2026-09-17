"""#171 — pure-math tests for the resumable segment stitch in lyrics_worker.py.

Covers `_segment_bounds` coverage/overlap and `_stitch_segments` identity
reconstruction + the vocals+instrumental==mix additivity (the karaoke stem
invariant). numpy only — no models, no soundfile, no audio-separator — so it
runs in the `eval-checks` CI job in a few seconds.

The stitch is the one piece of #171 math that cannot be exercised on the
no-compile win-resolume box; verifying it here is what closes gap 2 of the
integration review (previously verified only ad hoc on dev1).
"""

import importlib.util
import os

import numpy as np
import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_lyrics_worker():
    """Import scripts/lyrics_worker.py by path. Its module body is import-safe:
    all heavy imports (numpy/librosa/soundfile/torch/audio_separator) are inside
    the command functions, and `main()` only runs under `__main__`."""
    path = os.path.join(_SCRIPTS_DIR, "lyrics_worker.py")
    spec = importlib.util.spec_from_file_location("lyrics_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


lw = _load_lyrics_worker()


def _slice_into_windows(signal, seg_samples, step_samples):
    """Slice `signal` into the exact windows `_stitch_segments` expects: window i
    is `signal[i*step_samples : i*step_samples + seg_samples]`, the last clamped
    to the end. Mirrors `_segment_bounds` in sample space."""
    segs = []
    i = 0
    n = signal.shape[0]
    while True:
        s = i * step_samples
        e = min(s + seg_samples, n)
        segs.append(signal[s:e])
        if e >= n:
            break
        i += 1
    return segs


def _seg_step_overlap(sr):
    """(segment, step, overlap) in samples at `sr`, matching cmd_preprocess_vocals."""
    seg = round(lw.ISOLATION_SEGMENT_SECONDS * sr)
    overlap = round(lw.ISOLATION_OVERLAP_SECONDS * sr)
    return seg, seg - overlap, overlap


# ---- _segment_bounds ------------------------------------------------------


@pytest.mark.parametrize("total_s", [7.0, 161.0, 210.0, 617.0, 173.37])
def test_segment_bounds_cover_and_overlap(total_s):
    seg = lw.ISOLATION_SEGMENT_SECONDS
    ov = lw.ISOLATION_OVERLAP_SECONDS
    bounds = lw._segment_bounds(total_s, seg, ov)
    assert bounds[0][0] == 0.0
    assert bounds[-1][1] == pytest.approx(total_s)
    step = seg - ov
    for i in range(1, len(bounds)):
        assert bounds[i][0] == pytest.approx(bounds[i - 1][0] + step)
        # adjacent windows overlap
        assert bounds[i][0] < bounds[i - 1][1]
    # every window except the (possibly clamped) last is a full `seg` long
    for s, e in bounds[:-1]:
        assert e - s == pytest.approx(seg)


def test_segment_bounds_short_song_is_a_single_window():
    assert lw._segment_bounds(7.0, 30.0, 2.0) == [(0.0, 7.0)]


def test_segment_bounds_empty_for_nonpositive_duration():
    assert lw._segment_bounds(0.0, 30.0, 2.0) == []
    assert lw._segment_bounds(-5.0, 30.0, 2.0) == []


# ---- _stitch_segments identity reconstruction -----------------------------


@pytest.mark.parametrize("total_s", [7, 161, 210, 617])
def test_stitch_reconstructs_the_source_mono(total_s):
    sr = 16000  # the isolation output rate
    seg, step, overlap = _seg_step_overlap(sr)
    n = total_s * sr
    rng = np.random.default_rng(total_s)
    signal = rng.standard_normal(n).astype(np.float32)
    out = lw._stitch_segments(_slice_into_windows(signal, seg, step), step, overlap)
    assert out.shape[0] == n
    assert np.max(np.abs(out - signal)) < 1e-4


def test_stitch_reconstructs_the_source_odd_length():
    # A total that is NOT a whole number of steps — exercises a clamped final
    # window shorter than a full segment.
    sr = 16000
    seg, step, overlap = _seg_step_overlap(sr)
    n = 5 * step + 173_777
    rng = np.random.default_rng(999)
    signal = rng.standard_normal(n).astype(np.float32)
    out = lw._stitch_segments(_slice_into_windows(signal, seg, step), step, overlap)
    assert out.shape[0] == n
    assert np.max(np.abs(out - signal)) < 1e-4


# ---- additivity: identical crossfade weights preserve v + i == mix --------


@pytest.mark.parametrize("total_s", [7, 61])
def test_stitch_preserves_stem_additivity_stereo(total_s):
    # Stems are 48 kHz stereo; the karaoke invariant is vocals + instrumental
    # == mix. Stitching both stems with IDENTICAL crossfade weights must
    # preserve that additivity by linearity.
    sr = 48000
    seg, step, overlap = _seg_step_overlap(sr)
    n = total_s * sr
    rng = np.random.default_rng(total_s)
    vocals = rng.standard_normal((n, 2)).astype(np.float32)
    instrumental = rng.standard_normal((n, 2)).astype(np.float32)
    mix = vocals + instrumental

    def stitch(x):
        return lw._stitch_segments(_slice_into_windows(x, seg, step), step, overlap)

    sv = stitch(vocals)
    si = stitch(instrumental)
    sm = stitch(mix)
    assert sv.shape == (n, 2)
    assert np.max(np.abs((sv + si) - sm)) < 1e-4
