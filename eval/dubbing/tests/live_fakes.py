"""Test doubles for the #184 round-H continuous-session probe tests.

The Gemini Live API is an external network service — the ONLY thing faked
here. `FakeServer` scripts one behaviour per connection; `FakeSession` mimics
the SDK session (`send_realtime_input`, a `receive()` that ends after a
`turn_complete` and raises on a closed websocket). `VirtualClock` drives the
pacer deterministically.
"""

from __future__ import annotations

import asyncio
import contextlib
import time
from types import SimpleNamespace

from eval.dubbing import live_translate_continuous_probe as probe


def probe_opts(**kw) -> probe.ProbeOptions:
    base = {
        "target_lang": "sk",
        "voice": None,
        "compression": True,
        "trigger_tokens": 25000,
        "target_tokens": 8000,
        "resumption": True,
    }
    base.update(kw)
    return probe.ProbeOptions(**base)


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


CLOSE = object()


class FakeSession:
    """One fake Live connection. `on_frame(session, n)` reacts to the n-th frame
    (1-based, per connection) by queueing server messages; `CLOSE` in the queue
    makes `receive()` raise like the SDK does on a closed websocket. Like the
    SDK, `receive()` ends its iteration after a `turn_complete` message."""

    def __init__(self, server, on_frame, index):
        self.server = server
        self.on_frame = on_frame
        self.index = index  # 1-based connection number
        self.opened_at = server.clock.now()
        self.queue: asyncio.Queue = asyncio.Queue()
        self.frames: list[bytes] = []
        self.send_times: list[float] = []  # monotonic time each frame arrived
        self.stream_end = 0
        self.setup_complete = SimpleNamespace(session_id="s")

    async def send_realtime_input(self, *, audio=None, audio_stream_end=None):
        if audio_stream_end:
            self.stream_end += 1
            self.server.stream_ends += 1
            if self.server.on_stream_end is not None:
                self.server.on_stream_end(self)
            return
        n = len(self.frames) + 1
        if self.server.fail_at.get(self.index) == n:
            # The websocket died under this send: the frame never arrived.
            raise ConnectionError("send failed: websocket closed 1006")
        self.send_times.append(self.server.clock.now())
        self.frames.append(audio)
        self.server.all_frames.append(audio)
        if self.on_frame is not None:
            self.on_frame(self, n)
        slow = self.server.slow_at.get(self.index)
        if slow is not None and slow[0] == n:
            # A frame the server already has, but whose send has not returned.
            await asyncio.sleep(slow[1])
        if self.server.send_latency_s:
            # A real websocket send takes time; a fixed-sleep pacer would add it
            # on top of every frame period, the drift-corrected one must not.
            await self.server.clock.sleep(self.server.send_latency_s)

    async def receive(self):
        while True:
            m = await self.queue.get()
            if m is CLOSE:
                raise ConnectionError("websocket closed 1011")
            yield m
            sc = m.server_content
            if sc is not None and sc.turn_complete:
                break


class RealClock:
    now = staticmethod(time.monotonic)
    sleep = staticmethod(asyncio.sleep)


class VirtualClock:
    """A deterministic clock for the pacer: `sleep(d)` advances virtual time by
    exactly `d` (and yields once), so pacing assertions are exact and immune to
    a loaded test runner."""

    def __init__(self) -> None:
        self.t = 1000.0

    def now(self) -> float:
        return self.t

    async def sleep(self, d: float) -> None:
        self.t += d
        await asyncio.sleep(0)


class FakeServer:
    """A scripted Live service: one `on_frame` behaviour per connection, or the
    string "refuse" to make that connect attempt fail. Send latency and the
    reconnect delay run on `clock` (real by default, virtual for pacing)."""

    def __init__(
        self,
        scripts,
        send_latency_s=0.0,
        reconnect_delay_s=0.0,
        on_stream_end=None,
        clock=None,
        fail_at=None,
        slow_at=None,
    ):
        self.scripts = list(scripts)
        self.on_stream_end = on_stream_end
        self.clock = clock or RealClock()
        self.fail_at = fail_at or {}  # {connection: frame n that raises}
        self.slow_at = slow_at or {}  # {connection: (frame n, seconds)}
        self.configs: list[dict] = []
        self.sessions: list[FakeSession] = []
        self.all_frames: list[bytes] = []
        self.stream_ends = 0
        self.send_latency_s = send_latency_s
        self.reconnect_delay_s = reconnect_delay_s

    def connect(self, cfg):
        self.configs.append(cfg)
        script = self.scripts[len(self.configs) - 1]
        delay = self.reconnect_delay_s if len(self.configs) > 1 else 0.0

        @contextlib.asynccontextmanager
        async def cm():
            if delay:
                await self.clock.sleep(delay)
            if script == "refuse":
                raise ConnectionError("handle rejected: 1008 policy violation")
            s = FakeSession(self, script, len(self.configs))
            self.sessions.append(s)
            yield s

        return cm()


def pcm_frame_list(n: int) -> list[bytes]:
    return [bytes([i]) * 3200 for i in range(n)]


def fast_opts(**kw) -> probe.ProbeOptions:
    return probe_opts(frame_s=0.001, quiet_s=0.05, tail_cap_s=2.0, **kw)


def run_fake(server, frames, opts, redact_word=None):
    events = probe.EventLog(None)
    state = probe.ProbeState()
    probe_run = probe.run_probe(
        frames,
        server.connect,
        lambda b: b,
        opts,
        events,
        state,
        secret=redact_word,
    )
    # A loop that never ends is a FAILURE, never a hung CI job.
    asyncio.run(asyncio.wait_for(probe_run, timeout=RUN_TIMEOUT_S))
    return events, state


RUN_TIMEOUT_S = 60.0


def echo_frame(session, n):
    # Each input frame yields 4800 bytes (100 ms @ 24 kHz) of output audio.
    session.queue.put_nowait(live_msg(data=b"\x10" * 4800))
