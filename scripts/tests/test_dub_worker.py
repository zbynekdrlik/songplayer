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


def _results_fixture() -> list:
    """Two per-chunk results as `_process_chunk` returns them (cached or fresh):
    each already carries `at_ms` + `tempo` from the placement the mix applied."""
    return [
        {
            "index": 0,
            "chunk_start_ms": 0,
            "chunk_end_ms": 60_000,
            "out_len_ms": 61_000,
            "next_start_ms": 60_000,
            "tempo": 1.05,
            "at_ms": 0,
            "transcript_en": "Hello there friends",
            "transcript_sk": "Ahojte priatelia",
            "sk_timed": [
                {"t_ms": 500, "text": "Ahojte"},
                {"t_ms": 1200, "text": " priatelia"},
            ],
        },
        {
            "index": 1,
            "chunk_start_ms": 60_000,
            "chunk_end_ms": 120_000,
            "out_len_ms": 58_000,
            "next_start_ms": None,
            "tempo": 1.0,
            "at_ms": 60_000,
            "transcript_en": "Goodbye",
            "transcript_sk": "Dovidenia",
            "sk_timed": [{"t_ms": 400, "text": "Dovidenia"}],
        },
    ]


def test_build_transcripts_includes_at_ms_and_tempo():
    # D3 (#182): the transcripts JSON must carry each chunk's video-timeline
    # placement (`at_ms`) + applied `tempo` so the Rust subtitle builder can map
    # chunk-local SK positions onto the video timeline with NO second pass.
    t = dw.build_transcripts(_results_fixture())
    assert t["engine"] == "gemini-live-translate"
    assert t["target_lang"] == "sk"
    assert len(t["chunks"]) == 2

    c0 = t["chunks"][0]
    assert c0["index"] == 0
    assert c0["start_ms"] == 0
    assert c0["end_ms"] == 60_000
    # The two D3 fields — read from the SAME placement the mix uses.
    assert c0["at_ms"] == 0
    assert c0["tempo"] == 1.05
    assert c0["en"] == "Hello there friends"
    assert c0["sk"] == "Ahojte priatelia"
    assert c0["sk_timed"][0] == {"t_ms": 500, "text": "Ahojte"}

    c1 = t["chunks"][1]
    assert c1["at_ms"] == 60_000
    assert c1["tempo"] == 1.0
    assert c1["sk_timed"] == [{"t_ms": 400, "text": "Dovidenia"}]


def test_chunk_reusable_same_voice_reuses():
    # #184 round C: a cached chunk recorded under the SAME voice is reused.
    assert dw.chunk_reusable({"index": 0, "voice": "Charon"}, "Charon") is True


def test_chunk_reusable_different_voice_resynth():
    # A cached chunk recorded under ANOTHER voice must NOT be reused — the whole
    # dub must speak in one voice, so it is re-synthesized with the new one.
    assert dw.chunk_reusable({"index": 0, "voice": "Kore"}, "Charon") is False


def test_chunk_reusable_missing_voice_resynth():
    # A legacy chunk (pre-round-C) has no `voice` key → not reusable.
    assert dw.chunk_reusable({"index": 0}, "Charon") is False


# ── #184 round E: the per-chunk voice-band guard (pure helpers) ──────────────────


def test_chunk_voice_drift_drifted_over_steady_input():
    # The output jumps an octave up for the last windows while the input stays
    # steady → those windows are drift, not source-following. 3 of 9 voiced.
    drifted, voiced = dw.chunk_voice_drift(
        [100, 100, 100, 100, 100, 100, 220, 220, 220], [110] * 9
    )
    assert (drifted, voiced) == (3, 9)
    assert drifted / voiced > 0.20


def test_chunk_voice_drift_source_following_rise_is_not_drift():
    # The output rises WITH the source (the correct high-source case) → not drift.
    out = [100, 100, 100, 220, 220]
    assert dw.chunk_voice_drift(out, out) == (0, 5)


def test_chunk_voice_drift_empty_and_unvoiced():
    assert dw.chunk_voice_drift([], []) == (0, 0)
    assert dw.chunk_voice_drift([0, 0, 0], [100, 100]) == (0, 0)


def test_chunk_voice_drift_aligns_mismatched_window_counts():
    # The input has fewer windows (tempo compressed it); a high output stretch
    # over a steady input still counts as drift after fraction-alignment.
    drifted, voiced = dw.chunk_voice_drift([100] * 7 + [220] * 3, [110] * 5)
    assert (drifted, voiced) == (3, 10)


def test_chunk_is_drifted_threshold():
    # Drifted when drifted/voiced > 0.20; the boundary and zero-voiced cases.
    assert dw._chunk_is_drifted(3, 9) is True  # 0.33
    assert dw._chunk_is_drifted(2, 10) is False  # exactly 0.20, not > 0.20
    assert dw._chunk_is_drifted(3, 10) is True  # 0.30
    assert dw._chunk_is_drifted(0, 0) is False
    assert dw._chunk_is_drifted(0, 8) is False
