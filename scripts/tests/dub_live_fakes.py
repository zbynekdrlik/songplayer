"""Test doubles for the #184 round-H step-2 continuous dub session tests.

The Gemini Live API is an external network service — the ONLY thing faked here
(adapted from the round-H probe's `eval/dubbing/tests/live_fakes.py`; eval and
production stay decoupled, so this is a copy, not an import). `FakeServer`
scripts one behaviour per connection; `FakeSession` mimics the SDK session
(`send_realtime_input`, a `receive()` that ends after a `turn_complete` and
raises on a closed websocket). `VirtualClock` drives the pacer
deterministically. `open_after_frames` holds a (re)connect open until the
server has received N more frames — the proof that the OLD connection keeps
being fed while the new one opens.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import sys
import time
from types import SimpleNamespace

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)

import dub_live_session as dls  # noqa: E402

RUN_TIMEOUT_S = 60.0


def session_opts(**kw) -> dls.SessionOptions:
    return dls.SessionOptions(**kw)


def fast_opts(**kw) -> dls.SessionOptions:
    base = {"frame_s": 0.001, "quiet_s": 0.05, "tail_cap_s": 2.0}
    base.update(kw)
    return dls.SessionOptions(**base)


def live_msg(**kw):
    base = {
        "data": None,
        "server_content": None,
        "go_away": None,
        "session_resumption_update": None,
        "usage_metadata": None,
    }
    base.update(kw)
    return SimpleNamespace(**base)


def live_content(**kw):
    base = {
        "input_transcription": None,
        "output_transcription": None,
        "turn_complete": None,
        "generation_complete": None,
        "interrupted": None,
    }
    base.update(kw)
    return SimpleNamespace(**base)


def live_resumption(handle):
    return live_msg(
        session_resumption_update=SimpleNamespace(
            new_handle=handle,
            resumable=handle is not None,
            last_consumed_client_message_index=None,
        )
    )


def go_away(time_left="50s"):
    return live_msg(go_away=SimpleNamespace(time_left=time_left))


CLOSE = object()


class RealClock:
    now = staticmethod(time.monotonic)
    sleep = staticmethod(asyncio.sleep)


class VirtualClock:
    """A deterministic clock for the pacer: `sleep(d)` advances virtual time by
    exactly `d` (and yields once), so pacing assertions are exact."""

    def __init__(self) -> None:
        self.t = 1000.0

    def now(self) -> float:
        return self.t

    async def sleep(self, d: float) -> None:
        self.t += d
        await asyncio.sleep(0)


class FakeSession:
    """One fake Live connection. `on_frame(session, n)` reacts to the n-th frame
    (1-based, per connection) by queueing server messages; `CLOSE` in the queue
    makes `receive()` raise like the SDK does on a closed websocket."""

    def __init__(self, server, on_frame, index):
        self.server = server
        self.on_frame = on_frame
        self.index = index  # 1-based connection number
        self.queue: asyncio.Queue = asyncio.Queue()
        self.frames: list[bytes] = []
        self.send_times: list[float] = []
        self.global_indices: list[int] = []  # position in the server's frame log
        self.stream_end = 0
        self.setup_complete = SimpleNamespace(session_id=f"s{index}")

    async def send_realtime_input(self, *, audio=None, audio_stream_end=None):
        if audio_stream_end:
            self.stream_end += 1
            self.server.stream_ends += 1
            if self.server.on_stream_end is not None:
                self.server.on_stream_end(self)
            return
        n = len(self.frames) + 1
        if self.server.fail_at.get(self.index) == n:
            raise ConnectionError("send failed: websocket closed 1006")
        if self.server.hang_at.get(self.index) == n:
            # A send stuck on flow control: never returns, the frame never lands.
            await asyncio.Event().wait()
        self.send_times.append(self.server.clock.now())
        self.global_indices.append(len(self.server.all_frames))
        self.frames.append(audio)
        self.server.all_frames.append(audio)
        self.server.frame_conn.append(self.index)
        if self.on_frame is not None:
            self.on_frame(self, n)

    async def receive(self):
        while True:
            m = await self.queue.get()
            if m is CLOSE:
                raise ConnectionError("websocket closed 1011")
            yield m
            sc = m.server_content
            if sc is not None and sc.turn_complete:
                break


class FakeServer:
    """A scripted Live service: one `on_frame` behaviour per connection, or the
    string "refuse" to make that connect attempt fail. `open_after_frames[n]`
    keeps connection n's connect pending until the server has received that
    many MORE frames (on the still-active old connection). `connect_delay_s`
    (real seconds) delays every RE-connect; `connect_advance_s` moves the
    VIRTUAL clock by that much on every re-connect (the time a reconnect takes,
    as the pacer sees it); `hang_at[n] = k` makes the k-th send on connection n
    never return."""

    def __init__(
        self,
        scripts,
        on_stream_end=None,
        clock=None,
        fail_at=None,
        open_after_frames=None,
        connect_delay_s=0.0,
        connect_advance_s=0.0,
        hang_at=None,
        open_after_stream_end=False,
        never_open=False,
    ):
        self.scripts = list(scripts)
        self.on_stream_end = on_stream_end
        self.clock = clock or RealClock()
        self.fail_at = fail_at or {}
        self.open_after_frames = open_after_frames or {}
        self.connect_delay_s = connect_delay_s
        self.connect_advance_s = connect_advance_s
        self.hang_at = hang_at or {}
        # Every connect (the FIRST included) stays pending forever.
        self.never_open = never_open
        self.connects_cancelled = 0
        # Hold every RE-connect until audio_stream_end arrived (deterministic
        # "a reconnect still in flight at the stream end", no real-clock race).
        self.open_after_stream_end = open_after_stream_end
        self.configs: list[dict] = []
        self.sessions: list[FakeSession] = []
        self.all_frames: list[bytes] = []
        self.frame_conn: list[int] = []  # which connection got each frame
        self.stream_ends = 0

    def connect(self, cfg):
        self.configs.append(cfg)
        n = len(self.configs)
        script = self.scripts[n - 1]
        wait_frames = self.open_after_frames.get(n, 0)

        @contextlib.asynccontextmanager
        async def cm():
            if self.never_open:
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    self.connects_cancelled += 1
                    raise
            if n > 1 and self.connect_delay_s:
                await asyncio.sleep(self.connect_delay_s)
            if n > 1 and self.connect_advance_s:
                self.clock.t += self.connect_advance_s
            if n > 1 and self.open_after_stream_end:
                while self.stream_ends < 1:
                    await asyncio.sleep(0.001)
            if wait_frames:
                target = len(self.all_frames) + wait_frames
                while len(self.all_frames) < target:
                    await asyncio.sleep(0)
            if script == "refuse":
                raise ConnectionError("handle rejected: 1008 policy violation")
            s = FakeSession(self, script, n)
            self.sessions.append(s)
            yield s

        return cm()


def pcm_frame_list(n: int) -> list[bytes]:
    """`n` 100 ms frames of a loud tone-ish pattern (voiced input)."""
    return [bytes([i % 200 + 20, 0x40]) * 1600 for i in range(n)]


def echo_frame(session, n):
    # Each input frame yields 4800 bytes (100 ms @ 24 kHz) of VOICED output audio.
    session.queue.put_nowait(live_msg(data=b"\x00\x10" * 2400))


def run_fake(server, frames, opts, redact_word=None, sink=None, on_progress=None):
    events = dls.EventLog(None)
    sink = sink if sink is not None else dls.MemorySink()
    lines: list[str] = []
    extra = {} if on_progress is None else {"on_progress": on_progress}
    session = dls.ContinuousSession(
        frames,
        server.connect,
        lambda b: b,
        opts,
        events,
        sink,
        secret=redact_word,
        log=lines.append,
        **extra,
    )
    # A loop that never ends is a FAILURE, never a hung CI job.
    state = asyncio.run(asyncio.wait_for(session.run(), timeout=RUN_TIMEOUT_S))
    return events, state, sink, lines
