#!/usr/bin/env python3
"""dub_worker.py — Slovak dub synthesis via Gemini Live Translate (#183 D4).

`live-translate` streams the ORIGINAL audio of a video into
`gemini-3.5-live-translate-preview` (audio->audio, owner ruling #174), one Live
session per chunk, and writes the Slovak dub on the VIDEO timeline as the `--out`
FLAC (48 kHz stereo, so it matches the stem format the playback `StemMixReader`
mixes), plus an EN/SK transcripts JSON for D3.

The chunk plan (`--chunk-plan`, a JSON list of `{start_ms,end_ms}`) is computed by
the Rust worker from ffmpeg `silencedetect`; this child just executes it. Each
chunk is resumable (`<work-dir>/chunk_N.wav` + `.json` are reused on a re-run) and
the child heartbeats into the work dir so the Rust stall-timeout never kills a
healthy real-time stream. The Gemini key comes ONLY from `GEMINI_API_KEY` (env),
never argv/log.

Heavy imports (`google.genai`) are lazy so this module imports under CI/ruff and
the pytest helpers below run without the SDK.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import wave
from statistics import median

# #184 round F: the loudness rules ship next to this script (the Rust worker
# materialises it into the same tools dir, which is on sys.path for the child).
import dub_loudness as dl

INPUT_SR = 16000
OUTPUT_SR = 24000
FINAL_SR = 48000
CHUNK_BYTES = 3200  # 100 ms @ 16 kHz s16le mono
MAX_TEMPO = 1.08  # mirrors dabing::chunk_plan::MAX_TEMPO
MODEL = "gemini-3.5-live-translate-preview"
TARGET_LANG = "sk"
DRAIN_S = 27.0  # keep receiving this long after audio_stream_end (silence trail)
HEARTBEAT_EVERY_S = 5.0
DEFAULT_VOICE = "Charon"  # #184 round C: a male, matter-of-fact catalogue voice

# #184 round E2: per-chunk voice-band guard tuning.
VOICE_WINDOW_S = 5.0  # the f0-median window for the drift scan
VOICE_ST_ABOVE = 6.0  # semitones above the RUNNING baseline that count as "high"
# A chunk needs re-synthesis when it has >= VOICE_DRIFT_MIN_WINDOWS true-drift
# windows OR a true-drift FRACTION over VOICE_DRIFT_FRAC. VOICE_DRIFT_FRAC is the
# SAME 0.05 the file gate (`dub_voice_check.MAX_HIGH_BAND_FRACTION`) fails a file
# at — a unit test pins the equality so the chunk trigger can never sit laxer than
# the gate it protects (round E's bug: the trigger was 0.20, 4x the gate, so a
# chunk at 4/22 = 18 % passed the guard while failing the file).
VOICE_DRIFT_MIN_WINDOWS = 2
VOICE_DRIFT_FRAC = 0.05
VOICE_RESYNTH_ATTEMPTS = 2  # up to N re-synths (new session, same pin) per chunk

# #184 round E2: the SEED chunk (chunk 0, no running baseline yet) is checked
# against the pinned voice's EXPECTED f0 band — a coarse "chunk 0 is roughly the
# right voice" gate so a wrong seed does not poison the running baseline for every
# later chunk. Measured 2026-09-22 by `eval/dubbing/voice_band_measure.py` on the
# 120-s seg.wav with the autocorrelation estimator (`use_librosa=False`, the SAME
# path the runtime seed median uses — librosa/pyin would be a different unit), then
# WIDENED to `median × [0.80, 1.55]` so a legit content-driven shift still seeds
# (e.g. video 344's Charon dub median ~108 Hz) while a clear octave jump (×2) is
# rejected. Autocorrelation collapses octaves (all six voices measure ~87–94 Hz),
# so this is a COARSE seed gate, NOT a fine voice discriminator — the
# baseline-relative window guard + the on-box `--source` file gate (pyin) are the
# real drift detectors. An unknown voice → no seed check (seed as-is).
#                        seg p10–p90 / median (Hz, autocorrelation)
VOICE_F0_BAND = {
    "Charon": (73, 142),  # 84.9–100.8 / 91.3  (contains 344's ~108 Hz dub)
    "Orus": (75, 146),  # 90.3–103.0 / 94.1
    "Puck": (74, 144),  # 89.0–99.5 / 92.8
    "Kore": (72, 139),  # 81.3–105.3 / 89.9
    "Aoede": (70, 135),  # 81.3–98.2 / 87.3
    "Leda": (72, 139),  # 83.7–100.7 / 89.9
}


# ── pure helpers (unit-tested; no I/O, no heavy imports) ─────────────────────────


def pcm_len_ms(byte_len: int, sample_rate: int) -> int:
    """Duration in ms of `byte_len` bytes of s16le mono PCM at `sample_rate`."""
    if sample_rate <= 0:
        return 0
    return int(round(byte_len / 2 / sample_rate * 1000))


def pcm_pos_to_ms(byte_pos: int, sample_rate: int) -> int:
    """Timeline position (ms) of a byte offset into s16le mono PCM — used to stamp
    the SK transcript deltas by the output-audio position at arrival."""
    return pcm_len_ms(byte_pos, sample_rate)


def placement(
    chunk_start_ms: int, chunk_len_ms: int, out_len_ms: int, next_start_ms: int | None
) -> tuple[int, float]:
    """Where a chunk's output lands + its atempo factor. Mirrors the Rust
    `dabing::chunk_plan::placement_for`: start at the chunk start; speed up only to
    fit before `next_start_ms`, never faster than MAX_TEMPO, never slower than 1.0;
    the last chunk (`next_start_ms is None`) is never sped up."""
    tempo = 1.0
    if next_start_ms is not None and next_start_ms > chunk_start_ms:
        avail = next_start_ms - chunk_start_ms
        if avail > 0 and out_len_ms > avail:
            tempo = min(MAX_TEMPO, max(1.0, out_len_ms / avail))
    return chunk_start_ms, tempo


def slice_resample_args(
    ffmpeg: str, audio: str, start_ms: int, end_ms: int, out_pcm: str
) -> list[str]:
    """ffmpeg argv to cut `[start_ms,end_ms)` of `audio` and write 16 kHz mono
    s16le PCM to `out_pcm` (the Live-API input format). Output trimming (`-ss/-to`
    after `-i`) for sample-accurate chunk bounds."""
    return [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-y",
        "-i",
        audio,
        "-ss",
        f"{start_ms / 1000.0:.3f}",
        "-to",
        f"{end_ms / 1000.0:.3f}",
        "-ac",
        "1",
        "-ar",
        str(INPUT_SR),
        "-f",
        "s16le",
        "-acodec",
        "pcm_s16le",
        out_pcm,
    ]


def trim_silence_af(threshold_db: int = -45, min_silence_s: float = 0.2) -> str:
    """The ffmpeg `-af` that strips trailing silence (reverse, remove leading
    silence, reverse) — the Live session streams silence after `audio_stream_end`.
    Same filter as the reference `gemini_live_translate.trim_af`."""
    return (
        f"areverse,silenceremove=start_periods=1:start_threshold={threshold_db}dB:"
        f"start_duration={min_silence_s},areverse"
    )


def build_mix_filter(placements: list[tuple[float, int]]) -> str:
    """Build the ffmpeg `filter_complex` that places each chunk WAV (input `i`) on
    the timeline: `atempo` then `adelay` to its `at_ms`, then `amix` all, resample
    to 48 kHz. `placements[i] = (tempo, at_ms)`. Pure — unit-tested. `-ac 2` on the
    output upmixes the mono mix to stereo."""
    parts = []
    labels = []
    for i, (tempo, at_ms) in enumerate(placements):
        # atempo must be >= 1.0 here; adelay delays the (mono) stream to at_ms.
        parts.append(f"[{i}:a]atempo={tempo:.4f},adelay={int(at_ms)}:all=1[a{i}]")
        labels.append(f"[a{i}]")
    n = len(placements)
    mix = "".join(labels) + f"amix=inputs={n}:normalize=0,aresample={FINAL_SR}[mix]"
    return ";".join(parts + [mix])


def drain_deadline_s(input_pcm_bytes: int, drain_s: float = DRAIN_S) -> float:
    """How long to keep the receive stream open: the input's real-time duration
    plus a drain window for the model to finish translating."""
    input_s = input_pcm_bytes / 2 / INPUT_SR
    return round(input_s + drain_s, 2)


def chunk_reusable(meta: dict, voice: str, start_ms: int, end_ms: int) -> bool:
    """#184 round C + E + E2: a resumed chunk (`chunk_N.json`) may be reused ONLY
    if it was synthesized with the SAME voice as the one now requested, covers the
    SAME `[start_ms, end_ms)` slice of the source, AND carries the voice-guard
    record (`voice_medians`, non-empty). A chunk recorded under a different voice,
    a legacy chunk with no `voice` / boundary keys, a chunk from an OLDER chunk
    plan (the session ceiling changed, so slot N now covers a different slice), or
    a chunk that was never voice-guarded (pre-E2, or the guard was skipped) is NOT
    reusable — it is re-synthesized: reusing an unguarded chunk on a re-dub would
    silently keep exactly the drifted audio the re-dub was requested to fix. Pure —
    unit-tested."""
    return (
        meta.get("voice") == voice
        and meta.get("chunk_start_ms") == start_ms
        and meta.get("chunk_end_ms") == end_ms
        and bool(meta.get("voice_medians"))
    )


def chunk_voice_drift(
    out_medians: list,
    in_medians: list,
    out_baseline: float | None,
    in_baseline: float | None,
    st_above: float = VOICE_ST_ABOVE,
) -> tuple[int, int]:
    """(#184 round E2) Count the (DRIFTED, VOICED) 5-s windows of one synthesized
    chunk against the RUNNING baselines — the pinned-voice OUTPUT baseline (median
    of the voiced output windows of the chunks accepted so far) and the SOURCE
    INPUT baseline (running median of the input windows) — NOT the chunk's own
    medians. This is the round-E2 fix for the whole-chunk blind spot: a chunk that
    is high THROUGHOUT (round E's chunk 10) has a high chunk median, so round E saw
    0 drift; measured against the running voice baseline those windows are high and
    counted, unless the aligned SOURCE window is also high above the source
    baseline (source-following, discounted). Delegates to the ONE shared drift
    definition in `dub_voice_check.drift_windows` so the guard and the file check
    measure the same thing. `out_baseline` None/<=0 (the seed chunk, no baseline
    yet) → (0, voiced). Pure — unit-tested."""
    import dub_voice_check as dvc

    return dvc.drift_windows(
        out_medians, in_medians, out_baseline, in_baseline, st_above
    )


def _chunk_is_drifted(drifted: int, voiced: int) -> bool:
    """(#184 round E2) A chunk needs re-synthesis when it has at least
    `VOICE_DRIFT_MIN_WINDOWS` true-drift windows OR a true-drift FRACTION over
    `VOICE_DRIFT_FRAC` (== the file gate's `MAX_HIGH_BAND_FRACTION`). The absolute
    floor catches a couple of drifted windows a small chunk's fraction would miss;
    the fraction matches the file gate exactly (round E's 0.20 was 4x too lax).
    Pure."""
    if voiced <= 0:
        return False
    return drifted >= VOICE_DRIFT_MIN_WINDOWS or drifted / voiced > VOICE_DRIFT_FRAC


def voice_band_for(voice: str) -> tuple | None:
    """(#184 round E2) The `(lo, hi)` expected f0 band for `voice`, or None when
    the voice is not in the measured `VOICE_F0_BAND` table (an unknown voice gets
    no seed check — it seeds the baseline as-is). Pure."""
    return VOICE_F0_BAND.get(voice)


def seed_median_ok(median_hz: float, voice: str) -> bool | None:
    """(#184 round E2) Is the SEED chunk's voiced f0 median inside `voice`'s
    expected band? True/False when the voice is known, None when it is not (no
    check). A seed outside its band is a wrong-voice seed that would poison the
    running baseline for every later chunk, so it is treated as drifted. Pure."""
    band = voice_band_for(voice)
    if band is None:
        return None
    lo, hi = band
    return lo <= median_hz <= hi


def baseline_from_meta(meta: dict) -> tuple[list, list]:
    """(#184 round E2) The `(output_medians, input_medians)` a chunk contributes to
    the running pinned-voice / source baselines, read from its persisted
    `chunk_N.json`. Only an ACCEPTED chunk contributes: a chunk that shipped still
    drifted (`voice_band_ok is False`) or a legacy chunk with no persisted medians
    contributes nothing, so the baseline is never poisoned and a resumed run
    rebuilds the same baseline from the reused chunks. Pure — unit-tested."""
    if meta.get("voice_band_ok") is False:
        return ([], [])
    return (
        list(meta.get("voice_medians") or []),
        list(meta.get("voice_in_medians") or []),
    )


def build_transcripts(results: list[dict]) -> dict:
    """Assemble the EN/SK transcripts JSON (the D3 #182 subtitle source) from the
    per-chunk results. Pure — unit-tested. Each chunk carries its video-timeline
    placement so the Rust subtitle builder maps chunk-local SK positions onto the
    video timeline WITHOUT a second transcription pass; the placement is read from
    the SAME per-chunk result the mix uses (`_process_chunk`), so cached/resumed
    chunks are included with no re-synthesis and no extra API calls."""
    return {
        "engine": "gemini-live-translate",
        "target_lang": TARGET_LANG,
        "chunks": [
            {
                "index": r["index"],
                "start_ms": r["chunk_start_ms"],
                "end_ms": r["chunk_end_ms"],
                # Video-timeline placement (D3 #182): `at_ms` = where the chunk's
                # output lands, `tempo` = the atempo the mix applied. Read from the
                # SAME per-chunk result the mix uses, so cached/resumed chunks are
                # included with no re-synthesis.
                "at_ms": r["at_ms"],
                "tempo": r["tempo"],
                "en": r["transcript_en"],
                "sk": r["transcript_sk"],
                "sk_timed": r["sk_timed"],
            }
            for r in results
        ],
    }


# ── I/O helpers ─────────────────────────────────────────────────────────────────


def _log(msg: str) -> None:
    """Progress goes to stderr (stdout is reserved for the summary JSON)."""
    sys.stderr.write(msg.rstrip() + "\n")
    sys.stderr.flush()


def _run(args: list[str]) -> None:
    r = subprocess.run(args, capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(
            f"command failed ({args[0]}, rc={r.returncode}): {r.stderr[-800:]}"
        )


def _run_stderr(args: list[str]) -> str:
    """Run `args`; return its stderr (ffmpeg prints the loudnorm JSON there).
    Fails loudly on a non-zero exit, like `_run`."""
    r = subprocess.run(
        args, capture_output=True, text=True, encoding="utf-8", errors="replace"
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"command failed ({args[0]}, rc={r.returncode}): {r.stderr[-800:]}"
        )
    return r.stderr


def _ffmpeg() -> str:
    return os.environ.get("DUB_FFMPEG", "ffmpeg")


def _write_wav_from_pcm(pcm: bytes, sr: int, path: str) -> None:
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(pcm)


def _heartbeat(work_dir: str) -> None:
    try:
        with open(os.path.join(work_dir, "heartbeat"), "w") as f:
            f.write(str(time.time()))
    except OSError:
        pass


def _pcm_window_medians(pcm: bytes, sr: int) -> list:
    """5-s-window f0 medians of 16-bit s16le mono PCM bytes at `sr`, via numpy +
    the dependency-free autocorrelation path of `dub_voice_check` (no soundfile,
    no librosa). `[]` for empty/degenerate input. Used for the in-memory INPUT
    PCM and the output WAV samples alike."""
    import numpy as np

    import dub_voice_check as dvc

    if not pcm or sr <= 0:
        return []
    x = np.frombuffer(pcm, dtype="<i2").astype("float32") / 32768.0
    return dvc.window_medians(x, sr, VOICE_WINDOW_S, use_librosa=False)


def _wav_window_medians(wav_path: str) -> list:
    """5-s-window f0 medians of a 16-bit mono WAV (the trimmed dub output), read
    via `wave` (no soundfile). `[]` when it is not 16-bit mono."""
    with wave.open(wav_path, "rb") as w:
        sr = w.getframerate()
        if w.getsampwidth() != 2 or w.getnchannels() != 1 or sr <= 0:
            return []
        raw = w.readframes(w.getnframes())
    return _pcm_window_medians(raw, sr)


def _render_and_trim(
    pcm: bytes, pace: float, work_dir: str, voice: str, raw_wav: str, out_wav: str
) -> tuple[str, str, list]:
    """Translate one chunk's `pcm` to a trailing-silence-trimmed WAV at `out_wav`;
    return `(transcript_en, transcript_sk, sk_timed)`. Writes then removes the raw
    24 kHz WAV. Shared by the first synthesis and the round-E re-synthesis."""
    out_pcm, en, sk, sk_timed = _translate_pcm(pcm, pace, work_dir, voice)
    _write_wav_from_pcm(out_pcm, OUTPUT_SR, raw_wav)
    _run(
        [
            _ffmpeg(),
            "-hide_banner",
            "-nostdin",
            "-y",
            "-i",
            raw_wav,
            "-af",
            trim_silence_af(),
            out_wav,
        ]
    )
    try:
        os.remove(raw_wav)
    except OSError:
        pass
    return en, sk, sk_timed


def _score_candidate(
    out_medians: list,
    in_medians: list,
    out_baseline: float | None,
    in_baseline: float | None,
    voice: str,
) -> tuple[int, int, bool]:
    """(#184 round E2) Score one synthesized take: `(drifted, voiced, is_drifted)`.
    With a running baseline it is the window-drift against that baseline
    (`chunk_voice_drift` + `_chunk_is_drifted`). For the SEED take (no baseline
    yet) it is the band check: an in-band / unknown-voice seed scores `(0, voiced,
    False)`; an out-of-band seed scores `(voiced, voiced, True)` so a re-synth that
    lands in band (0 drifted) is kept over it. Pure-ish — calls chunk_voice_drift."""
    voiced_meds = [m for m in out_medians if m > 0]
    voiced = len(voiced_meds)
    if out_baseline and out_baseline > 0:
        drifted, voiced = chunk_voice_drift(
            out_medians, in_medians, out_baseline, in_baseline
        )
        return drifted, voiced, _chunk_is_drifted(drifted, voiced)
    # Seed take (no running baseline yet): check the median against the voice band.
    med = median(voiced_meds) if voiced_meds else 0.0
    if seed_median_ok(med, voice) is False:
        return voiced, voiced, True  # whole seed is the wrong voice
    return 0, voiced, False


def _apply_voice_band_guard(
    idx: int,
    pcm: bytes,
    pace: float,
    work_dir: str,
    voice: str,
    wav_path: str,
    out_baseline: float | None,
    in_baseline: float | None,
    en: str,
    sk: str,
    sk_timed: list,
) -> tuple:
    """#184 round E2: scan the just-synthesized chunk `wav_path` against the
    RUNNING pinned-voice output baseline + source input baseline (the seed chunk,
    with no baseline yet, against the voice's expected band) and re-synthesize up
    to `VOICE_RESYNTH_ATTEMPTS` times (new session, same pin) when it drifts,
    keeping the take with the fewest true-drift windows. Returns
    `(voice_band_ok, drifted, voiced, out_medians, in_medians, en, sk, sk_timed)`
    of the kept take — the medians so the caller can feed the running baselines
    (only when accepted; a still-drifted chunk must NOT poison them).

    BEST-EFFORT: this decorates the dub, it must never FAIL it. Any scan failure
    (numpy/dub_voice_check import, a truncated/unreadable output wav) is caught,
    logged, and falls through with `voice_band_ok=None`, empty medians (no baseline
    update) and no re-synthesis — the dub is shipped as-is (round-E rule)."""
    try:
        in_medians = _pcm_window_medians(pcm, INPUT_SR)
        out_medians = _wav_window_medians(wav_path)
        drifted, voiced, is_drifted = _score_candidate(
            out_medians, in_medians, out_baseline, in_baseline, voice
        )
        attempt = 0
        while is_drifted and attempt < VOICE_RESYNTH_ATTEMPTS:
            attempt += 1
            _log(
                f"chunk {idx}: voice drift {drifted}/{voiced} windows -> "
                f"re-synth {attempt}/{VOICE_RESYNTH_ATTEMPTS}"
            )
            cand_raw = os.path.join(work_dir, f"chunk_{idx}.cand.raw.wav")
            cand_wav = os.path.join(work_dir, f"chunk_{idx}.cand.wav")
            en2, sk2, sk_timed2 = _render_and_trim(
                pcm, pace, work_dir, voice, cand_raw, cand_wav
            )
            out2 = _wav_window_medians(cand_wav)
            drifted2, voiced2, is_drifted2 = _score_candidate(
                out2, in_medians, out_baseline, in_baseline, voice
            )
            if drifted2 < drifted:
                os.replace(cand_wav, wav_path)
                en, sk, sk_timed = en2, sk2, sk_timed2
                out_medians, drifted, voiced, is_drifted = (
                    out2,
                    drifted2,
                    voiced2,
                    is_drifted2,
                )
                _log(
                    f"chunk {idx}: kept re-synth candidate ({drifted}/{voiced} drifted)"
                )
            else:
                try:
                    os.remove(cand_wav)
                except OSError:
                    pass
                _log(
                    f"chunk {idx}: kept previous take ({drifted}/{voiced} drifted; "
                    f"candidate {drifted2}/{voiced2})"
                )
        voice_band_ok = not is_drifted
        # The medians are persisted for transparency + baseline rebuild on resume;
        # `baseline_from_meta` excludes a still-drifted chunk (voice_band_ok False)
        # from the running baselines, so a drifted chunk never poisons them.
        return (
            voice_band_ok,
            drifted,
            voiced,
            out_medians,
            in_medians,
            en,
            sk,
            sk_timed,
        )
    except Exception as e:  # best-effort guard — a scan failure must NOT fail the dub
        _log(f"chunk {idx}: voice-band guard skipped ({type(e).__name__}: {e})")
        return (None, 0, 0, [], [], en, sk, sk_timed)


# ── the Live translation of one chunk ───────────────────────────────────────────


def _translate_pcm(
    pcm: bytes, pace: float, work_dir: str, voice: str
) -> tuple[bytes, str, str, list]:
    """Stream 16 kHz mono s16le `pcm` into the Live API; return
    (out_pcm_24k, transcript_en, transcript_sk, sk_timed). `sk_timed` is a coarse
    list of `{t_ms, text}` stamped by the output-audio position at arrival — the D3
    subtitle seed. `voice` PINS the output voice via `speech_config` so a video's
    dub stays one voice (#184 round C — the probe confirmed the translate model
    accepts `speech_config` and renders it stably). Heartbeats into `work_dir` so
    the Rust stall timeout sees a live stream even mid-chunk."""
    import asyncio

    import google.genai as genai
    from google.genai import types

    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise RuntimeError("GEMINI_API_KEY not set")

    async def run() -> tuple[bytes, str, str, list]:
        client = genai.Client(api_key=key, http_options={"api_version": "v1alpha"})
        config = types.LiveConnectConfig(
            response_modalities=["AUDIO"],
            input_audio_transcription=types.AudioTranscriptionConfig(),
            output_audio_transcription=types.AudioTranscriptionConfig(),
            translation_config=types.TranslationConfig(
                target_language_code=TARGET_LANG,
                echo_target_language=True,
            ),
            speech_config=types.SpeechConfig(
                voice_config=types.VoiceConfig(
                    prebuilt_voice_config=types.PrebuiltVoiceConfig(voice_name=voice)
                )
            ),
        )
        out = bytearray()
        en_parts: list[str] = []
        sk_parts: list[str] = []
        sk_timed: list = []
        last_hb = time.monotonic()
        t0 = time.monotonic()
        deadline = t0 + drain_deadline_s(len(pcm))
        # Sleep between 100 ms chunks, divided by the pacing factor (2x = faster).
        chunk_sleep = 0.1 / max(0.5, pace)

        async with client.aio.live.connect(model=MODEL, config=config) as session:

            async def send() -> None:
                for i in range(0, len(pcm), CHUNK_BYTES):
                    await session.send_realtime_input(
                        audio=types.Blob(
                            data=bytes(pcm[i : i + CHUNK_BYTES]),
                            mime_type=f"audio/pcm;rate={INPUT_SR}",
                        )
                    )
                    await asyncio.sleep(chunk_sleep)
                await session.send_realtime_input(audio_stream_end=True)

            send_task = asyncio.create_task(send())
            gen = session.receive().__aiter__()
            while time.monotonic() < deadline:
                if time.monotonic() - last_hb > HEARTBEAT_EVERY_S:
                    _heartbeat(work_dir)
                    last_hb = time.monotonic()
                try:
                    resp = await asyncio.wait_for(gen.__anext__(), timeout=5.0)
                except asyncio.TimeoutError:
                    continue
                except StopAsyncIteration:
                    break
                data = getattr(resp, "data", None)
                if data:
                    out.extend(data)
                sc = getattr(resp, "server_content", None)
                if sc is not None:
                    iat = getattr(sc, "input_transcription", None)
                    if iat is not None and getattr(iat, "text", None):
                        en_parts.append(iat.text)
                    oat = getattr(sc, "output_transcription", None)
                    if oat is not None and getattr(oat, "text", None):
                        sk_parts.append(oat.text)
                        sk_timed.append(
                            {
                                "t_ms": pcm_pos_to_ms(len(out), OUTPUT_SR),
                                "text": oat.text,
                            }
                        )
                    if getattr(sc, "turn_complete", False) and send_task.done():
                        break
            if not send_task.done():
                send_task.cancel()
                try:
                    await send_task
                except asyncio.CancelledError:
                    # airuleset:script-ok awaiting our own just-cancelled sender is
                    # the standard clean-teardown idiom; nothing to log.
                    pass
        return bytes(out), "".join(en_parts), "".join(sk_parts), sk_timed

    return asyncio.run(run())


def _process_chunk(
    idx: int,
    chunk: dict,
    next_start: int | None,
    audio: str,
    work_dir: str,
    pace: float,
    voice: str,
    out_baseline: float | None = None,
    in_baseline: float | None = None,
) -> dict:
    """Translate one chunk (resumable): returns the chunk result dict. Reuses an
    existing `chunk_N.json` + `chunk_N.wav` on a re-run ONLY when it was made with
    the SAME `voice` and the SAME chunk boundaries (#184 round C + E — a chunk
    recorded under another voice, a legacy chunk with no `voice`, or a chunk from
    an older chunk plan is re-synthesized so the whole dub is one voice laid at
    the right offsets). `out_baseline` / `in_baseline` (#184 round E2) are the
    running pinned-voice / source medians of the chunks accepted so far, passed to
    the voice-band guard; the seed chunk gets `None`."""
    start_ms = int(chunk["start_ms"])
    end_ms = int(chunk["end_ms"])
    result_path = os.path.join(work_dir, f"chunk_{idx}.json")
    wav_path = os.path.join(work_dir, f"chunk_{idx}.wav")
    if os.path.exists(result_path) and os.path.exists(wav_path):
        with open(result_path, encoding="utf-8") as f:
            meta = json.load(f)
        if chunk_reusable(meta, voice, start_ms, end_ms):
            _log(f"chunk {idx}: resume (already done, voice {voice})")
            return meta
        _log(
            f"chunk {idx}: re-synthesizing (voice {voice}, {start_ms}-{end_ms} ms; "
            f"cached {meta.get('voice')}, {meta.get('chunk_start_ms')}-{meta.get('chunk_end_ms')} ms)"
        )

    chunk_len = end_ms - start_ms
    _heartbeat(work_dir)

    # 1. Cut + resample the chunk to the Live-API input format.
    pcm_path = os.path.join(work_dir, f"chunk_{idx}.in.pcm")
    _run(slice_resample_args(_ffmpeg(), audio, start_ms, end_ms, pcm_path))
    with open(pcm_path, "rb") as f:
        pcm = f.read()

    # 2. Translate (audio->audio) and trim the trailing silence.
    raw_wav = os.path.join(work_dir, f"chunk_{idx}.raw.wav")
    en, sk, sk_timed = _render_and_trim(pcm, pace, work_dir, voice, raw_wav, wav_path)

    # #184 round E2: scan the synthesized chunk against the running pinned-voice /
    # source baselines (seed chunk against the voice band) and re-synthesize up to
    # 2 times if it drifts — best-effort (a scan failure never fails the dub,
    # `voice_band_ok` then comes back None). `out_medians`/`in_medians` feed the
    # running baselines (only when accepted; see `baseline_from_meta`).
    voice_band_ok, drifted, voiced, out_medians, in_medians, en, sk, sk_timed = (
        _apply_voice_band_guard(
            idx,
            pcm,
            pace,
            work_dir,
            voice,
            wav_path,
            out_baseline,
            in_baseline,
            en,
            sk,
            sk_timed,
        )
    )

    # 3. Measured output length + placement.
    out_len_ms = _wav_duration_ms(wav_path)
    _, tempo = placement(start_ms, chunk_len, out_len_ms, next_start)

    result = {
        "index": idx,
        "chunk_start_ms": start_ms,
        "chunk_end_ms": end_ms,
        "out_len_ms": out_len_ms,
        "next_start_ms": next_start,
        "tempo": round(tempo, 4),
        "at_ms": start_ms,
        # #184 round C: record the voice so a later re-run reuses this chunk only
        # when the requested voice is unchanged (see `chunk_reusable`).
        "voice": voice,
        # #184 round E2: the per-chunk voice-band verdict + counts, plus the
        # output/input 5-s medians so a resumed run rebuilds the running baselines
        # from the reused chunks (`baseline_from_meta`).
        "voice_band_ok": voice_band_ok,
        "voice_drifted_windows": drifted,
        "voice_windows": voiced,
        "voice_medians": out_medians,
        "voice_in_medians": in_medians,
        "transcript_en": en,
        "transcript_sk": sk,
        "sk_timed": sk_timed,
    }
    with open(result_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False)
    # Clean the big input PCM (keep chunk_N.wav + .json for resume/mix).
    try:
        os.remove(pcm_path)
    except OSError:
        pass
    _log(
        f"chunk {idx}: {chunk_len} ms in -> {out_len_ms} ms out, tempo {tempo:.3f}, "
        f"voice_band_ok={voice_band_ok} ({drifted}/{voiced})"
    )
    return result


def _wav_duration_ms(path: str) -> int:
    with wave.open(path, "rb") as w:
        frames = w.getnframes()
        rate = w.getframerate()
    if rate <= 0:
        return 0
    return int(round(frames / rate * 1000))


def _assemble_dub(
    audio: str,
    wavs: list[str],
    placements: list[tuple[float, int]],
    out: str,
    work_dir: str,
) -> dict:
    """#184 round F: assemble the dub on the video timeline, loudness-matched to
    the audio it translates (`audio`, the video's normalized original):

    1. measure `audio`'s integrated loudness → `dl.loudness_target` (clamped);
    2. analyse the assembled mix against that target (loudnorm pass 1, to null);
    3. write the 48 kHz stereo dub with the LINEAR second pass
       (`dl.build_loudnorm_second_pass`) to a PARTIAL file, promoted over `out`
       only once ffmpeg succeeded AND reported its loudness — a failed rebuild
       never destroys the previous good dub.

    Heartbeats before each full-length pass. Returns the loudness stats (also
    written, JSON-safe, to `<work_dir>/loudness.json` as box-side evidence). Any
    ffmpeg failure or unparseable measurement raises — the dub fails loudly
    rather than shipping at an unknown level."""
    ff = _ffmpeg()
    _heartbeat(work_dir)
    source = dl.parse_loudnorm_json(_run_stderr(dl.loudness_measure_args(ff, audio)))
    target = dl.loudness_target(source["input_i"])
    mix_filter = build_mix_filter(placements)
    analysis = dl.loudnorm_analysis_filter(target)
    _heartbeat(work_dir)
    mix = dl.parse_loudnorm_json(
        _run_stderr(dl.assembly_args(ff, wavs, mix_filter, analysis, None, FINAL_SR))
    )
    second = dl.build_loudnorm_second_pass(mix, target)
    part = dl.partial_out_path(out)
    _heartbeat(work_dir)
    try:
        applied = dl.parse_loudnorm_json(
            _run_stderr(dl.assembly_args(ff, wavs, mix_filter, second, part, FINAL_SR))
        )
        os.replace(part, out)
    except BaseException:
        try:
            os.remove(part)
        except OSError:
            pass
        raise
    stats = {
        "source_i": source["input_i"],
        "target_i": target,
        "mix_i": mix["input_i"],
        "output_i": applied.get("output_i"),
        "normalization_type": applied.get("normalization_type"),
    }
    if stats["normalization_type"] != "linear":
        _log(
            "dub loudness: WARNING loudnorm fell back to "
            f"{stats['normalization_type']} mode (the linear gain would breach "
            f"TP {dl.DUB_TRUE_PEAK} or the mix LRA exceeds {dl.DUB_LRA})"
        )
    with open(os.path.join(work_dir, "loudness.json"), "w", encoding="utf-8") as f:
        json.dump(dl.json_safe_stats(stats), f, allow_nan=False)
    _log(
        f"dub loudness: source {stats['source_i']:.2f} LUFS -> target "
        f"{target:.2f}; mix {stats['mix_i']:.2f} -> output {stats['output_i']} "
        f"({stats['normalization_type']})"
    )
    return stats


def cmd_live_translate(args: argparse.Namespace) -> None:
    os.makedirs(args.work_dir, exist_ok=True)
    with open(args.chunk_plan, encoding="utf-8") as f:
        chunks = json.load(f)
    if not chunks:
        raise RuntimeError("empty chunk plan")

    # #184 round E2: the running PINNED-VOICE (output) + SOURCE (input) baselines,
    # accumulated in chunk order from the voiced medians of the chunks ACCEPTED so
    # far. The first chunk seeds them (checked against the voice band); every later
    # chunk's guard measures drift against the median of what accumulated BEFORE it.
    accepted_out: list = []
    accepted_in: list = []
    results = []
    for i, chunk in enumerate(chunks):
        next_start = int(chunks[i + 1]["start_ms"]) if i + 1 < len(chunks) else None
        out_baseline = median(accepted_out) if accepted_out else None
        in_baseline = median(accepted_in) if accepted_in else None
        result = _process_chunk(
            i,
            chunk,
            next_start,
            args.audio,
            args.work_dir,
            args.pace,
            args.voice,
            out_baseline,
            in_baseline,
        )
        results.append(result)
        # Feed the running baselines with this chunk's medians — but only when it
        # was ACCEPTED (a still-drifted or legacy/guard-skipped chunk contributes
        # nothing, so it never poisons the pinned-voice baseline). Resume-safe: a
        # reused chunk contributes its persisted medians the same way.
        fed_out, fed_in = baseline_from_meta(result)
        accepted_out.extend(fed_out)
        accepted_in.extend(fed_in)

    # Assemble the dub on the video timeline: place each chunk at its at_ms with
    # its atempo, mix, 48 kHz stereo. #184 round F (owner verdict 2026-09-23,
    # #184 comment 5793796815 — "rovnaké pomery = rovnaká hlasitosť"): the mix is
    # normalized to the MEASURED loudness of the audio it translates (clamped
    # -24..-10 LUFS) with a two-pass LINEAR loudnorm — no longer a fixed
    # single-pass dynamic -16, which left the dub ~1.6 LU under the original.
    placements = [(float(r["tempo"]), int(r["at_ms"])) for r in results]
    wavs = [os.path.join(args.work_dir, f"chunk_{r['index']}.wav") for r in results]
    _assemble_dub(args.audio, wavs, placements, args.out, args.work_dir)

    # Transcripts JSON for D3 (EN + SK, per-chunk with timeline placement).
    transcripts = build_transcripts(results)
    with open(args.transcripts, "w", encoding="utf-8") as f:
        json.dump(transcripts, f, ensure_ascii=False)

    # The summary JSON is the ONLY thing on stdout (the Rust worker parses it).
    summary = {
        "out_path": args.out,
        "transcripts_path": args.transcripts,
        "chunks": [
            {
                "index": r["index"],
                "chunk_start_ms": r["chunk_start_ms"],
                "chunk_end_ms": r["chunk_end_ms"],
                "out_len_ms": r["out_len_ms"],
                "next_start_ms": r["next_start_ms"],
                "tempo": r["tempo"],
            }
            for r in results
        ],
    }
    sys.stdout.write(json.dumps(summary))
    sys.stdout.flush()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Slovak dub synthesis (Gemini Live Translate)"
    )
    sub = parser.add_subparsers(dest="cmd", required=True)
    lt = sub.add_parser("live-translate", help="translate a video's audio to a SK dub")
    lt.add_argument("--audio", required=True)
    lt.add_argument("--out", required=True)
    lt.add_argument("--transcripts", required=True)
    lt.add_argument("--chunk-plan", dest="chunk_plan", required=True)
    lt.add_argument("--work-dir", dest="work_dir", required=True)
    lt.add_argument("--pace", type=float, default=1.0)
    lt.add_argument("--voice", default=DEFAULT_VOICE)
    args = parser.parse_args()

    try:
        if args.cmd == "live-translate":
            cmd_live_translate(args)
        else:
            raise RuntimeError(f"unknown command: {args.cmd}")
    except Exception as e:  # loud failure: JSON error on stderr + non-zero exit.
        # Defensive: never let the key leak into the logged error tail, even if an
        # SDK exception embedded it.
        msg = str(e)
        key = os.environ.get("GEMINI_API_KEY")
        if key:
            msg = msg.replace(key, "<redacted>")
        sys.stderr.write(json.dumps({"error": msg}) + "\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
