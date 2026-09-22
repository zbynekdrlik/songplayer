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
import sys

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# `scripts/` on the path so `dub_worker`'s `import dub_voice_check` (inside
# `chunk_voice_drift`) resolves the same way it does on the box (the child's
# own dir is on sys.path there). Also lets this test import the shared helpers.
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)


def _load_dub_worker():
    path = os.path.join(_SCRIPTS_DIR, "dub_worker.py")
    spec = importlib.util.spec_from_file_location("dub_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _load_dub_voice_check():
    path = os.path.join(_SCRIPTS_DIR, "dub_voice_check.py")
    spec = importlib.util.spec_from_file_location("dub_voice_check", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dw = _load_dub_worker()
dvc = _load_dub_voice_check()


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


def _cached(voice="Charon", start_ms=0, end_ms=120_000):
    return {
        "index": 0,
        "voice": voice,
        "chunk_start_ms": start_ms,
        "chunk_end_ms": end_ms,
    }


def test_chunk_reusable_same_voice_same_bounds_reuses():
    # #184 round C: a cached chunk recorded under the SAME voice (and, round E,
    # the SAME chunk boundaries) is reused.
    assert dw.chunk_reusable(_cached(), "Charon", 0, 120_000) is True


def test_chunk_reusable_different_voice_resynth():
    # A cached chunk recorded under ANOTHER voice must NOT be reused — the whole
    # dub must speak in one voice, so it is re-synthesized with the new one.
    assert dw.chunk_reusable(_cached(voice="Kore"), "Charon", 0, 120_000) is False


def test_chunk_reusable_missing_voice_resynth():
    # A legacy chunk (pre-round-C) has no `voice` key → not reusable.
    assert dw.chunk_reusable({"index": 0}, "Charon", 0, 120_000) is False


def test_chunk_reusable_other_bounds_resynth():
    # #184 round E: the chunk plan changed (an 8-min session ceiling became 2 min),
    # so `chunk_0.json` on disk covers 0–480 s while slot 0 now covers 0–120 s. Same
    # voice, other boundaries → NOT reusable, or the final mix would lay the old
    # 8-min audio over the new 2-min chunks that follow it.
    old = _cached(start_ms=0, end_ms=480_000)
    assert dw.chunk_reusable(old, "Charon", 0, 120_000) is False
    assert (
        dw.chunk_reusable(
            _cached(start_ms=120_000, end_ms=240_000), "Charon", 0, 120_000
        )
        is False
    )


def test_chunk_reusable_legacy_meta_without_bounds_resynth():
    # A pre-round-E chunk recorded no boundaries → not reusable (never guess).
    assert (
        dw.chunk_reusable({"index": 0, "voice": "Charon"}, "Charon", 0, 120_000)
        is False
    )


# ── #184 round E2: baseline-relative, source-discounted voice guard ──────────────


def test_chunk_voice_drift_whole_chunk_high_vs_running_baseline():
    # The round-E whole-chunk blind spot: a chunk that is high THROUGHOUT has a
    # high chunk median, so round E saw 0 drift. Measured against the RUNNING
    # pinned-voice baseline (108) with a steady source below the source baseline
    # (131), every voiced window is drift.
    assert dw.chunk_voice_drift([220] * 10, [110] * 10, 108, 131) == (10, 10)


def test_chunk_voice_drift_source_following_is_not_drift():
    # The output is high AND the source is high at the same index (both above
    # their baselines) → the dub is following the source, not drifting.
    assert dw.chunk_voice_drift([220] * 5, [220] * 5, 108, 108) == (0, 5)


def test_chunk_voice_drift_seed_has_no_baseline():
    # No running baseline yet (the seed chunk) → nothing can drift.
    assert dw.chunk_voice_drift([220] * 5, [110] * 5, None, 131) == (0, 5)
    assert dw.chunk_voice_drift([], [], 108, 131) == (0, 0)


def test_chunk_voice_drift_aligns_mismatched_window_counts():
    # The input has fewer windows (tempo compressed it); a high output stretch
    # over a steady input still counts as drift after fraction-alignment.
    drifted, voiced = dw.chunk_voice_drift([100] * 7 + [220] * 3, [110] * 5, 108, 110)
    assert (drifted, voiced) == (3, 10)


def test_chunk_is_drifted_min_windows_and_fraction():
    # Round E2 trigger: >= 2 true-drift windows OR fraction > 0.05 (the file gate).
    assert dw._chunk_is_drifted(1, 24) is False  # 1 window, 1/24 = 0.042 < 0.05
    assert dw._chunk_is_drifted(2, 24) is True  # >= 2 windows floor
    assert dw._chunk_is_drifted(1, 15) is True  # 1/15 = 0.067 > 0.05
    assert dw._chunk_is_drifted(1, 20) is False  # 1/20 = 0.05 exactly, not > 0.05
    assert dw._chunk_is_drifted(0, 0) is False
    assert dw._chunk_is_drifted(0, 8) is False


def test_voice_drift_frac_equals_the_file_gate():
    # The chunk trigger's fraction is the SAME 0.05 the file check fails a file at,
    # so the guard can never sit laxer than the gate it protects (round E's 0.20
    # was 4x too lax — chunk 5 at 18 % passed the guard while failing the file).
    assert dw.VOICE_DRIFT_FRAC == dvc.MAX_HIGH_BAND_FRACTION


def test_chunk_voice_drift_shares_the_file_definition():
    # The guard delegates to the ONE shared drift definition (dub_voice_check),
    # so the guard and the file check measure the same thing.
    assert dw.chunk_voice_drift([220] * 4, [110] * 4, 108, 131) == dvc.drift_windows(
        [220] * 4, [110] * 4, 108, 131
    )


def test_voice_f0_band_charon_contains_measured_dub_median():
    # The measured band for Charon must contain 108 Hz — the measured median of
    # video 344's Charon dub (the acceptance case).
    lo, hi = dw.VOICE_F0_BAND["Charon"]
    assert lo <= 108 <= hi
    # Every catalogue voice has a (lo, hi) with lo < hi.
    for voice, (blo, bhi) in dw.VOICE_F0_BAND.items():
        assert blo < bhi, voice


def test_seed_median_ok_band_check():
    # In-band seed → True, out-of-band → False, unknown voice → None (no check).
    assert dw.seed_median_ok(108, "Charon") is True
    assert dw.seed_median_ok(200, "Charon") is False  # a female-band render
    assert dw.seed_median_ok(50, "Charon") is False  # sub-bass
    assert dw.seed_median_ok(108, "NoSuchVoice") is None


def test_baseline_from_meta_rebuilds_from_persisted_medians():
    # A resumed run rebuilds the running baselines from the persisted medians.
    meta = {
        "voice_band_ok": True,
        "voice_medians": [108, 110],
        "voice_in_medians": [130],
    }
    assert dw.baseline_from_meta(meta) == ([108, 110], [130])
    # A still-drifted chunk (voice_band_ok False) contributes nothing.
    drifted = {
        "voice_band_ok": False,
        "voice_medians": [220],
        "voice_in_medians": [110],
    }
    assert dw.baseline_from_meta(drifted) == ([], [])
    # A guard-skipped chunk (None) with no medians contributes nothing.
    assert dw.baseline_from_meta({"voice_band_ok": None}) == ([], [])
    # A legacy chunk (no keys at all) contributes nothing.
    assert dw.baseline_from_meta({"index": 0}) == ([], [])


def test_voice_band_guard_degrades_when_the_scan_fails(tmp_path):
    # Best-effort: the voice-band guard must NEVER fail the dub it decorates. A
    # scan failure (here a missing output WAV → a real wave error) is caught: the
    # guard returns voice_band_ok=None with no drift, empty medians (no baseline
    # update), leaves the transcripts untouched, and does not re-synthesize.
    missing = os.path.join(tmp_path, "gone.wav")
    ok, drifted, voiced, out_meds, in_meds, en, sk, sk_timed = (
        dw._apply_voice_band_guard(
            0,
            b"",
            1.0,
            str(tmp_path),
            "Charon",
            missing,
            None,
            None,
            "EN",
            "SK",
            [{"t_ms": 1, "text": "x"}],
        )
    )
    assert ok is None
    assert (drifted, voiced) == (0, 0)
    assert (out_meds, in_meds) == ([], [])
    assert (en, sk, sk_timed) == ("EN", "SK", [{"t_ms": 1, "text": "x"}])


def test_score_candidate_baseline_vs_seed():
    # With a running baseline: window drift against it (a whole-chunk-high take).
    assert dw._score_candidate([220] * 4, [110] * 4, 108, 131, "Charon") == (4, 4, True)
    # A clean take against the baseline is not drifted.
    assert dw._score_candidate([108] * 4, [110] * 4, 108, 131, "Charon") == (
        0,
        4,
        False,
    )
    # Seed take (no baseline): in-band median → not drifted.
    assert dw._score_candidate([108] * 4, [110] * 4, None, None, "Charon") == (
        0,
        4,
        False,
    )
    # Seed take: out-of-band median (a female-band render under a male pin) → the
    # whole seed scores as drifted so an in-band re-synth is kept over it.
    assert dw._score_candidate([200] * 4, [110] * 4, None, None, "Charon") == (
        4,
        4,
        True,
    )
    # Seed take with an unknown voice → no band check, not drifted.
    assert dw._score_candidate([300] * 4, [110] * 4, None, None, "Nope") == (
        0,
        4,
        False,
    )


def test_apply_voice_band_guard_keeps_clean_resynth(tmp_path, monkeypatch):
    # The core round-E2 control flow: a drifted chunk against a running baseline is
    # re-synthesized and the CLEANER candidate is kept (its transcripts + medians),
    # so the chunk ships accepted (voice_band_ok True) and feeds the baselines.
    work = str(tmp_path)
    wav_path = os.path.join(work, "chunk_0.wav")
    open(wav_path, "w").close()
    in_steady = [110] * 10

    def fake_wav_medians(path):
        # The original take is high throughout (drifted); the candidate is clean.
        return [108] * 10 if path.endswith(".cand.wav") else [220] * 10

    def fake_render(pcm, pace, work_dir, voice, raw_wav, out_wav):
        open(out_wav, "w").close()  # create the candidate file for os.replace
        return "EN2", "SK2", [{"t_ms": 2, "text": "y"}]

    monkeypatch.setattr(dw, "_pcm_window_medians", lambda pcm, sr: in_steady)
    monkeypatch.setattr(dw, "_wav_window_medians", fake_wav_medians)
    monkeypatch.setattr(dw, "_render_and_trim", fake_render)

    ok, drifted, voiced, out_meds, in_meds, en, sk, sk_timed = (
        dw._apply_voice_band_guard(
            0, b"pcm", 1.0, work, "Charon", wav_path, 108, 131, "EN", "SK", [{"t": 1}]
        )
    )
    assert ok is True  # the clean re-synth candidate was kept
    assert (drifted, voiced) == (0, 10)
    assert out_meds == [108] * 10 and in_meds == in_steady
    assert (en, sk, sk_timed) == ("EN2", "SK2", [{"t_ms": 2, "text": "y"}])


def test_apply_voice_band_guard_ships_still_drifted(tmp_path, monkeypatch):
    # When every re-synth is still drifted, the chunk ships voice_band_ok=False
    # after VOICE_RESYNTH_ATTEMPTS attempts (never fails the dub) and does NOT feed
    # the baselines (baseline_from_meta excludes it).
    work = str(tmp_path)
    wav_path = os.path.join(work, "chunk_0.wav")
    open(wav_path, "w").close()
    calls = {"n": 0}

    def fake_render(pcm, pace, work_dir, voice, raw_wav, out_wav):
        calls["n"] += 1
        open(out_wav, "w").close()
        return "EN2", "SK2", [{"t_ms": 2, "text": "y"}]

    monkeypatch.setattr(dw, "_pcm_window_medians", lambda pcm, sr: [110] * 10)
    monkeypatch.setattr(dw, "_wav_window_medians", lambda path: [220] * 10)
    monkeypatch.setattr(dw, "_render_and_trim", fake_render)

    ok, drifted, voiced, out_meds, in_meds, en, sk, sk_timed = (
        dw._apply_voice_band_guard(
            0, b"pcm", 1.0, work, "Charon", wav_path, 108, 131, "EN", "SK", [{"t": 1}]
        )
    )
    assert ok is False
    assert (drifted, voiced) == (10, 10)
    assert calls["n"] == dw.VOICE_RESYNTH_ATTEMPTS  # exactly 2 re-synth attempts
    # The original transcripts are kept (no candidate improved).
    assert (en, sk) == ("EN", "SK")
    # A still-drifted chunk contributes nothing to the baselines.
    meta = {"voice_band_ok": ok, "voice_medians": out_meds, "voice_in_medians": in_meds}
    assert dw.baseline_from_meta(meta) == ([], [])
