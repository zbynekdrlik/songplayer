#!/usr/bin/env python3
"""live_translate_continuous_probe.py — ONE logical Gemini Live Translate session
over a whole talk (#184 round H step 1).

The production dub child (`scripts/dub_worker.py`) opens a FRESH Live session per
<= 120 s chunk with a forced prebuilt voice; the model's documented limitation
("voices might shift after long pauses, assign the wrong gender based on how the
speech starts") fires at every one of those session starts. The documented way
to run long is ONE session with sliding-window context compression + session
resumption across ~10-min connections, fed at 1.0x real time in 100 ms frames.
Two capabilities of THIS model are UNVERIFIED in the docs — does
`gemini-3.5-live-translate-preview` honour `context_window_compression` /
`session_resumption` / GoAway, and how does a `speech_config` voice interact with
its voice copying — so this probe measures them before step 2 rebuilds the dub
worker. Design record: songplayer#184 comment 5794427305.

What it does: decode `--audio` [start, start+duration) to 16 kHz mono s16le;
stream it in 100 ms frames at 1.0x real time with wall-clock drift correction
(frame k is sent at `anchor + k*0.1`, re-anchored per connection, never a summed
`sleep(0.1)`); record every server message field with a monotonic timestamp
(fields the probe has no dedicated event for land in `other_fields`); on GoAway
stop sending at a frame boundary, keep receiving the old connection's trailing
translation for a bounded grace, then reconnect with the latest resumption
handle and continue from the next unsent frame (same on a connection that
closes before the slice is fully sent); `audio_stream_end` is sent once, after
the last frame; then keep receiving until there has been no VOICED output (a
chunk above `VOICED_DBFS` — the session streams silence after speech) for
`QUIET_S`, or `TAIL_CAP_S` passes.

Outputs in `--out-dir`: `output.wav` (24 kHz mono, the output PCM in arrival
order), `output_chunks.json` (`[arrival_s, n_bytes, buffer_offset_s]` per chunk,
so a constant-offset placement is reconstructable), `input_text.txt`,
`output_text.txt`, `events.jsonl` (flushed per line — a crash keeps what was
seen), `summary.json`; the summary JSON is the ONLY stdout line (progress goes
to stderr).

The key comes ONLY from `GEMINI_API_KEY` (env) and is redacted from every
error / event text. `google-genai` is imported lazily inside `_run_live` so the
module and its pure helpers import (and are unit-tested) without the SDK.

SDK surface VERIFIED against the google-genai 2.24.0 SOURCE (the version in the
box `lyrics_venv`; wheel read 2026-09-23, `google/genai/types.py` + `live.py`):
  - `LiveConnectConfig` fields `response_modalities`, `speech_config`,
    `session_resumption`, `input_audio_transcription`,
    `output_audio_transcription`, `context_window_compression`,
    `translation_config`; the SDK BaseModel is `extra='forbid'`, so a wrong
    field name fails loudly at config build.
  - `ContextWindowCompressionConfig{trigger_tokens, sliding_window}`,
    `SlidingWindow{target_tokens}`, `SessionResumptionConfig{handle, transparent}`,
    `TranslationConfig{target_language_code, echo_target_language}`,
    `SpeechConfig{voice_config: VoiceConfig{prebuilt_voice_config:
    PrebuiltVoiceConfig{voice_name}}}`.
  - `LiveServerMessage{setup_complete, server_content, usage_metadata, go_away,
    session_resumption_update, ...}` + the `.data` property (concatenated
    inline audio); `LiveServerContent{input_transcription, output_transcription,
    turn_complete, generation_complete, interrupted}`;
    `LiveServerGoAway{time_left}`; `LiveServerSessionResumptionUpdate{new_handle,
    resumable, last_consumed_client_message_index}` — the index is only sent
    when `SessionResumptionConfig.transparent` is set, which this probe does NOT
    set (the design resumes by handle only), so it is expected to log `None`
    and `frames_since_handle` is the probe's own estimate of the input the
    resumed state may miss.
  - `AsyncSession.send_realtime_input(audio=Blob | audio_stream_end=True)`.
  - `AsyncSession.receive()` ENDS its iteration once an interaction completes
    (`live.py::_is_interaction_complete`: `interaction_status == IDLE` when the
    server sets one, else `turn_complete`), so a continuous session must
    re-enter it; a closed websocket raises `errors.APIError` (code = the close
    code). `LiveServerGoAway.time_left` is a Duration string (`"9s"`, `"2.5s"`).
  - The setup_complete message is consumed by `connect()` and exposed as
    `session.setup_complete` — it never appears in `receive()`.
  - The Developer-API default `api_version` is `v1beta` (`_api_client.py`).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import subprocess
import sys
import time
import wave
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

import numpy as np

MODEL = "gemini-3.5-live-translate-preview"
INPUT_SR = 16000
OUTPUT_SR = 24000
FRAME_S = 0.1
FRAME_BYTES = 3200  # 100 ms @ 16 kHz s16le mono
QUIET_S = 8.0  # no VOICED output this long after the last frame -> drained
TAIL_CAP_S = 60.0  # hard cap on the post-slice drain
MAX_CONNECTIONS = 50  # a runaway reconnect loop is a finding, not an endless run
RMS_WINDOW_S = 300.0  # the per-5-min output level trend
SILENCE_DBFS = -120.0  # floor for an all-zero window (JSON has no -inf)
# An output chunk louder than this is speech; the session streams (near-)digital
# silence between/after utterances, well below it; speech sits around -30..-15.
VOICED_DBFS = -50.0
GO_AWAY_MARGIN_S = 1.0  # leave the old connection this long before its time_left
SEND_STOP_TIMEOUT_S = 2.0  # let an in-flight frame send finish before cancelling
PROGRESS_EVERY_FRAMES = 600  # one stderr progress line per minute of input
# Top-level / server_content fields with a dedicated event (or none needed);
# anything else a message carries is logged as `other_fields`.
_KNOWN_FIELDS = {
    "data",
    "server_content",
    "usage_metadata",
    "go_away",
    "session_resumption_update",
    "setup_complete",
}
_KNOWN_CONTENT_FIELDS = {
    "model_turn",
    "input_transcription",
    "output_transcription",
    "turn_complete",
    "generation_complete",
    "interrupted",
}

# ── pure helpers (no network; unit-tested) ─────────────────────────────────────


def frame_deadline(t0: float, k: int, frame_s: float = FRAME_S) -> float:
    """Wall-clock send time of frame `k` of a schedule anchored at `t0`.
    Computed, never accumulated — so frame 15 000 is still exactly `t0+1500 s`."""
    return t0 + k * frame_s


def pcm_frames(pcm: bytes, frame_bytes: int = FRAME_BYTES) -> list[bytes]:
    """Split PCM into fixed frames; the last partial frame is zero-padded."""
    frames = [pcm[i : i + frame_bytes] for i in range(0, len(pcm), frame_bytes)]
    if frames and len(frames[-1]) < frame_bytes:
        frames[-1] = frames[-1] + b"\x00" * (frame_bytes - len(frames[-1]))
    return frames


def should_reconnect(kind: str, sent_all: bool) -> bool:
    """A GoAway always reconnects (the server is about to drop us — after the
    old connection's grace, and only if the drain did not end first); a closed
    connection reconnects only while input is still unsent; `done` never does."""
    if kind == "go_away":
        return True
    if kind == "closed":
        return not sent_all
    return False


def max_gap(arrivals: list[float]) -> float:
    """Largest gap between consecutive output-chunk arrivals (0.0 if < 2)."""
    ts = sorted(arrivals)
    if len(ts) < 2:
        return 0.0
    return max(b - a for a, b in zip(ts, ts[1:]))


def _samples_dbfs(samples: np.ndarray) -> float:
    if samples.size == 0:
        return SILENCE_DBFS
    w = samples.astype(np.float64) / 32768.0
    rms = float(np.sqrt(np.mean(w * w)))
    db = 20.0 * math.log10(rms) if rms > 0 else SILENCE_DBFS
    return round(max(db, SILENCE_DBFS), 2)


def _s16(pcm_s16le: bytes) -> np.ndarray:
    return np.frombuffer(pcm_s16le[: len(pcm_s16le) // 2 * 2], dtype="<i2")


def pcm_dbfs(pcm_s16le: bytes) -> float:
    """RMS level of mono s16le PCM in dBFS, floored at `SILENCE_DBFS`."""
    return _samples_dbfs(_s16(pcm_s16le))


def rms_windows(pcm_s16le: bytes, sr: int, window_s: float) -> list[float]:
    """RMS level (dBFS, floored at `SILENCE_DBFS`) per `window_s` window of mono
    s16le PCM; a trailing partial window is included."""
    samples = _s16(pcm_s16le)
    step = max(1, int(round(sr * window_s)))
    return [_samples_dbfs(samples[i : i + step]) for i in range(0, len(samples), step)]


def go_away_grace_s(time_left: str | None, quiet_s: float) -> float:
    """How long to keep receiving on a connection after its GoAway: until
    `GO_AWAY_MARGIN_S` before the server's `time_left` (a Duration string such
    as `"9s"`), capped at `quiet_s`; an unknown `time_left` -> `quiet_s`."""
    text = (time_left or "").strip()
    if not text.endswith("s"):
        return quiet_s
    try:
        left = float(text[:-1])
    except ValueError:
        return quiet_s
    return max(0.0, min(left - GO_AWAY_MARGIN_S, quiet_s))


def place_output(
    chunks: list[tuple[float, int]], sr: int = OUTPUT_SR
) -> list[tuple[float, int, float]]:
    """`(arrival_s, n_bytes)` per output chunk -> `(arrival_s, n_bytes,
    buffer_offset_s)`: where each chunk starts in the continuous output buffer
    (monotonic), so arrival-time placement can be rebuilt offline."""
    placed = []
    pos = 0
    for arrival_s, n_bytes in chunks:
        placed.append((arrival_s, n_bytes, pos / 2 / sr))
        pos += n_bytes
    return placed


def redact(text: str, key: str | None) -> str:
    """Remove the API key from any text (an SDK error may embed the URL)."""
    if not key:
        return text
    return text.replace(key, "<redacted>")


def decode_args(
    ffmpeg: str, audio: str, start_s: float, duration_s: float | None
) -> list[str]:
    """ffmpeg argv decoding the slice to 16 kHz mono s16le on stdout (input
    seek before `-i` = fast)."""
    args = [ffmpeg, "-hide_banner", "-loglevel", "error", "-ss", str(start_s)]
    if duration_s is not None:
        args += ["-t", str(duration_s)]
    args += ["-i", audio, "-vn", "-ac", "1", "-ar", str(INPUT_SR), "-f", "s16le", "-"]
    return args


@dataclass
class ProbeOptions:
    target_lang: str
    voice: str | None
    compression: bool
    trigger_tokens: int
    target_tokens: int
    resumption: bool
    frame_s: float = FRAME_S
    quiet_s: float = QUIET_S
    tail_cap_s: float = TAIL_CAP_S
    max_connections: int = MAX_CONNECTIONS
    # The pacer's clock + sleep (injectable so the 1.0x schedule is testable on a
    # virtual clock; the drain / GoAway timers always use the real event log).
    clock: Callable[[], float] = time.monotonic
    sleep: Callable[[float], Awaitable[None]] = asyncio.sleep


def live_config(opts: ProbeOptions, handle: str | None) -> dict:
    """The `LiveConnectConfig` kwargs (plain dicts, SDK field names) for one
    connection. `speech_config` only in the pinned-voice arm; compression and
    resumption unless switched off."""
    cfg: dict[str, Any] = {
        "response_modalities": ["AUDIO"],
        "translation_config": {
            "target_language_code": opts.target_lang,
            "echo_target_language": False,
        },
        "input_audio_transcription": {},
        "output_audio_transcription": {},
    }
    if opts.compression:
        cfg["context_window_compression"] = {
            "trigger_tokens": opts.trigger_tokens,
            "sliding_window": {"target_tokens": opts.target_tokens},
        }
    if opts.resumption:
        cfg["session_resumption"] = {"handle": handle}
    if opts.voice:
        cfg["speech_config"] = {
            "voice_config": {"prebuilt_voice_config": {"voice_name": opts.voice}}
        }
    return cfg


def build_summary(
    events: list[dict],
    *,
    model: str,
    api_version: str,
    voice: str | None,
    compression: bool,
    resumption: bool,
    slice_s: float,
    frames_sent: int,
    frames_total: int,
    output_pcm: bytes,
    output_sr: int,
    errors: list[str],
) -> dict:
    """The one-line result: session counts from the event log + output levels."""

    def count(kind: str, **match: Any) -> int:
        return sum(
            1
            for e in events
            if e["kind"] == kind and all(e.get(k) == v for k, v in match.items())
        )

    audio = [e for e in events if e["kind"] == "audio"]
    audio_t = [e["t"] for e in audio]
    voiced = [e for e in audio if e.get("voiced")]
    voiced_t = [e["t"] for e in voiced]
    send_t = [e["t"] for e in events if e["kind"] == "send_start"]
    drain_ends = [e.get("reason") for e in events if e["kind"] == "drain_end"]
    output_s = len(output_pcm) / 2 / output_sr

    def latency(ts: list[float]) -> float | None:
        return round(ts[0] - send_t[0], 3) if ts and send_t else None

    return {
        "model": model,
        "api_version": api_version,
        "voice": voice or "none",
        "compression": compression,
        "resumption": resumption,
        "slice_s": round(slice_s, 3),
        "frames_sent": frames_sent,
        "frames_total": frames_total,
        "connections": count("connect"),
        "reconnects": count("reconnect"),
        "resumptions_offered": count("session_resumption_update", handle_present=True),
        "go_aways": count("go_away"),
        "reconnect_failures": count("reconnect_failed"),
        "output_audio_s": round(output_s, 3),
        "output_to_input_ratio": round(output_s / slice_s, 4) if slice_s > 0 else None,
        # All chunks (the session also streams silence): arrival/network stalls.
        "max_output_gap_s": round(max_gap(audio_t), 3),
        # Voiced chunks only: the gaps a listener hears in the dub.
        "max_voiced_gap_s": round(max_gap(voiced_t), 3),
        "voiced_output_s": round(sum(e["n_bytes"] for e in voiced) / 2 / output_sr, 3),
        "first_output_latency_s": latency(audio_t),
        "first_voiced_latency_s": latency(voiced_t),
        "drain_end_reason": drain_ends[-1] if drain_ends else None,
        "rms_per_5min": rms_windows(output_pcm, output_sr, RMS_WINDOW_S),
        "errors": list(errors),
    }


# ── the event log + the probe state ─────────────────────────────────────────────


class EventLog:
    """Every server message kind / probe decision with a monotonic `t` (s since
    the log opened); mirrored line-by-line to `events.jsonl` when a path is
    given, flushed per line so a crash keeps everything seen so far."""

    def __init__(self, path: str | None) -> None:
        self.t0 = time.monotonic()
        self.events: list[dict] = []
        self._fh = open(path, "w", encoding="utf-8") if path else None

    def now(self) -> float:
        return time.monotonic() - self.t0

    def log(self, kind: str, **fields: Any) -> dict:
        ev = {"t": round(self.now(), 4), "kind": kind, **fields}
        self.events.append(ev)
        if self._fh is not None:
            self._fh.write(json.dumps(ev, ensure_ascii=False) + "\n")
            self._fh.flush()
        return ev

    def close(self) -> None:
        if self._fh is not None:
            self._fh.close()
            self._fh = None


@dataclass
class ProbeState:
    frames_sent: int = 0  # == index of the next unsent frame
    stream_end_sent: bool = False
    sent_all_t: float | None = None
    last_audio_t: float | None = None
    last_voiced_t: float | None = None
    first_send_logged: bool = False
    latest_handle: str | None = None
    frames_at_handle: int = 0  # frames_sent when the latest handle arrived
    out: bytearray = field(default_factory=bytearray)
    chunks: list[tuple[float, int]] = field(default_factory=list)
    input_parts: list[str] = field(default_factory=list)
    output_parts: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def _err(e: BaseException, secret: str | None) -> str:
    return redact(f"{type(e).__name__}: {e}", secret)


def _set_fields(obj: Any) -> dict[str, Any]:
    """The non-None fields of an SDK message (pydantic) or a test double."""
    model_fields = getattr(type(obj), "model_fields", None)
    names = list(model_fields) if model_fields else list(vars(obj))
    return {
        n: getattr(obj, n, None) for n in names if getattr(obj, n, None) is not None
    }


def _other_fields(msg: Any) -> dict[str, Any]:
    """Message fields the probe has no dedicated event for, name -> value
    (dotted for server_content subfields and for model_turn parts other than the
    inline audio), so an unexpected message kind is never silent."""
    other = {n: v for n, v in _set_fields(msg).items() if n not in _KNOWN_FIELDS}
    sc = getattr(msg, "server_content", None)
    if sc is None:
        return other
    for n, v in _set_fields(sc).items():
        if n not in _KNOWN_CONTENT_FIELDS:
            other[f"server_content.{n}"] = v
    turn = getattr(sc, "model_turn", None)
    for part in getattr(turn, "parts", None) or []:
        for n, v in _set_fields(part).items():
            if n != "inline_data":  # the audio, captured through `.data`
                other[f"server_content.model_turn.parts.{n}"] = v
    return other


def record_message(msg: Any, state: ProbeState, events: EventLog) -> Any:
    """Record one server message; return its GoAway (or None)."""
    data = getattr(msg, "data", None)
    if data:
        t = events.now()
        db = pcm_dbfs(data)
        voiced = db > VOICED_DBFS
        state.out.extend(data)
        state.chunks.append((round(t, 4), len(data)))
        state.last_audio_t = t
        if voiced:
            state.last_voiced_t = t
        events.log("audio", n_bytes=len(data), dbfs=db, voiced=voiced)
    other = _other_fields(msg)
    if other:
        events.log(
            "other_fields",
            fields=list(other),
            detail={n: str(v)[:300] for n, v in other.items()},
        )
    sc = getattr(msg, "server_content", None)
    if sc is not None:
        it = getattr(sc, "input_transcription", None)
        if it is not None and getattr(it, "text", None):
            state.input_parts.append(it.text)
            events.log("input_transcription", text=it.text)
        ot = getattr(sc, "output_transcription", None)
        if ot is not None and getattr(ot, "text", None):
            state.output_parts.append(ot.text)
            events.log("output_transcription", text=ot.text)
        for flag in ("turn_complete", "generation_complete", "interrupted"):
            if getattr(sc, flag, None):
                events.log(flag)
    sru = getattr(msg, "session_resumption_update", None)
    if sru is not None:
        handle = getattr(sru, "new_handle", None)
        if handle:
            state.latest_handle = handle
            state.frames_at_handle = state.frames_sent
        events.log(
            "session_resumption_update",
            handle_present=bool(handle),
            resumable=getattr(sru, "resumable", None),
            # Only sent with SessionResumptionConfig.transparent (not set here):
            # logged anyway so a server that sends it regardless is visible.
            last_consumed_client_message_index=getattr(
                sru, "last_consumed_client_message_index", None
            ),
        )
    um = getattr(msg, "usage_metadata", None)
    if um is not None:
        events.log(
            "usage_metadata",
            prompt_token_count=getattr(um, "prompt_token_count", None),
            response_token_count=getattr(um, "response_token_count", None),
            total_token_count=getattr(um, "total_token_count", None),
        )
    ga = getattr(msg, "go_away", None)
    if ga is not None:
        time_left = getattr(ga, "time_left", None)
        events.log("go_away", time_left=None if time_left is None else str(time_left))
    return ga


# ── the session loop (network-agnostic: `connect` / `make_blob` are injected) ───


async def _run_connection(
    session: Any,
    frames: list[bytes],
    make_blob: Callable[[bytes], Any],
    opts: ProbeOptions,
    events: EventLog,
    state: ProbeState,
    secret: str | None,
) -> str:
    """Stream the remaining frames into one connection while receiving. Returns
    `go_away` (after the old connection's grace), `closed` or `done` (drained
    after the whole slice was sent)."""
    k0 = state.frames_sent
    anchor = opts.clock()  # re-anchored per connection: 1.0x from here on
    stop_send = asyncio.Event()  # checked at every frame boundary
    go_away_deadline: list[float] = []  # set once, by the receiver

    async def send() -> None:
        for k in range(k0, len(frames)):
            delay = frame_deadline(anchor, k - k0, opts.frame_s) - opts.clock()
            # Always yield (sleep(0) when behind schedule) so the receiver keeps
            # running even if a send never blocks.
            await opts.sleep(max(0.0, delay))
            if stop_send.is_set():
                return
            await session.send_realtime_input(audio=make_blob(frames[k]))
            state.frames_sent = k + 1
            if not state.first_send_logged:
                state.first_send_logged = True
                events.log("send_start", frame_index=k)
            if state.frames_sent % PROGRESS_EVERY_FRAMES == 0:
                sys.stderr.write(
                    f"probe: frame {state.frames_sent}/{len(frames)} "
                    f"output {len(state.out) / 2 / OUTPUT_SR:.1f}s\n"
                )
        if not state.stream_end_sent and not stop_send.is_set():
            await session.send_realtime_input(audio_stream_end=True)
            state.stream_end_sent = True
            state.sent_all_t = events.now()
            events.log("audio_stream_end", frame_index=state.frames_sent)

    async def receive() -> str:
        try:
            while True:
                got = 0
                # `receive()` ends after each completed interaction -> re-enter.
                async for msg in session.receive():
                    got += 1
                    ga = record_message(msg, state, events)
                    if ga is not None and not go_away_deadline:
                        # Stop feeding this connection at the next frame boundary
                        # but keep receiving its trailing translation.
                        stop_send.set()
                        grace = go_away_grace_s(
                            getattr(ga, "time_left", None), opts.quiet_s
                        )
                        go_away_deadline.append(events.now() + grace)
                        events.log(
                            "go_away_grace",
                            grace_s=grace,
                            frame_index=state.frames_sent,
                        )
                if got == 0:
                    events.log("closed", error="receive ended with no message")
                    return "closed"
        except Exception as e:  # the connection closed / errored: a finding
            events.log("closed", error=_err(e, secret))
            return "closed"

    send_task = asyncio.create_task(send())
    recv_task = asyncio.create_task(receive())
    poll_s = max(0.001, min(0.5, opts.quiet_s / 4))
    reason: str | None = None
    try:
        while reason is None:
            await asyncio.wait({recv_task}, timeout=poll_s)
            now = events.now()
            if recv_task.done():
                closed = recv_task.result()
                reason = "go_away" if go_away_deadline else closed
                break
            if (
                send_task.done()
                and not send_task.cancelled()
                and send_task.exception() is not None
            ):
                events.log("closed", error=_err(send_task.exception(), secret))
                reason = "closed"
                break
            if state.stream_end_sent and state.sent_all_t is not None:
                # Quiet = no VOICED output: the session streams silence.
                last = max(state.last_voiced_t or 0.0, state.sent_all_t)
                if now - last >= opts.quiet_s:
                    events.log("drain_end", reason="quiet")
                    reason = "done"
                    break
                if now - state.sent_all_t >= opts.tail_cap_s:
                    events.log("drain_end", reason="tail_cap")
                    reason = "done"
                    break
            if go_away_deadline and now >= go_away_deadline[0]:
                reason = "go_away"
    finally:
        stop_send.set()
        if not send_task.done():
            # Let an in-flight frame send finish so frames_sent stays exact.
            await asyncio.wait({send_task}, timeout=SEND_STOP_TIMEOUT_S)
        for task in (send_task, recv_task):
            if not task.done():
                task.cancel()
        # Collect both (a cancelled or failed sender is expected here; its error,
        # if any, was already logged above).
        await asyncio.gather(send_task, recv_task, return_exceptions=True)
    return reason


async def run_probe(
    frames: list[bytes],
    connect: Callable[[dict], Any],
    make_blob: Callable[[bytes], Any],
    opts: ProbeOptions,
    events: EventLog,
    state: ProbeState,
    secret: str | None = None,
) -> None:
    """One logical session over `frames`, reconnecting (with the latest
    resumption handle when resumption is on) on GoAway or an early close. A
    refused (re)connect is recorded and ends the probe cleanly."""
    handle: str | None = None
    connections = 0
    while True:
        if connections >= opts.max_connections:
            state.errors.append(f"max connections ({opts.max_connections}) reached")
            events.log("stop", reason="max_connections")
            return
        opened = False
        reason: str | None = None
        try:
            async with connect(live_config(opts, handle)) as session:
                opened = True
                connections += 1
                events.log(
                    "connect",
                    connection=connections,
                    handle_present=handle is not None,
                    frame_index=state.frames_sent,
                )
                sc = getattr(session, "setup_complete", None)
                events.log(
                    "setup_complete",
                    present=sc is not None,
                    session_id=getattr(sc, "session_id", None),
                )
                reason = await _run_connection(
                    session, frames, make_blob, opts, events, state, secret
                )
        except Exception as e:
            err = _err(e, secret)
            if not opened:
                if connections == 0:
                    events.log("connect_failed", error=err)
                else:
                    events.log(
                        "reconnect_failed", error=err, handle_present=handle is not None
                    )
                state.errors.append(err)
                return
            if reason is None:
                raise  # a bug inside the loop, not a connection event: be loud
            events.log("close_error", error=err)
        if not should_reconnect(reason, state.stream_end_sent):
            return
        handle = state.latest_handle if opts.resumption else None
        events.log(
            "reconnect",
            reason=reason,
            handle_present=handle is not None,
            frame_index=state.frames_sent,
            # Input the resumed state may not include (sent after the handle).
            frames_since_handle=(
                state.frames_sent - state.frames_at_handle if handle else None
            ),
        )


# ── the real Live connection + the CLI ─────────────────────────────────────────


async def _run_live(
    args: argparse.Namespace,
    key: str,
    frames: list[bytes],
    opts: ProbeOptions,
    events: EventLog,
    state: ProbeState,
) -> None:
    import google.genai as genai
    from google.genai import types

    if args.api_version:
        client = genai.Client(
            api_key=key, http_options={"api_version": args.api_version}
        )
    else:
        client = genai.Client(api_key=key)

    def connect(cfg: dict) -> Any:
        return client.aio.live.connect(
            model=args.model, config=types.LiveConnectConfig(**cfg)
        )

    def make_blob(frame: bytes) -> Any:
        return types.Blob(data=frame, mime_type=f"audio/pcm;rate={INPUT_SR}")

    await run_probe(frames, connect, make_blob, opts, events, state, secret=key)


def decode_slice(args: argparse.Namespace) -> bytes:
    res = subprocess.run(
        decode_args(args.ffmpeg, args.audio, args.start_s, args.duration_s),
        capture_output=True,
        check=False,
    )
    if res.returncode != 0:
        tail = res.stderr.decode("utf-8", "replace")[-2000:]
        raise RuntimeError(f"ffmpeg decode failed ({res.returncode}): {tail}")
    if not res.stdout:
        raise RuntimeError("ffmpeg decoded an empty slice")
    return res.stdout


def _write_outputs(out_dir: str, state: ProbeState, summary: dict) -> None:
    with wave.open(os.path.join(out_dir, "output.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(OUTPUT_SR)
        w.writeframes(bytes(state.out))
    with open(os.path.join(out_dir, "output_chunks.json"), "w", encoding="utf-8") as f:
        json.dump(place_output(state.chunks, OUTPUT_SR), f)
    for name, parts in (
        ("input_text.txt", state.input_parts),
        ("output_text.txt", state.output_parts),
    ):
        with open(os.path.join(out_dir, name), "w", encoding="utf-8") as f:
            f.write("".join(parts))
    with open(os.path.join(out_dir, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Continuous-session Gemini Live Translate probe (#184 round H)"
    )
    p.add_argument("--audio", required=True, help="any ffmpeg-readable file")
    p.add_argument("--ffmpeg", default="ffmpeg")
    p.add_argument("--start-s", dest="start_s", type=float, default=0.0)
    p.add_argument("--duration-s", dest="duration_s", type=float, default=None)
    p.add_argument(
        "--voice", default="none", help="prebuilt voice name, or none (speaker copy)"
    )
    p.add_argument("--target-lang", dest="target_lang", default="sk")
    p.add_argument("--out-dir", dest="out_dir", required=True)
    p.add_argument("--model", default=MODEL)
    p.add_argument(
        "--api-version",
        dest="api_version",
        choices=["v1alpha", "v1beta"],
        default=None,
        help="default: the SDK default (v1beta in google-genai 2.24.0)",
    )
    p.add_argument(
        "--compression-trigger-tokens", dest="trigger_tokens", type=int, default=25000
    )
    p.add_argument(
        "--compression-target-tokens", dest="target_tokens", type=int, default=8000
    )
    p.add_argument("--no-compression", dest="compression", action="store_false")
    p.add_argument("--no-resumption", dest="resumption", action="store_false")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    key = os.environ.get("GEMINI_API_KEY")
    if not key:
        sys.stderr.write(json.dumps({"error": "GEMINI_API_KEY not set"}) + "\n")
        return 1
    voice = None if args.voice.lower() == "none" else args.voice
    opts = ProbeOptions(
        target_lang=args.target_lang,
        voice=voice,
        compression=args.compression,
        trigger_tokens=args.trigger_tokens,
        target_tokens=args.target_tokens,
        resumption=args.resumption,
    )
    os.makedirs(args.out_dir, exist_ok=True)
    events = EventLog(os.path.join(args.out_dir, "events.jsonl"))
    state = ProbeState()
    frames: list[bytes] = []
    slice_s = 0.0
    failed = False
    try:
        pcm = decode_slice(args)
        slice_s = len(pcm) / 2 / INPUT_SR
        frames = pcm_frames(pcm)
        events.log("decoded", frames=len(frames), slice_s=round(slice_s, 3))
        asyncio.run(_run_live(args, key, frames, opts, events, state))
    except Exception as e:  # loud: recorded in the summary + non-zero exit
        failed = True
        state.errors.append(_err(e, key))
        sys.stderr.write(json.dumps({"error": _err(e, key)}) + "\n")
    finally:
        events.close()
    summary = build_summary(
        events.events,
        model=args.model,
        api_version=args.api_version or "sdk-default",
        voice=voice,
        compression=args.compression,
        resumption=args.resumption,
        slice_s=slice_s,
        frames_sent=state.frames_sent,
        frames_total=len(frames),
        output_pcm=bytes(state.out),
        output_sr=OUTPUT_SR,
        errors=state.errors,
    )
    _write_outputs(args.out_dir, state, summary)
    # ASCII-only: the Windows run redirects stdout to a file in the cp1252
    # locale, where a non-ASCII error text would raise UnicodeEncodeError.
    sys.stdout.write(json.dumps(summary, ensure_ascii=True) + "\n")
    sys.stdout.flush()
    if failed or summary["connections"] == 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
