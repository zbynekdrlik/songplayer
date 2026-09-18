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

INPUT_SR = 16000
OUTPUT_SR = 24000
FINAL_SR = 48000
CHUNK_BYTES = 3200  # 100 ms @ 16 kHz s16le mono
MAX_TEMPO = 1.08  # mirrors dabing::chunk_plan::MAX_TEMPO
MODEL = "gemini-3.5-live-translate-preview"
TARGET_LANG = "sk"
DRAIN_S = 27.0  # keep receiving this long after audio_stream_end (silence trail)
HEARTBEAT_EVERY_S = 5.0


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


def placement(chunk_start_ms: int, chunk_len_ms: int, out_len_ms: int,
              next_start_ms: int | None) -> tuple[int, float]:
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


def slice_resample_args(ffmpeg: str, audio: str, start_ms: int, end_ms: int,
                        out_pcm: str) -> list[str]:
    """ffmpeg argv to cut `[start_ms,end_ms)` of `audio` and write 16 kHz mono
    s16le PCM to `out_pcm` (the Live-API input format). Output trimming (`-ss/-to`
    after `-i`) for sample-accurate chunk bounds."""
    return [
        ffmpeg, "-hide_banner", "-nostdin", "-y",
        "-i", audio,
        "-ss", f"{start_ms / 1000.0:.3f}",
        "-to", f"{end_ms / 1000.0:.3f}",
        "-ac", "1", "-ar", str(INPUT_SR),
        "-f", "s16le", "-acodec", "pcm_s16le",
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
        parts.append(
            f"[{i}:a]atempo={tempo:.4f},adelay={int(at_ms)}:all=1[a{i}]"
        )
        labels.append(f"[a{i}]")
    n = len(placements)
    mix = "".join(labels) + f"amix=inputs={n}:normalize=0,aresample={FINAL_SR}[mix]"
    return ";".join(parts + [mix])


def drain_deadline_s(input_pcm_bytes: int, drain_s: float = DRAIN_S) -> float:
    """How long to keep the receive stream open: the input's real-time duration
    plus a drain window for the model to finish translating."""
    input_s = input_pcm_bytes / 2 / INPUT_SR
    return round(input_s + drain_s, 2)


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


# ── the Live translation of one chunk ───────────────────────────────────────────


def _translate_pcm(pcm: bytes, pace: float, work_dir: str) -> tuple[bytes, str, str, list]:
    """Stream 16 kHz mono s16le `pcm` into the Live API; return
    (out_pcm_24k, transcript_en, transcript_sk, sk_timed). `sk_timed` is a coarse
    list of `{t_ms, text}` stamped by the output-audio position at arrival — the D3
    subtitle seed. Heartbeats into `work_dir` so the Rust stall timeout sees a live
    stream even mid-chunk."""
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
                            data=bytes(pcm[i:i + CHUNK_BYTES]),
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
                            {"t_ms": pcm_pos_to_ms(len(out), OUTPUT_SR), "text": oat.text}
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


def _process_chunk(idx: int, chunk: dict, next_start: int | None, audio: str,
                   work_dir: str, pace: float) -> dict:
    """Translate one chunk (resumable): returns the chunk result dict. Reuses an
    existing `chunk_N.json` + `chunk_N.wav` on a re-run."""
    result_path = os.path.join(work_dir, f"chunk_{idx}.json")
    wav_path = os.path.join(work_dir, f"chunk_{idx}.wav")
    if os.path.exists(result_path) and os.path.exists(wav_path):
        _log(f"chunk {idx}: resume (already done)")
        with open(result_path, encoding="utf-8") as f:
            return json.load(f)

    start_ms = int(chunk["start_ms"])
    end_ms = int(chunk["end_ms"])
    chunk_len = end_ms - start_ms
    _heartbeat(work_dir)

    # 1. Cut + resample the chunk to the Live-API input format.
    pcm_path = os.path.join(work_dir, f"chunk_{idx}.in.pcm")
    _run(slice_resample_args(_ffmpeg(), audio, start_ms, end_ms, pcm_path))
    with open(pcm_path, "rb") as f:
        pcm = f.read()

    # 2. Translate (audio->audio) and trim the trailing silence.
    out_pcm, en, sk, sk_timed = _translate_pcm(pcm, pace, work_dir)
    raw_wav = os.path.join(work_dir, f"chunk_{idx}.raw.wav")
    _write_wav_from_pcm(out_pcm, OUTPUT_SR, raw_wav)
    _run([_ffmpeg(), "-hide_banner", "-nostdin", "-y", "-i", raw_wav,
          "-af", trim_silence_af(), wav_path])

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
        "transcript_en": en,
        "transcript_sk": sk,
        "sk_timed": sk_timed,
    }
    with open(result_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False)
    # Clean the big intermediates (keep chunk_N.wav + .json for resume/mix).
    for p in (pcm_path, raw_wav):
        try:
            os.remove(p)
        except OSError:
            pass
    _log(f"chunk {idx}: {chunk_len} ms in -> {out_len_ms} ms out, tempo {tempo:.3f}")
    return result


def _wav_duration_ms(path: str) -> int:
    with wave.open(path, "rb") as w:
        frames = w.getnframes()
        rate = w.getframerate()
    if rate <= 0:
        return 0
    return int(round(frames / rate * 1000))


def cmd_live_translate(args: argparse.Namespace) -> None:
    os.makedirs(args.work_dir, exist_ok=True)
    with open(args.chunk_plan, encoding="utf-8") as f:
        chunks = json.load(f)
    if not chunks:
        raise RuntimeError("empty chunk plan")

    results = []
    for i, chunk in enumerate(chunks):
        next_start = int(chunks[i + 1]["start_ms"]) if i + 1 < len(chunks) else None
        results.append(_process_chunk(i, chunk, next_start, args.audio, args.work_dir, args.pace))

    # Assemble the dub on the video timeline: place each chunk at its at_ms with
    # its atempo, mix, 48 kHz stereo, loudnorm -16 (owner-approved dub-only mix).
    placements = [(float(r["tempo"]), int(r["at_ms"])) for r in results]
    wavs = [os.path.join(args.work_dir, f"chunk_{r['index']}.wav") for r in results]
    filt = build_mix_filter(placements)
    mix_args = [_ffmpeg(), "-hide_banner", "-nostdin", "-y"]
    for w in wavs:
        mix_args += ["-i", w]
    mix_args += [
        "-filter_complex", f"{filt};[mix]loudnorm=I=-16:TP=-1.5:LRA=11[out]",
        "-map", "[out]", "-ar", str(FINAL_SR), "-ac", "2",
        args.out,
    ]
    _run(mix_args)

    # Transcripts JSON for D3 (EN + SK, per-chunk with timeline offsets).
    transcripts = {
        "engine": "gemini-live-translate",
        "target_lang": TARGET_LANG,
        "chunks": [
            {
                "index": r["index"],
                "start_ms": r["chunk_start_ms"],
                "end_ms": r["chunk_end_ms"],
                "en": r["transcript_en"],
                "sk": r["transcript_sk"],
                "sk_timed": r["sk_timed"],
            }
            for r in results
        ],
    }
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
    parser = argparse.ArgumentParser(description="Slovak dub synthesis (Gemini Live Translate)")
    sub = parser.add_subparsers(dest="cmd", required=True)
    lt = sub.add_parser("live-translate", help="translate a video's audio to a SK dub")
    lt.add_argument("--audio", required=True)
    lt.add_argument("--out", required=True)
    lt.add_argument("--transcripts", required=True)
    lt.add_argument("--chunk-plan", dest="chunk_plan", required=True)
    lt.add_argument("--work-dir", dest="work_dir", required=True)
    lt.add_argument("--pace", type=float, default=1.0)
    args = parser.parse_args()

    try:
        if args.cmd == "live-translate":
            cmd_live_translate(args)
        else:
            raise RuntimeError(f"unknown command: {args.cmd}")
    except Exception as e:  # loud failure: JSON error on stderr + non-zero exit.
        sys.stderr.write(json.dumps({"error": str(e)}) + "\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
