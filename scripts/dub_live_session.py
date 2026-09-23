#!/usr/bin/env python3
"""dub_live_session.py — ONE logical Gemini Live Translate session over a whole
video's audio (#184 round H step 2, the production dub path).

This is the round-H probe's session loop (`eval/dubbing/live_translate_
continuous_probe.py`, verified on the box: 3 connections via GoAway + resumption
handle, compression past 15 min, output/input 1.0017 — #184 comment 5797563445)
promoted to production, with ONE change the probe itself pointed at: on a GoAway
the input is NOT paused. The probe stopped sending, drained the old connection
for up to 8 s and only then reconnected — its 13 s output gap. Here the sender
keeps feeding the OLD connection while the NEW one opens with the latest
resumption handle, switches at the next frame boundary once it is open, and the
old connection only drains its trailing translation (overlap).

What one run does, given 16 kHz mono s16le 100 ms frames:
  - frame `k` is sent at `t0 + k * FRAME_S` on ONE wall-clock anchor (computed,
    never a summed sleep), so the input is paced at 1.0x and a frame's position
    on the video timeline is its index — even across a reconnect, where the
    frames owed are sent at catch-up speed rather than shifted later;
  - every server message is recorded with a monotonic timestamp: output audio is
    appended to a sink (in memory for tests, a raw file for a real dub) with its
    arrival time and connection, input/output transcriptions are kept, the
    resumption handle of the ACTIVE connection is tracked;
  - `audio_stream_end` is sent once, after the last frame; the run then drains
    until there has been no VOICED output for `quiet_s` (the session streams
    silence after speech), or `tail_cap_s` passes, or every connection closed.
A refused (re)connect or `max_connections` raises `SessionFailed` — the dub child
fails with that error and the Rust worker's backoff retries it.

The network is injected (`connect(cfg)` returns an async context manager
yielding a session with `send_realtime_input` / `receive`, `make_blob(frame)`
wraps a frame) so the whole loop runs in CI against a fake Live server with no
`google-genai` installed. SDK facts (google-genai 2.24.0, read from source, see
the probe docstring): `receive()` ends after each completed interaction so it is
re-entered in a loop; a closed websocket raises; `setup_complete` is consumed by
`connect()` (read `session.setup_complete`); `LiveServerGoAway.time_left` is a
Duration string (`"50s"`).
"""

from __future__ import annotations

import asyncio
import json
import math
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

import numpy as np

MODEL = "gemini-3.5-live-translate-preview"
INPUT_SR = 16000
OUTPUT_SR = 24000
FRAME_S = 0.1
FRAME_BYTES = 3200  # 100 ms @ 16 kHz s16le mono
QUIET_S = 8.0  # no VOICED output this long -> drained
TAIL_CAP_S = 60.0  # hard cap on the drain after audio_stream_end
MAX_CONNECTIONS = 50  # a runaway reconnect loop fails the dub, never runs forever
CONNECT_TIMEOUT_S = 30.0  # a (re)connect that never completes is a refusal
# A send stuck longer than this (websocket flow control that never drains) is a
# dead connection: close it and send the frame on the next one.
SEND_TIMEOUT_S = 30.0
SILENCE_DBFS = -120.0  # floor for an all-zero chunk (JSON has no -inf)
# A chunk louder than this is speech; the session streams (near-)digital silence
# between/after utterances, well below it; speech sits around -30..-15 dBFS.
VOICED_DBFS = -50.0
GO_AWAY_MARGIN_S = 1.0  # stop draining an old connection this long before time_left
PROGRESS_EVERY_FRAMES = 600  # one stderr progress line per minute of input
# `--voice speaker` (the default): NO speech_config — the model speaks in the
# speaker's own voice (owner decision #184 5797691198).
SPEAKER_VOICE = "speaker"

# ── pure helpers (no network; unit-tested) ─────────────────────────────────────


def frame_deadline(t0: float, k: int, frame_s: float = FRAME_S) -> float:
    """Wall-clock send time of frame `k` of a schedule anchored at `t0`.
    Computed, never accumulated — frame 21 600 is still exactly `t0 + 2160 s`."""
    return t0 + k * frame_s


def pcm_frames(pcm: bytes, frame_bytes: int = FRAME_BYTES) -> list[bytes]:
    """Split PCM into fixed frames; the last partial frame is zero-padded."""
    frames = [pcm[i : i + frame_bytes] for i in range(0, len(pcm), frame_bytes)]
    if frames and len(frames[-1]) < frame_bytes:
        frames[-1] = frames[-1] + b"\x00" * (frame_bytes - len(frames[-1]))
    return frames


def voice_for_config(voice: str | None) -> str | None:
    """The prebuilt voice to pin via `speech_config`, or None for the speaker's
    own voice (`speaker`, any case, or blank)."""
    v = (voice or "").strip()
    if not v or v.lower() == SPEAKER_VOICE:
        return None
    return v


def max_gap(arrivals: list[float]) -> float:
    """Largest gap between consecutive arrivals (0.0 if fewer than 2)."""
    ts = sorted(arrivals)
    if len(ts) < 2:
        return 0.0
    return max(b - a for a, b in zip(ts, ts[1:]))


def pcm_dbfs(pcm_s16le: bytes) -> float:
    """RMS level of mono s16le PCM in dBFS, floored at `SILENCE_DBFS`."""
    samples = np.frombuffer(pcm_s16le[: len(pcm_s16le) // 2 * 2], dtype="<i2")
    if samples.size == 0:
        return SILENCE_DBFS
    w = samples.astype(np.float64) / 32768.0
    rms = float(np.sqrt(np.mean(w * w)))
    db = 20.0 * math.log10(rms) if rms > 0 else SILENCE_DBFS
    return round(max(db, SILENCE_DBFS), 2)


def first_voiced_frame(frames: list[bytes]) -> int | None:
    """Index of the first input frame above `VOICED_DBFS` (the input onset), or
    None when the whole input is below it."""
    for k, frame in enumerate(frames):
        if pcm_dbfs(frame) > VOICED_DBFS:
            return k
    return None


def go_away_drain_s(time_left: str | None, default_s: float) -> float:
    """How long an old connection may keep draining after its GoAway: until
    `GO_AWAY_MARGIN_S` before the server's `time_left` (a Duration string such
    as `"50s"`); an unknown `time_left` -> `default_s`."""
    text = (time_left or "").strip()
    if not text.endswith("s"):
        return default_s
    try:
        left = float(text[:-1])
    except ValueError:
        return default_s
    return max(0.0, left - GO_AWAY_MARGIN_S)


def redact(text: str, key: str | None) -> str:
    """Remove the API key from any text (an SDK error may embed the URL)."""
    if not key:
        return text
    return text.replace(key, "<redacted>")


@dataclass
class SessionOptions:
    target_lang: str = "sk"
    voice: str | None = None  # None = the speaker's own voice (no speech_config)
    compression: bool = True
    trigger_tokens: int = 25000
    target_tokens: int = 8000
    resumption: bool = True
    frame_s: float = FRAME_S
    quiet_s: float = QUIET_S
    tail_cap_s: float = TAIL_CAP_S
    max_connections: int = MAX_CONNECTIONS
    connect_timeout_s: float = CONNECT_TIMEOUT_S
    send_timeout_s: float = SEND_TIMEOUT_S
    # The pacer's clock + sleep (injectable so the 1.0x schedule is testable on a
    # virtual clock; the drain / GoAway timers always use the event log's clock).
    clock: Callable[[], float] = time.monotonic
    sleep: Callable[[float], Awaitable[None]] = asyncio.sleep


def live_config(opts: SessionOptions, handle: str | None) -> dict:
    """The `LiveConnectConfig` kwargs (plain dicts, SDK field names) for one
    connection: translation to `target_lang` WITHOUT echoing target-language
    input, both transcriptions, sliding-window compression + resumption (unless
    switched off), and `speech_config` ONLY for a pinned prebuilt voice."""
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


# ── the event log, the output sink, the run state ──────────────────────────────


class EventLog:
    """Every server message kind / session decision with a monotonic `t` (s since
    the log opened); mirrored line-by-line to a JSONL file when a path is given,
    flushed per line so a crash keeps everything seen so far."""

    def __init__(self, path: str | None = None) -> None:
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


class MemorySink:
    """Output PCM kept in memory (tests); `append` returns the byte offset."""

    def __init__(self) -> None:
        self.data = bytearray()

    def append(self, data: bytes) -> int:
        offset = len(self.data)
        self.data.extend(data)
        return offset


class FileSink:
    """Output PCM appended to a raw file (a real dub: a 36-min talk is ~100 MB of
    24 kHz s16le, never held in memory); `append` returns the byte offset."""

    def __init__(self, path: str) -> None:
        self.path = path
        self._fh = open(path, "wb")
        self._size = 0

    def append(self, data: bytes) -> int:
        offset = self._size
        self._fh.write(data)
        self._size += len(data)
        return offset

    def close(self) -> None:
        if not self._fh.closed:
            self._fh.close()


@dataclass
class OutputChunk:
    arrival_s: float  # event-log time the chunk arrived
    conn: int  # 1-based connection it arrived on
    offset: int  # byte offset in the sink
    n_bytes: int
    voiced: bool
    active: bool  # arrived while its connection was the ACTIVE one (not draining)


@dataclass
class SessionState:
    frames_sent: int = 0  # == index of the next unsent frame
    t0_s: float | None = None  # event-log time frame 0 was sent (the timeline zero)
    stream_end_sent: bool = False
    sent_all_t: float | None = None
    last_voiced_t: float | None = None
    latest_handle: str | None = None
    frames_at_handle: int = 0  # frames_sent when the latest handle arrived
    chunks: list[OutputChunk] = field(default_factory=list)
    # Transcriptions carry the connection they arrived on: during an overlap the
    # old connection's trailing text arrives AFTER the new one's first text, but
    # translates EARLIER input — consumers order them by connection.
    input_parts: list[tuple[int, str]] = field(default_factory=list)  # (conn, text)
    output_parts: list[tuple[float, str, int]] = field(default_factory=list)
    drain_end_reason: str | None = None
    errors: list[str] = field(default_factory=list)


class SessionFailed(RuntimeError):
    """The session cannot continue: a refused (re)connect or the connection cap."""


def _err(e: BaseException, secret: str | None) -> str:
    return redact(f"{type(e).__name__}: {e}", secret)


class _Conn:
    """One Live connection: its session, its lifetime task and its drain state."""

    def __init__(self, index: int) -> None:
        self.index = index
        self.session: Any = None
        self.stop = asyncio.Event()
        self.closed = False
        self.close_error: str | None = None
        self.go_away_t: float | None = None
        self.time_left: str | None = None
        self.draining = False
        self.drain_from: float | None = None
        self.drain_deadline: float | None = None
        self.last_voiced_t: float | None = None
        self.task: asyncio.Task | None = None


# ── the session (network-agnostic: `connect` / `make_blob` are injected) ───────


class ContinuousSession:
    """ONE logical Live Translate session over `frames` (see the module doc).
    `log` receives the human progress lines (stderr in the child); `on_progress`
    is called from a supervisor poll ONLY when frames were sent or output arrived
    since the last call (the child's heartbeat — a stuck session must look stuck
    to the Rust stall timeout, never alive just because the loop still ticks)."""

    def __init__(
        self,
        frames: list[bytes],
        connect: Callable[[dict], Any],
        make_blob: Callable[[bytes], Any],
        opts: SessionOptions,
        events: EventLog,
        sink: Any,
        secret: str | None = None,
        log: Callable[[str], None] | None = None,
        on_progress: Callable[[], None] | None = None,
    ) -> None:
        self.frames = frames
        self.connect = connect
        self.make_blob = make_blob
        self.opts = opts
        self.events = events
        self.sink = sink
        self.secret = secret
        self._say = log or (lambda _msg: None)
        self.on_progress = on_progress
        self.state = SessionState()
        self.conns: list[_Conn] = []
        self._active: _Conn | None = None
        self._active_ready = asyncio.Event()
        self._opening: asyncio.Task | None = None
        self._stopping = False
        self._progress_marker: tuple[int, int] = (0, 0)
        # A LOCAL failure while recording (a full disk, a bug) — fatal, never
        # mistaken for a closed websocket (which would reconnect into it again).
        self._fatal: BaseException | None = None

    # ── the supervisor ─────────────────────────────────────────────────────────

    async def run(self) -> SessionState:
        st = self.state
        opts = self.opts
        self._set_active(await self._open(handle=None))
        sender = asyncio.create_task(self._send_all())
        poll_s = max(0.001, min(0.5, opts.quiet_s / 4))
        try:
            while True:
                waiting = {sender} if not sender.done() else set()
                if self._opening is not None:
                    waiting.add(self._opening)
                if waiting:
                    await asyncio.wait(waiting, timeout=poll_s)
                else:
                    await asyncio.sleep(poll_s)
                if self._fatal is not None:
                    raise self._fatal
                marker = (st.frames_sent, len(st.chunks))
                if self.on_progress is not None and marker != self._progress_marker:
                    self._progress_marker = marker
                    self.on_progress()
                if sender.done() and not sender.cancelled() and sender.exception():
                    raise sender.exception()
                if self._opening is not None and self._opening.done():
                    self._opening.result()  # a refused reconnect raises here
                    self._opening = None
                # A safety net: the GoAway / close handlers already started it.
                self._maybe_reconnect()
                now = self.events.now()
                self._end_drained_connections(now)
                reason = self._final_drain_reason(now)
                if reason is not None:
                    st.drain_end_reason = reason
                    self.events.log("drain_end", reason=reason)
                    self._say(f"live: drain end ({reason})")
                    return st
        finally:
            await self._teardown(sender)

    def _maybe_reconnect(self) -> None:
        """Start opening the next connection the moment the active one got a
        GoAway or closed while input is still unsent (called from the GoAway /
        close handlers, and by the supervisor as a safety net). The old
        connection keeps being fed until the new one is open (overlap) — the
        input never pauses."""
        st = self.state
        active = self._active
        if self._stopping or self._fatal is not None:
            return
        # Every frame already sent: a new connection would carry nothing (the
        # old one drains what it got; audio_stream_end is best-effort on it).
        if self._all_frames_sent() or active is None:
            return
        if self._opening is not None:
            return
        if not (active.closed or active.go_away_t is not None):
            return
        reason = "go_away" if active.go_away_t is not None else "closed"
        handle = st.latest_handle if self.opts.resumption else None
        self.events.log(
            "reconnect",
            reason=reason,
            handle_present=handle is not None,
            frame_index=st.frames_sent,
            # Input the resumed state may not include (sent after the handle).
            frames_since_handle=(
                st.frames_sent - st.frames_at_handle if handle else None
            ),
        )
        self._say(
            f"live: reconnect ({reason}) handle_present={handle is not None} "
            f"frame_index={st.frames_sent}"
        )
        self._opening = asyncio.create_task(self._reconnect(handle))

    def _all_frames_sent(self) -> bool:
        return self.state.frames_sent >= len(self.frames)

    async def _reconnect(self, handle: str | None) -> None:
        """Open the next connection and switch to it. A reconnect started just
        before the last frames went out (a GoAway on the last frames) has nothing
        left to carry once they did: its refusal must not fail a fully sent dub,
        and a connection that opens anyway is closed instead of switched to —
        the old one drains."""
        try:
            new = await self._open(handle)
        except SessionFailed as e:
            if not self._all_frames_sent():
                raise  # the supervisor fails the session with it
            self.events.log("reconnect_ignored", reason="all_frames_sent")
            self._say(f"live: reconnect after the last frame failed, ignored: {e}")
            return
        if self._all_frames_sent():
            self.events.log(
                "reconnect_ignored", reason="all_frames_sent", connection=new.index
            )
            self._say(
                f"live: connection {new.index} opened after the last frame, closed"
            )
            new.stop.set()
            return
        self._switch_to(new, self.events.now())

    def _switch_to(self, new: _Conn, now: float) -> None:
        """Make `new` the active connection; the old one (if still open) only
        drains its trailing translation from here on."""
        old = self._active
        self._set_active(new)
        self.events.log(
            "switch", connection=new.index, frame_index=self.state.frames_sent
        )
        self._say(
            f"live: switch to connection {new.index} at frame {self.state.frames_sent}"
        )
        if old is not None and not old.closed:
            old.draining = True
            old.drain_from = now
            start = old.go_away_t if old.go_away_t is not None else now
            old.drain_deadline = start + go_away_drain_s(
                old.time_left, self.opts.quiet_s
            )

    def _end_drained_connections(self, now: float) -> None:
        for c in self.conns:
            if not c.draining or c.closed or c.stop.is_set():
                continue
            last = max(c.last_voiced_t or 0.0, c.drain_from or 0.0)
            deadline_hit = c.drain_deadline is not None and now >= c.drain_deadline
            if deadline_hit or now - last >= self.opts.quiet_s:
                why = "deadline" if deadline_hit else "quiet"
                self.events.log("drained", connection=c.index, reason=why)
                self._say(f"live: connection {c.index} drained ({why})")
                c.stop.set()

    def _final_drain_reason(self, now: float) -> str | None:
        st = self.state
        if not st.stream_end_sent or st.sent_all_t is None:
            return None
        if all(c.closed for c in self.conns):
            return "closed"
        last = max(st.last_voiced_t or 0.0, st.sent_all_t)
        if now - last >= self.opts.quiet_s:
            return "quiet"
        if now - st.sent_all_t >= self.opts.tail_cap_s:
            return "tail_cap"
        return None

    async def _teardown(self, sender: asyncio.Task) -> None:
        self._stopping = True  # closing connections must not start a reconnect
        # Python 3.11's `wait_for` can swallow a cancellation that lands as its
        # inner send completes; the sender would then wait on `_active_ready`
        # forever. Waking it makes it see `_stopping` and stop on its own.
        self._active_ready.set()
        if not sender.done():
            sender.cancel()
        if self._opening is not None and not self._opening.done():
            self._opening.cancel()
        for c in self.conns:
            c.stop.set()
        tasks = [sender] + [c.task for c in self.conns if c.task is not None]
        if self._opening is not None:
            tasks.append(self._opening)
        await asyncio.gather(*tasks, return_exceptions=True)

    # ── connections ────────────────────────────────────────────────────────────

    def _not_ready(self) -> None:
        """No open active connection to send on — unless tearing down, where the
        event stays set so a sender whose cancellation was swallowed wakes, sees
        `_stopping` and stops (see `_teardown`)."""
        if not self._stopping:
            self._active_ready.clear()

    def _set_active(self, conn: _Conn) -> None:
        self._active = conn
        if conn.closed:
            self._not_ready()
        else:
            self._active_ready.set()

    async def _open(self, handle: str | None) -> _Conn:
        """Open connection n+1 (with `handle` when resuming). A refusal, a
        connect that never completes, or the connection cap -> SessionFailed."""
        n = len(self.conns) + 1
        if n > self.opts.max_connections:
            msg = f"max connections ({self.opts.max_connections}) reached"
            self.events.log("stop", reason="max_connections")
            raise SessionFailed(msg)
        conn = _Conn(n)
        cfg = live_config(self.opts, handle)
        opened: asyncio.Future = asyncio.get_running_loop().create_future()
        conn.task = asyncio.create_task(self._conn_main(conn, cfg, opened))
        self.conns.append(conn)
        try:
            await asyncio.wait_for(asyncio.shield(opened), self.opts.connect_timeout_s)
        except Exception as e:  # refused / timed out: the dub cannot continue
            conn.task.cancel()
            await asyncio.gather(conn.task, return_exceptions=True)
            conn.closed = True
            if isinstance(e, (asyncio.TimeoutError, TimeoutError)):
                err = f"did not complete within {self.opts.connect_timeout_s:g} s"
            else:
                err = _err(e, self.secret)
            kind = "connect_failed" if n == 1 else "reconnect_failed"
            self.events.log(kind, error=err, handle_present=handle is not None)
            self.state.errors.append(err)
            what = "connect" if n == 1 else f"reconnect (connection {n})"
            raise SessionFailed(f"Live {what} refused: {err}") from None
        sc = getattr(conn.session, "setup_complete", None)
        self.events.log(
            "connect",
            connection=n,
            handle_present=handle is not None,
            frame_index=self.state.frames_sent,
            session_id=getattr(sc, "session_id", None),
        )
        self._say(
            f"live: connect {n} (handle_present={handle is not None}, "
            f"frame {self.state.frames_sent})"
        )
        return conn

    async def _conn_main(self, conn: _Conn, cfg: dict, opened: asyncio.Future) -> None:
        """A connection's whole lifetime: open, receive until the server closes it
        or `conn.stop` is set, then leave the context (closing it)."""
        try:
            async with self.connect(cfg) as session:
                conn.session = session
                opened.set_result(conn)
                recv = asyncio.create_task(self._receive(conn))
                stop = asyncio.create_task(conn.stop.wait())
                await asyncio.wait({recv, stop}, return_when=asyncio.FIRST_COMPLETED)
                if recv.done() and not recv.cancelled():
                    local = recv.exception()
                    if local is not None:
                        # Raised by `_record` (our side), not by the transport.
                        self._fatal = self._fatal or local
                    else:
                        conn.close_error = recv.result()
                for t in (recv, stop):
                    if not t.done():
                        t.cancel()
                await asyncio.gather(recv, stop, return_exceptions=True)
        except Exception as e:
            if not opened.done():
                opened.set_exception(e)
                return
            conn.close_error = conn.close_error or _err(e, self.secret)
        finally:
            self._mark_closed(conn)

    def _mark_closed(self, conn: _Conn, error: str | None = None) -> None:
        if error and not conn.close_error:
            conn.close_error = error
        if conn.closed:
            return
        conn.closed = True
        self.events.log("closed", connection=conn.index, error=conn.close_error)
        if conn is self._active:
            self._not_ready()
            if conn.close_error and not self.state.stream_end_sent:
                self._say(f"live: connection {conn.index} closed: {conn.close_error}")
            self._maybe_reconnect()

    async def _receive(self, conn: _Conn) -> str | None:
        """Receive until the connection closes; returns the close reason. The SDK's
        `receive()` ends after each completed interaction -> re-entered. Only the
        TRANSPORT is guarded: an exception from `_record` (a full disk, a bug)
        propagates, so it fails the session instead of posing as a close."""
        while True:
            got = 0
            messages = conn.session.receive().__aiter__()
            while True:
                try:
                    msg = await messages.__anext__()
                except StopAsyncIteration:
                    break
                except Exception as e:  # the websocket closed / errored
                    return _err(e, self.secret)
                got += 1
                self._record(msg, conn)
            if got == 0:
                return "receive ended with no message"

    def _record(self, msg: Any, conn: _Conn) -> None:
        st = self.state
        now = self.events.now()
        data = getattr(msg, "data", None)
        if data:
            db = pcm_dbfs(data)
            voiced = db > VOICED_DBFS
            offset = self.sink.append(data)
            st.chunks.append(
                OutputChunk(
                    arrival_s=round(now, 4),
                    conn=conn.index,
                    offset=offset,
                    n_bytes=len(data),
                    voiced=voiced,
                    active=not conn.draining,
                )
            )
            if voiced:
                conn.last_voiced_t = now
                st.last_voiced_t = now
            self.events.log(
                "audio",
                connection=conn.index,
                n_bytes=len(data),
                dbfs=db,
                voiced=voiced,
            )
        sc = getattr(msg, "server_content", None)
        if sc is not None:
            it = getattr(sc, "input_transcription", None)
            if it is not None and getattr(it, "text", None):
                st.input_parts.append((conn.index, it.text))
                self.events.log(
                    "input_transcription", connection=conn.index, text=it.text
                )
            ot = getattr(sc, "output_transcription", None)
            if ot is not None and getattr(ot, "text", None):
                st.output_parts.append((round(now, 4), ot.text, conn.index))
                self.events.log(
                    "output_transcription", connection=conn.index, text=ot.text
                )
            for flag in ("turn_complete", "generation_complete", "interrupted"):
                if getattr(sc, flag, None):
                    self.events.log(flag, connection=conn.index)
        sru = getattr(msg, "session_resumption_update", None)
        if sru is not None:
            handle = getattr(sru, "new_handle", None)
            # Only the ACTIVE connection's state is resumed from: a draining
            # connection's handle would rewind the session to before the switch.
            if handle and conn is self._active and not conn.draining:
                st.latest_handle = handle
                st.frames_at_handle = st.frames_sent
            self.events.log(
                "session_resumption_update",
                connection=conn.index,
                handle_present=bool(handle),
                resumable=getattr(sru, "resumable", None),
            )
        ga = getattr(msg, "go_away", None)
        if ga is not None:
            time_left = getattr(ga, "time_left", None)
            text = None if time_left is None else str(time_left)
            self.events.log(
                "go_away",
                connection=conn.index,
                time_left=text,
                frame_index=st.frames_sent,
            )
            if conn.go_away_t is None:
                conn.go_away_t = now
                conn.time_left = text
                self._say(
                    f"live: go_away time_left {text} on connection {conn.index} "
                    f"at frame {st.frames_sent}"
                )
                if conn is self._active:
                    self._maybe_reconnect()  # open the next one NOW, keep sending

    # ── the sender ─────────────────────────────────────────────────────────────

    async def _send_to_active(self, **kwargs: Any) -> None:
        """Send one realtime-input message to the ACTIVE connection, waiting for a
        (re)opened one when it closed; a failed send — or one stuck longer than
        `send_timeout_s` — marks it closed and retries on the next connection."""
        timeout = self.opts.send_timeout_s
        while True:
            await self._active_ready.wait()
            if self._stopping:
                raise asyncio.CancelledError  # see `_teardown`
            conn = self._active
            if conn is None or conn.closed:
                self._not_ready()
                continue
            try:
                await asyncio.wait_for(
                    conn.session.send_realtime_input(**kwargs), timeout
                )
                return
            except (asyncio.TimeoutError, TimeoutError):
                self._mark_closed(conn, f"send timed out after {timeout:g} s")
                conn.stop.set()  # leave the stuck connection's context
            except Exception as e:  # the websocket died under this send
                self._mark_closed(conn, _err(e, self.secret))
                conn.stop.set()

    async def _send_all(self) -> None:
        st = self.state
        opts = self.opts
        total = len(self.frames)
        anchor: float | None = None
        for k in range(total):
            await self._active_ready.wait()
            if self._stopping:
                return
            if anchor is None:
                anchor = opts.clock()  # ONE anchor for the whole run
            delay = frame_deadline(anchor, k, opts.frame_s) - opts.clock()
            # Always yield (sleep(0) when behind schedule) so the receivers and
            # the supervisor keep running even if a send never blocks.
            await opts.sleep(max(0.0, delay))
            await self._send_to_active(audio=self.make_blob(self.frames[k]))
            st.frames_sent = k + 1
            if st.t0_s is None:
                st.t0_s = self.events.now()
                self.events.log("send_start", frame_index=k)
            if st.frames_sent % PROGRESS_EVERY_FRAMES == 0:
                out_s = sum(c.n_bytes for c in st.chunks if c.active) / 2 / OUTPUT_SR
                self._say(f"live: frame {st.frames_sent}/{total} output {out_s:.1f}s")
        # Every frame is sent: no reconnect starts from here on, so the stream
        # end goes to the active connection once, best-effort — a connection
        # that died after the last frame has nothing left to receive it for.
        await self._end_stream()
        st.stream_end_sent = True
        st.sent_all_t = self.events.now()
        self.events.log("audio_stream_end", frame_index=st.frames_sent)
        self._say(f"live: audio_stream_end after frame {st.frames_sent}/{total}")

    async def _end_stream(self) -> None:
        conn = self._active
        if conn is None or conn.closed:
            self.events.log("stream_end_skipped", reason="no open connection")
            return
        try:
            await asyncio.wait_for(
                conn.session.send_realtime_input(audio_stream_end=True),
                self.opts.send_timeout_s,
            )
        except Exception as e:  # the drain decides; nothing left to resend
            self.events.log("stream_end_failed", error=_err(e, self.secret))


def build_summary(state: SessionState, events: list[dict], input_s: float) -> dict:
    """The session's one-line result from the event log + the output chunks.

    `output_to_input_ratio` counts the output of the ACTIVE connection plus the
    VOICED trailing translation of a draining one: the silence a draining
    connection keeps streaming while the new one already streams is the overlap,
    not extra dub, so it would inflate the ratio by ~`quiet_s` per reconnect."""

    def count(kind: str) -> int:
        return sum(1 for e in events if e["kind"] == kind)

    chunks = state.chunks
    stream_bytes = sum(c.n_bytes for c in chunks if c.active or c.voiced)
    output_s = stream_bytes / 2 / OUTPUT_SR
    voiced_t = [c.arrival_s for c in chunks if c.voiced]
    audio_t = [c.arrival_s for c in chunks]

    def latency(ts: list[float]) -> float | None:
        if not ts or state.t0_s is None:
            return None
        return round(min(ts) - state.t0_s, 3)

    return {
        "input_s": round(input_s, 3),
        "frames_sent": state.frames_sent,
        "connections": count("connect"),
        "reconnects": count("reconnect"),
        "go_aways": count("go_away"),
        "resumptions_offered": sum(
            1
            for e in events
            if e["kind"] == "session_resumption_update" and e.get("handle_present")
        ),
        "output_audio_s": round(output_s, 3),
        "output_to_input_ratio": round(output_s / input_s, 4) if input_s > 0 else None,
        "overlap_output_s": round(
            sum(c.n_bytes for c in chunks if not c.active) / 2 / OUTPUT_SR, 3
        ),
        "max_output_gap_s": round(max_gap(audio_t), 3),
        "max_voiced_gap_s": round(max_gap(voiced_t), 3),
        "voiced_output_s": round(
            sum(c.n_bytes for c in chunks if c.voiced) / 2 / OUTPUT_SR, 3
        ),
        "first_output_latency_s": latency(audio_t),
        "first_voiced_latency_s": latency(voiced_t),
        "drain_end_reason": state.drain_end_reason,
        "errors": list(state.errors),
    }
