#!/usr/bin/env python3
"""dub_worker.py — Slovak dub synthesis via Gemini Live Translate (#183 D4,
#184 round H step 2: the frozen target state).

`live-translate` streams a video's audio into `gemini-3.5-live-translate-preview`
(audio->audio, owner ruling #174) as ONE logical Live session in the speaker's
own voice (`scripts/dub_live_session.py`: 1.0x wall-clock-paced 100 ms frames,
sliding-window context compression, session resumption with an overlapping
reconnect on GoAway), places the one continuous output stream on the VIDEO
timeline (`video_ms = arrival_ms - t0_ms - latency_ms`), and writes the Slovak
dub as the `--out` FLAC (48 kHz stereo, the stem format the playback
`StemMixReader` mixes), loudness-matched to `--audio` (round F) and promoted with
a POSIX-semantics rename (round F2), plus the EN/SK transcripts JSON for D3.

`--audio` is chosen by the Rust worker: the vocals stem when it exists, else the
normalized original. `--voice speaker` (default) sends NO `speech_config`; any
other value pins that prebuilt voice. `--model` makes a newer Live Translate
model a setting change. The Gemini key comes ONLY from `GEMINI_API_KEY` (env),
never argv/log. The child heartbeats into `--work-dir` so the Rust stall timeout
never kills a healthy real-time stream. Superseded (round C-E2) per-chunk
sessions, resume, re-synthesis and atempo placement are DELETED (#184 5798586079).

`google.genai` is imported lazily so this module imports under CI/ruff and the
pytest helpers run without the SDK.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import struct
import subprocess
import sys
import time

# #184 round F: the loudness rules ship next to this script (the Rust worker
# materialises it into the same tools dir, which is on sys.path for the child).
import dub_loudness as dl

# #184 round H step 2: the ONE continuous Live session (shipped next to it too).
import dub_live_session as dls

# #184 round F2: the partial dub is promoted with a POSIX-semantics rename, so a
# dub SongPlayer holds open (video loaded in SP-dabing) can still be replaced.
import win_replace as wr

INPUT_SR = dls.INPUT_SR
OUTPUT_SR = dls.OUTPUT_SR
FINAL_SR = 48000
MODEL = dls.MODEL
TARGET_LANG = "sk"
HEARTBEAT_EVERY_S = 5.0
# The measured first-voiced output latency is clamped to this range before it
# places the output: a value outside it is a measurement artefact (an intro the
# input-onset probe could not see, a very late first utterance), not the model.
LATENCY_MIN_MS = 1000
LATENCY_MAX_MS = 6000
# A connection's output stream may run this far ahead of its arrival time before
# its streamed silence is skipped to catch up (see `place_output`).
CATCH_UP_TOLERANCE_MS = 500
RAW_OUTPUT = "live_output.raw"  # the session's output PCM, arrival order
PLACED_WAV = "dub_placed.wav"  # the output placed on the video timeline
# The round C-E2 resume cache (per-chunk wav/json/pcm + the Rust chunk plan).
LEGACY_PREFIXES = ("chunk_",)

# ── pure helpers (unit-tested; no I/O, no heavy imports) ─────────────────────────


def decode_args(ffmpeg: str, audio: str) -> list[str]:
    """ffmpeg argv decoding the whole `audio` to 16 kHz mono s16le on stdout (the
    Live-API input format)."""
    return [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-i",
        audio,
        "-vn",
        "-ac",
        "1",
        "-ar",
        str(INPUT_SR),
        "-f",
        "s16le",
        "-",
    ]


def clamp_latency_ms(ms: float) -> int:
    """The placement latency, clamped to `LATENCY_MIN_MS..=LATENCY_MAX_MS`."""
    return int(round(min(max(ms, LATENCY_MIN_MS), LATENCY_MAX_MS)))


def measure_latency_ms(
    first_voiced_arrival_s: float | None, t0_s: float | None, onset_ms: int | None
) -> int | None:
    """The measured first-voiced output latency: when the first voiced output
    chunk arrived, minus when frame 0 was sent, minus where the first voiced
    INPUT frame sits (so a silent or instrumental intro is not counted as model
    latency). None when there was no voiced output or no send."""
    if first_voiced_arrival_s is None or t0_s is None:
        return None
    return int(round((first_voiced_arrival_s - t0_s) * 1000)) - (onset_ms or 0)


def timeline_ms(arrival_s: float, t0_s: float, latency_ms: int) -> int:
    """Video-timeline position of something that arrived at `arrival_s`:
    `arrival - t0 - latency`, never before the start."""
    return max(0, int(round((arrival_s - t0_s) * 1000)) - latency_ms)


def place_output(
    chunks: list, t0_s: float, latency_ms: int, sr: int = OUTPUT_SR
) -> list[int | None]:
    """Start SAMPLE of every output chunk (`.arrival_s`, `.conn`, `.n_bytes`,
    `.voiced`) on the video timeline, or None for a dropped one. Each
    connection's output is ONE continuous stream in arrival order: a chunk lands
    at `max(cursor, arrival - t0 - latency)`, so a burst never overlaps itself and
    a stall re-syncs to arrival. When the stream has run AHEAD of arrival by more
    than `CATCH_UP_TOLERANCE_MS` (a burst, output slightly faster than real
    time), its streamed SILENCE is dropped until it is back — voiced audio is
    never dropped — so the dub does not drift late for the rest of the
    connection. Connections keep separate cursors: the old connection's trailing
    translation and the new one's start overlap in time (they are summed by
    `render_placed_wav`); one cursor across both would push every later second
    late by the overlap."""
    tolerance = CATCH_UP_TOLERANCE_MS * sr // 1000
    cursors: dict[int, int] = {}
    starts: list[int | None] = []
    for c in chunks:
        want = timeline_ms(c.arrival_s, t0_s, latency_ms) * sr // 1000
        cursor = cursors.get(c.conn, 0)
        if not c.voiced and cursor - want > tolerance:
            starts.append(None)  # skip streamed silence to catch up
            continue
        pos = max(cursor, want)
        starts.append(pos)
        cursors[c.conn] = pos + c.n_bytes // 2
    return starts


def _by_connection(parts: list, conn_of) -> list:
    """`parts` stably ordered by connection: the old connection translates
    EARLIER input than the next one, even when its trailing text arrives after
    the next connection's first text (the overlap)."""
    return sorted(parts, key=conn_of)


def joined_by_connection(parts: list) -> str:
    """The `(conn, text)` transcription parts joined in connection order."""
    return "".join(text for _, text in _by_connection(parts, lambda p: p[0]))


def sk_timed_from(parts: list, t0_s: float, latency_ms: int) -> list[dict]:
    """The SK output-transcription fragments `(arrival_s, text, conn)` in
    connection order, stamped on the video timeline (`t_ms`, non-decreasing —
    the D3 subtitle builder reads them as consecutive windows). A connection's
    fragments that arrived after the NEXT connection's first fragment (its
    trailing translation during the overlap) are capped at that time, so the
    next connection's subtitles keep their own arrival times — its audio is
    placed at its arrival too — instead of being pushed later."""
    first_of: dict[int, float] = {}
    for arrival_s, text, conn in parts:
        if text and conn not in first_of:
            first_of[conn] = arrival_s
    conns = sorted(first_of)
    next_first = {c: first_of[n] for c, n in zip(conns, conns[1:])}
    out = []
    last = 0
    for arrival_s, text, conn in _by_connection(parts, lambda p: p[2]):
        if not text:
            continue
        capped = min(arrival_s, next_first.get(conn, arrival_s))
        last = max(last, timeline_ms(capped, t0_s, latency_ms))
        out.append({"t_ms": last, "text": text})
    return out


def build_transcripts(en: str, sk: str, sk_timed: list, total_ms: int) -> dict:
    """The EN/SK transcripts JSON (the D3 #182 subtitle source) in the shape
    `dabing::subtitles::DubTranscripts` reads: ONE chunk covering the whole video
    (`at_ms` 0, `tempo` 1.0 — the output is not stretched), so the SK fragment
    times are video-timeline times. Pure — unit-tested."""
    return {
        "engine": "gemini-live-translate",
        "target_lang": TARGET_LANG,
        "chunks": [
            {
                "index": 0,
                "start_ms": 0,
                "end_ms": total_ms,
                "at_ms": 0,
                "tempo": 1.0,
                "en": en,
                "sk": sk,
                "sk_timed": sk_timed,
            }
        ],
    }


def wav_header(n_samples: int, sr: int) -> bytes:
    """The 44-byte header of a 16-bit mono PCM WAV of `n_samples` samples."""
    data = n_samples * 2
    return (
        b"RIFF"
        + struct.pack("<I", 36 + data)
        + b"WAVEfmt "
        + struct.pack("<IHHIIHH", 16, 1, 1, sr, sr * 2, 2, 16)
        + b"data"
        + struct.pack("<I", data)
    )


def stream_filter() -> str:
    """The `filter_complex` for the ONE placed output WAV (input 0): resample to
    48 kHz into `[mix]`, which the round-F loudnorm passes read; `-ac 2` on the
    output upmixes the mono stream to stereo."""
    return f"[0:a]aresample={FINAL_SR}[mix]"


def legacy_work_files(names: list[str]) -> list[str]:
    """The round C-E2 per-chunk resume leftovers among a work dir's `names`."""
    return sorted(n for n in names if n.startswith(LEGACY_PREFIXES))


# ── I/O helpers ─────────────────────────────────────────────────────────────────


def _log(msg: str) -> None:
    """Progress goes to stderr (stdout is reserved for the summary JSON)."""
    sys.stderr.write(msg.rstrip() + "\n")
    sys.stderr.flush()


def _run_stderr(args: list[str]) -> str:
    """Run `args`; return its stderr (ffmpeg prints the loudnorm JSON there).
    Fails loudly on a non-zero exit."""
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


def _heartbeat(work_dir: str) -> None:
    try:
        with open(os.path.join(work_dir, "heartbeat"), "w") as f:
            f.write(str(time.time()))
    except OSError as e:  # the stall timeout will say so if this persists
        _log(f"heartbeat write failed: {e}")


def _decode_input(audio: str) -> bytes:
    r = subprocess.run(decode_args(_ffmpeg(), audio), capture_output=True)
    if r.returncode != 0:
        tail = r.stderr.decode("utf-8", "replace")[-800:]
        raise RuntimeError(f"ffmpeg decode of {audio} failed ({r.returncode}): {tail}")
    if not r.stdout:
        raise RuntimeError(f"ffmpeg decoded no audio from {audio}")
    return r.stdout


def _remove_legacy_work_files(work_dir: str) -> None:
    """Delete the superseded round C-E2 per-chunk resume cache, if any."""
    stale = legacy_work_files(os.listdir(work_dir))
    for name in stale:
        os.remove(os.path.join(work_dir, name))
    if stale:
        _log(f"removed {len(stale)} superseded per-chunk work files from {work_dir}")


def render_placed_wav(
    raw_path: str,
    chunks: list,
    starts: list[int | None],
    total_samples: int,
    wav_path: str,
) -> None:
    """Write the placed output as a 24 kHz mono 16-bit WAV of `total_samples`:
    chunk `i` (read from `raw_path` at its `.offset`) is ADDED at `starts[i]`
    (overlapping connections sum, clipped to int16; a None start = a dropped
    chunk); samples past the end are dropped. Memory-light: the WAV body is a
    memmap, the raw file is read per chunk."""
    import numpy as np

    header = wav_header(total_samples, OUTPUT_SR)
    with open(wav_path, "wb") as f:
        f.write(header)
        f.truncate(len(header) + total_samples * 2)  # zero-filled body
    if total_samples == 0:
        return
    out = np.memmap(
        wav_path, dtype="<i2", mode="r+", offset=len(header), shape=(total_samples,)
    )
    try:
        with open(raw_path, "rb") as raw:
            for c, pos in zip(chunks, starts):
                if pos is None:
                    continue
                raw.seek(c.offset)
                data = np.frombuffer(raw.read(c.n_bytes), dtype="<i2")
                end = min(pos + data.size, total_samples)
                if end <= pos:
                    continue
                acc = out[pos:end].astype(np.int32) + data[: end - pos]
                out[pos:end] = np.clip(acc, -32768, 32767).astype("<i2")
        out.flush()
    finally:
        # Unmap even on a failure: Windows cannot delete a mapped file, and the
        # cleanup of a failed run must be able to remove it.
        del out


def _assemble_dub(audio: str, wav: str, out: str, work_dir: str) -> dict:
    """#184 round F: assemble the dub loudness-matched to the audio it translates
    (`audio`, the child's `--audio`):

    1. measure `audio`'s integrated loudness → `dl.loudness_target` (clamped);
    2. analyse the placed output against that target (loudnorm pass 1, to null);
    3. write the 48 kHz stereo dub with the LINEAR second pass
       (`dl.build_loudnorm_second_pass`) to a PARTIAL file, promoted over `out`
       (`wr.replace_file`, a POSIX rename that works while SongPlayer holds `out`
       open) only once ffmpeg succeeded AND reported its loudness — a failed
       rebuild never destroys the previous good dub.

    Heartbeats before each full-length pass. Returns the loudness stats (also
    written, JSON-safe, to `<work_dir>/loudness.json` as box-side evidence). Any
    ffmpeg failure or unparseable measurement raises — the dub fails loudly
    rather than shipping at an unknown level."""
    ff = _ffmpeg()
    part = dl.partial_out_path(out)
    # A child hard-killed mid final pass (stall timeout, server exit) never ran
    # the cleanup below — clear its leftover partial before anything else.
    if os.path.exists(part):
        _log(f"dub loudness: removing a stale partial {part} (an earlier run died)")
        os.remove(part)
    _heartbeat(work_dir)
    source = dl.parse_loudnorm_json(_run_stderr(dl.loudness_measure_args(ff, audio)))
    target = dl.loudness_target(source["input_i"])
    mix_filter = stream_filter()
    analysis = dl.loudnorm_analysis_filter(target)
    _heartbeat(work_dir)
    mix = dl.parse_loudnorm_json(
        _run_stderr(dl.assembly_args(ff, [wav], mix_filter, analysis, None, FINAL_SR))
    )
    second = dl.build_loudnorm_second_pass(mix, target)
    _heartbeat(work_dir)
    try:
        applied = dl.parse_loudnorm_json(
            _run_stderr(dl.assembly_args(ff, [wav], mix_filter, second, part, FINAL_SR))
        )
        # NOT os.replace: on Windows that is MoveFileExW, which fails with
        # WinError 5 while SongPlayer holds `out` open (#184 round F2).
        wr.replace_file(part, out)
    except BaseException:
        # The previous good dub stays; drop the half-written partial, then
        # re-raise the ORIGINAL failure (a cleanup error is logged, not raised).
        if os.path.exists(part):
            try:
                os.remove(part)
            except OSError as e:
                _log(f"dub loudness: could not remove partial {part}: {e}")
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


# ── the Live session (the real connection) ──────────────────────────────────────


def _run_session(
    frames: list[bytes],
    model: str,
    voice: str | None,
    work_dir: str,
    sink: dls.FileSink,
    events: dls.EventLog,
) -> dls.SessionState:
    import google.genai as genai
    from google.genai import types

    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        raise RuntimeError("GEMINI_API_KEY not set")
    client = genai.Client(api_key=key)

    def connect(cfg: dict):
        return client.aio.live.connect(
            model=model, config=types.LiveConnectConfig(**cfg)
        )

    def make_blob(frame: bytes):
        return types.Blob(data=frame, mime_type=f"audio/pcm;rate={INPUT_SR}")

    last_hb = [0.0]

    def on_progress() -> None:
        # Only called when frames or output moved: a stuck session goes stale
        # for the Rust stall timeout instead of looking alive.
        if time.monotonic() - last_hb[0] >= HEARTBEAT_EVERY_S:
            _heartbeat(work_dir)
            last_hb[0] = time.monotonic()

    session = dls.ContinuousSession(
        frames,
        connect,
        make_blob,
        dls.SessionOptions(target_lang=TARGET_LANG, voice=voice),
        events,
        sink,
        secret=key,
        log=_log,
        on_progress=on_progress,
    )
    return asyncio.run(session.run())


def _remove_intermediates(*paths: str) -> None:
    """Delete the raw + placed intermediates (~100 MB each for a 36-min talk);
    `events.jsonl` / `session_summary.json` / `loudness.json` stay as evidence.
    A file that cannot be removed is logged, never raised: this runs in a
    `finally`, and its error must not replace the run's real one."""
    for path in paths:
        if not os.path.exists(path):
            continue
        try:
            os.remove(path)
        except OSError as e:
            _log(f"could not remove the intermediate {path}: {e}")


def _translate(args: argparse.Namespace, raw_path: str, wav_path: str) -> dict:
    """Decode → the ONE continuous session → place → assemble → transcripts.
    Returns the session summary (the stdout `session` object)."""
    voice = dls.voice_for_config(args.voice)
    _log(
        f"dub: model {args.model}, voice {voice or 'speaker (no speech_config)'}, "
        f"input {args.audio}"
    )
    _heartbeat(args.work_dir)
    pcm = _decode_input(args.audio)
    input_s = len(pcm) / 2 / INPUT_SR
    frames = dls.pcm_frames(pcm)
    del pcm
    onset = dls.first_voiced_frame(frames)
    onset_ms = None if onset is None else int(round(onset * dls.FRAME_S * 1000))

    events = dls.EventLog(os.path.join(args.work_dir, "events.jsonl"))
    sink = dls.FileSink(raw_path)
    try:
        state = _run_session(frames, args.model, voice, args.work_dir, sink, events)
    finally:
        sink.close()
        events.close()
    summary = dls.build_summary(state, events.events, input_s)

    voiced = [c.arrival_s for c in state.chunks if c.voiced]
    measured = measure_latency_ms(min(voiced) if voiced else None, state.t0_s, onset_ms)
    if measured is None or state.t0_s is None:
        raise RuntimeError(
            f"the Live session produced no voiced output ({summary['output_audio_s']} s)"
        )
    latency_ms = clamp_latency_ms(measured)
    summary.update(
        latency_ms=latency_ms, measured_latency_ms=measured, input_onset_ms=onset_ms
    )

    # Place the ONE continuous output stream on the video timeline and write it.
    starts = place_output(state.chunks, state.t0_s, latency_ms)
    summary["dropped_silence_s"] = round(
        sum(c.n_bytes for c, s in zip(state.chunks, starts) if s is None)
        / 2
        / OUTPUT_SR,
        3,
    )
    with open(
        os.path.join(args.work_dir, "session_summary.json"), "w", encoding="utf-8"
    ) as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    _log(
        f"dub session: connections {summary['connections']}, reconnects "
        f"{summary['reconnects']}, output/input {summary['output_to_input_ratio']}, "
        f"max voiced gap {summary['max_voiced_gap_s']}s, latency {latency_ms} ms "
        f"(measured {measured} ms, input onset {onset_ms} ms), dropped silence "
        f"{summary['dropped_silence_s']}s, drain {summary['drain_end_reason']}"
    )
    input_samples = int(round(input_s * OUTPUT_SR))
    ends = [s + c.n_bytes // 2 for s, c in zip(starts, state.chunks) if s is not None]
    total = max([input_samples] + ends)
    _heartbeat(args.work_dir)
    render_placed_wav(raw_path, state.chunks, starts, total, wav_path)

    # #184 round F (owner verdict 2026-09-23, #184 comment 5793796815 — "rovnaké
    # pomery = rovnaká hlasitosť"): normalized to the MEASURED loudness of the
    # audio it translates (clamped -24..-10 LUFS), two-pass LINEAR loudnorm.
    _assemble_dub(args.audio, wav_path, args.out, args.work_dir)

    # Transcripts JSON for D3: one chunk on the video timeline, the overlap's
    # two connections in connection order (never interleaved).
    sk_parts = [(conn, text) for _, text, conn in state.output_parts]
    transcripts = build_transcripts(
        joined_by_connection(state.input_parts),
        joined_by_connection(sk_parts),
        sk_timed_from(state.output_parts, state.t0_s, latency_ms),
        int(round(input_s * 1000)),
    )
    with open(args.transcripts, "w", encoding="utf-8") as f:
        json.dump(transcripts, f, ensure_ascii=False)
    return summary


def cmd_live_translate(args: argparse.Namespace) -> None:
    os.makedirs(args.work_dir, exist_ok=True)
    _remove_legacy_work_files(args.work_dir)
    raw_path = os.path.join(args.work_dir, RAW_OUTPUT)
    wav_path = os.path.join(args.work_dir, PLACED_WAV)
    try:
        summary = _translate(args, raw_path, wav_path)
    finally:
        _remove_intermediates(raw_path, wav_path)

    # The summary JSON is the ONLY thing on stdout (the Rust worker parses it).
    sys.stdout.write(
        json.dumps(
            {
                "out_path": args.out,
                "transcripts_path": args.transcripts,
                "session": summary,
            },
            ensure_ascii=True,
        )
        + "\n"
    )
    sys.stdout.flush()


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Slovak dub synthesis (Gemini Live Translate, one session)"
    )
    sub = parser.add_subparsers(dest="cmd", required=True)
    lt = sub.add_parser("live-translate", help="translate a video's audio to a SK dub")
    lt.add_argument("--audio", required=True, help="the vocals stem or the original")
    lt.add_argument("--out", required=True)
    lt.add_argument("--transcripts", required=True)
    lt.add_argument("--work-dir", dest="work_dir", required=True)
    lt.add_argument("--model", default=MODEL)
    lt.add_argument(
        "--voice",
        default=dls.SPEAKER_VOICE,
        help="'speaker' (the speaker's own voice, no speech_config) or a prebuilt name",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    try:
        if args.cmd == "live-translate":
            cmd_live_translate(args)
        else:
            raise RuntimeError(f"unknown command: {args.cmd}")
    except Exception as e:  # loud failure: JSON error on stderr + non-zero exit.
        # Defensive: never let the key leak into the logged error tail, even if an
        # SDK exception embedded it.
        msg = dls.redact(f"{type(e).__name__}: {e}", os.environ.get("GEMINI_API_KEY"))
        sys.stderr.write(json.dumps({"error": msg}) + "\n")
        sys.exit(1)


if __name__ == "__main__":
    main()
