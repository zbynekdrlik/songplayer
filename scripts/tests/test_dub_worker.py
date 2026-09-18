"""Pure-helper tests for scripts/dub_worker.py (#183 D4).

The dub child's runtime work (Live API, ffmpeg) cannot run in CI, but its pure
helpers can: PCM position/length math, the placement/atempo decision (which must
match the Rust `dabing::chunk_plan::placement_for`), the ffmpeg slice/resample
argv, the trailing-silence filter, and the mix filter_complex builder. stdlib
only — no google.genai, no ffmpeg — so it runs in the `eval-checks` CI job.

The module body of dub_worker.py is import-safe: all heavy imports (google.genai)
are inside the command functions, so importing it by path here is safe.
"""

import importlib.util
import os

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_dub_worker():
    path = os.path.join(_SCRIPTS_DIR, "dub_worker.py")
    spec = importlib.util.spec_from_file_location("dub_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dw = _load_dub_worker()


def test_pcm_len_ms_and_pos():
    # 24 kHz s16le mono: 48000 bytes = 24000 samples = 1000 ms.
    assert dw.pcm_len_ms(48000, 24000) == 1000
    assert dw.pcm_pos_to_ms(24000, 24000) == 500
    assert dw.pcm_len_ms(0, 24000) == 0
    assert dw.pcm_len_ms(100, 0) == 0


def test_placement_matches_rust_spec():
    # Fits before the next chunk -> no tempo change, placed at chunk start.
    at, tempo = dw.placement(120_000, 60_000, 55_000, 180_000)
    assert at == 120_000
    assert abs(tempo - 1.0) < 1e-6

    # Slight overrun -> speed up just enough, under the cap.
    _, tempo = dw.placement(0, 60_000, 63_000, 60_000)
    assert abs(tempo - 1.05) < 1e-3
    assert tempo <= dw.MAX_TEMPO

    # Big overrun -> clamp to MAX_TEMPO.
    _, tempo = dw.placement(0, 60_000, 90_000, 60_000)
    assert abs(tempo - dw.MAX_TEMPO) < 1e-6

    # Last chunk (no next) is never sped up.
    at, tempo = dw.placement(600_000, 120_000, 130_000, None)
    assert at == 600_000
    assert abs(tempo - 1.0) < 1e-6


def test_slice_resample_args_target_16k_mono_s16le():
    args = dw.slice_resample_args("ffmpeg", "/c/a.flac", 10_000, 70_000, "/w/c0.pcm")
    assert args[0] == "ffmpeg"
    assert "-i" in args and "/c/a.flac" in args
    # Cut bounds present as seconds.
    assert "10.000" in args
    assert "70.000" in args
    # Live-API input format: 16 kHz mono s16le.
    assert "16000" in args
    assert args[args.index("-ac") + 1] == "1"
    assert "s16le" in args
    assert args[-1] == "/w/c0.pcm"


def test_trim_silence_af_reverses_around_silenceremove():
    af = dw.trim_silence_af()
    assert af.startswith("areverse,silenceremove=")
    assert af.endswith(",areverse")


def test_build_mix_filter_places_and_mixes_each_chunk():
    filt = dw.build_mix_filter([(1.0, 0), (1.05, 60_000)])
    # One atempo+adelay per input, at its at_ms.
    assert "[0:a]atempo=1.0000,adelay=0:all=1[a0]" in filt
    assert "[1:a]atempo=1.0500,adelay=60000:all=1[a1]" in filt
    # A single amix of both, resampled to 48 kHz, labelled [mix].
    assert "[a0][a1]amix=inputs=2:normalize=0,aresample=48000[mix]" in filt


def test_drain_deadline_is_input_seconds_plus_drain():
    # 16 kHz s16le mono: 32000 bytes/s. 320000 bytes = 10 s input.
    assert dw.drain_deadline_s(320_000, drain_s=5.0) == 15.0
