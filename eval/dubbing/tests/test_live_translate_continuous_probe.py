"""Tests for the #184 round-H continuous-session probe
(`eval/dubbing/live_translate_continuous_probe.py`).

The pure helpers (pacing, framing, reconnect decision, gap / RMS / placement /
summary math, redaction, the Live config and the ffmpeg decode args) are tested
directly. The session loop (`run_probe`) is driven against a FAKE Live server —
the Gemini Live API is an external network service, the only thing faked here —
so the reconnect / resumption-handle / resend-from-next-frame logic is proven
with no network and no google-genai installed.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import math
import pathlib
import struct
import subprocess
import sys
import time
from types import SimpleNamespace

import pytest

from eval.dubbing import live_translate_continuous_probe as probe

# ── pure helpers ────────────────────────────────────────────────────────────────


def test_module_imports_without_google_genai():
    # The SDK import is lazy (inside the connection function only): with any
    # `google` import made impossible, the module still imports.
    code = (
        "import sys; sys.modules['google'] = None; "
        "from eval.dubbing import live_translate_continuous_probe as p; "
        "assert p.frame_deadline(0.0, 1) == 0.1"
    )
    root = pathlib.Path(__file__).resolve().parents[3]
    res = subprocess.run(
        [sys.executable, "-c", code], cwd=root, capture_output=True, text=True
    )
    assert res.returncode == 0, res.stderr
    assert not hasattr(probe, "genai")
    assert not hasattr(probe, "types")


def test_frame_deadline_is_drift_free_over_15000_frames():
    t0 = 1234.5678
    for k in range(15001):
        assert abs(probe.frame_deadline(t0, k) - (t0 + k * 0.1)) < 1e-9


def test_frame_deadline_does_not_accumulate_like_a_summed_sleep():
    # Summing 0.1 15 000 times drifts (float accumulation, ~2.7e-10 here); the
    # deadline is the single product k*frame_s, bit-exact — an accumulating
    # schedule cannot hit it.
    acc = 0.0
    for _ in range(15000):
        acc += 0.1
    assert acc != 15000 * 0.1
    assert probe.frame_deadline(0.0, 15000) == 15000 * 0.1
    assert probe.frame_deadline(2.0, 12345) == 2.0 + 12345 * 0.1


def test_frame_deadline_honours_custom_frame_length():
    assert probe.frame_deadline(10.0, 4, frame_s=0.02) == pytest.approx(10.08)
    assert probe.frame_deadline(10.0, 0) == 10.0


def test_pcm_frames_splits_exact_multiples_without_padding():
    pcm = bytes(range(256)) * 25  # 6400 bytes = exactly 2 frames
    frames = probe.pcm_frames(pcm)
    assert len(frames) == 2
    assert all(len(f) == 3200 for f in frames)
    assert b"".join(frames) == pcm


def test_pcm_frames_zero_pads_the_last_partial_frame():
    pcm = b"\x01" * (3200 * 2 + 100)
    frames = probe.pcm_frames(pcm)
    assert len(frames) == 3
    assert frames[2] == b"\x01" * 100 + b"\x00" * 3100
    assert len(frames[2]) == 3200


def test_pcm_frames_empty_input_is_no_frames():
    assert probe.pcm_frames(b"") == []


def test_pcm_frames_custom_frame_size():
    frames = probe.pcm_frames(b"\x02" * 5, frame_bytes=2)
    assert frames == [b"\x02\x02", b"\x02\x02", b"\x02\x00"]


@pytest.mark.parametrize(
    ("kind", "sent_all", "expected"),
    [
        ("go_away", False, True),
        ("go_away", True, True),
        ("closed", False, True),
        ("closed", True, False),
        ("done", False, False),
        ("done", True, False),
    ],
)
def test_should_reconnect(kind, sent_all, expected):
    assert probe.should_reconnect(kind, sent_all) is expected


def test_max_gap_empty_and_single_are_zero():
    assert probe.max_gap([]) == 0.0
    assert probe.max_gap([5.0]) == 0.0


def test_max_gap_is_the_largest_consecutive_gap():
    assert probe.max_gap([1.0, 1.5, 4.0, 4.1]) == pytest.approx(2.5)
    # Order-independent (arrivals are sorted first).
    assert probe.max_gap([4.1, 1.0, 4.0, 1.5]) == pytest.approx(2.5)


def _s16(samples: list[int]) -> bytes:
    return struct.pack(f"<{len(samples)}h", *samples)


def test_rms_windows_constant_then_silence():
    sr = 1000
    pcm = _s16([16384] * sr + [0] * sr)  # 1 s at 0.5 FS, then 1 s silence
    got = probe.rms_windows(pcm, sr, 1.0)
    assert len(got) == 2
    assert got[0] == pytest.approx(20 * math.log10(16384 / 32768), abs=0.05)
    assert got[1] == probe.SILENCE_DBFS


def test_rms_windows_sine_is_amplitude_over_root_two():
    sr = 8000
    amp = 10000
    pcm = _s16([int(amp * math.sin(2 * math.pi * 440 * i / sr)) for i in range(sr)])
    (db,) = probe.rms_windows(pcm, sr, 1.0)
    want = 20 * math.log10((amp / math.sqrt(2)) / 32768)
    assert db == pytest.approx(want, abs=0.1)


def test_rms_windows_keeps_the_partial_last_window_and_empty_is_empty():
    sr = 100
    pcm = _s16([1000] * 250)  # 2.5 windows of 1 s
    assert len(probe.rms_windows(pcm, sr, 1.0)) == 3
    assert probe.rms_windows(b"", sr, 1.0) == []


def test_place_output_offsets_are_monotonic_cumulative_buffer_positions():
    sr = 24000
    chunks = [(0.5, 4800), (0.6, 9600), (3.0, 480)]
    placed = probe.place_output(chunks, sr)
    assert [p[0] for p in placed] == [0.5, 0.6, 3.0]
    assert [p[1] for p in placed] == [4800, 9600, 480]
    assert [p[2] for p in placed] == pytest.approx([0.0, 0.1, 0.3])
    offsets = [p[2] for p in placed]
    assert offsets == sorted(offsets)


def test_pcm_dbfs_levels():
    assert probe.pcm_dbfs(_s16([16384] * 100)) == pytest.approx(-6.02, abs=0.05)
    assert probe.pcm_dbfs(_s16([0] * 100)) == probe.SILENCE_DBFS
    assert probe.pcm_dbfs(b"") == probe.SILENCE_DBFS


def test_voiced_threshold_separates_speech_from_streamed_silence():
    assert probe.pcm_dbfs(_s16([3000, -3000] * 50)) > probe.VOICED_DBFS
    assert probe.pcm_dbfs(_s16([30, -30] * 50)) < probe.VOICED_DBFS


@pytest.mark.parametrize(
    ("time_left", "quiet_s", "expected"),
    [
        ("10s", 8.0, 8.0),  # capped at the quiet window
        ("3s", 8.0, 2.0),  # leave 1 s before the server drops us
        ("2.500s", 8.0, 1.5),
        ("0.5s", 8.0, 0.0),
        (None, 8.0, 8.0),  # unknown -> the quiet window
        ("junk", 8.0, 8.0),
    ],
)
def test_go_away_grace(time_left, quiet_s, expected):
    assert probe.go_away_grace_s(time_left, quiet_s) == pytest.approx(expected)


def test_redact_replaces_the_key_everywhere():
    assert probe.redact("key=abc123 again abc123", "abc123") == (
        "key=<redacted> again <redacted>"
    )


def test_redact_without_a_key_is_identity():
    assert probe.redact("nothing secret", None) == "nothing secret"
    assert probe.redact("nothing secret", "") == "nothing secret"


def _opts(**kw) -> probe.ProbeOptions:
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


def test_live_config_default_arm_is_speaker_copy_with_compression_and_resumption():
    cfg = probe.live_config(_opts(), handle=None)
    assert cfg["response_modalities"] == ["AUDIO"]
    assert cfg["translation_config"] == {
        "target_language_code": "sk",
        "echo_target_language": False,
    }
    assert cfg["input_audio_transcription"] == {}
    assert cfg["output_audio_transcription"] == {}
    assert cfg["context_window_compression"] == {
        "trigger_tokens": 25000,
        "sliding_window": {"target_tokens": 8000},
    }
    assert cfg["session_resumption"] == {"handle": None}
    assert "speech_config" not in cfg


def test_live_config_voice_arm_pins_a_prebuilt_voice():
    cfg = probe.live_config(_opts(voice="Charon"), handle=None)
    assert cfg["speech_config"] == {
        "voice_config": {"prebuilt_voice_config": {"voice_name": "Charon"}}
    }


def test_live_config_carries_the_resumption_handle():
    cfg = probe.live_config(_opts(), handle="h-42")
    assert cfg["session_resumption"] == {"handle": "h-42"}


def test_live_config_without_compression_or_resumption():
    cfg = probe.live_config(_opts(compression=False, resumption=False), handle="x")
    assert "context_window_compression" not in cfg
    assert "session_resumption" not in cfg


def test_decode_args_slice_to_16k_mono_s16le_on_stdout():
    args = probe.decode_args("C:/tools/ffmpeg.exe", "in.flac", 30.0, 1500.0)
    assert args[0] == "C:/tools/ffmpeg.exe"
    assert args[args.index("-ss") + 1] == "30.0"
    assert args[args.index("-t") + 1] == "1500.0"
    assert args.index("-ss") < args.index("-i")  # fast input seek
    assert args[args.index("-i") + 1] == "in.flac"
    assert args[args.index("-ac") + 1] == "1"
    assert args[args.index("-ar") + 1] == "16000"
    assert args[args.index("-f") + 1] == "s16le"
    assert args[-1] == "-"


def test_decode_args_without_duration_reads_to_the_end():
    args = probe.decode_args("ffmpeg", "in.flac", 0.0, None)
    assert "-t" not in args


def test_build_summary_on_synthetic_events():
    sr = 24000
    pcm = _s16([8192] * (sr * 2))  # 2 s of output
    events = [
        {"t": 0.0, "kind": "connect", "connection": 1},
        {"t": 0.1, "kind": "send_start"},
        {"t": 1.1, "kind": "audio", "n_bytes": 100},
        {"t": 1.2, "kind": "session_resumption_update", "handle_present": True},
        {"t": 1.3, "kind": "session_resumption_update", "handle_present": False},
        {"t": 1.6, "kind": "audio", "n_bytes": 100},
        {"t": 5.6, "kind": "audio", "n_bytes": 100},
        {"t": 6.0, "kind": "go_away", "time_left": "5s"},
        {"t": 6.1, "kind": "reconnect", "reason": "go_away"},
        {"t": 6.2, "kind": "connect", "connection": 2},
        {"t": 6.3, "kind": "reconnect_failed", "error": "x"},
    ]
    s = probe.build_summary(
        events,
        model="m",
        api_version="v1beta",
        voice=None,
        compression=True,
        resumption=True,
        slice_s=4.0,
        frames_sent=40,
        frames_total=40,
        output_pcm=pcm,
        output_sr=sr,
        errors=["boom"],
    )
    assert s["model"] == "m"
    assert s["api_version"] == "v1beta"
    assert s["voice"] == "none"
    assert s["compression"] is True
    assert s["resumption"] is True
    assert s["slice_s"] == 4.0
    assert s["frames_sent"] == 40
    assert s["connections"] == 2
    assert s["reconnects"] == 1
    assert s["resumptions_offered"] == 1
    assert s["go_aways"] == 1
    assert s["reconnect_failures"] == 1
    assert s["output_audio_s"] == pytest.approx(2.0)
    assert s["output_to_input_ratio"] == pytest.approx(0.5)
    assert s["max_output_gap_s"] == pytest.approx(4.0)
    assert s["first_output_latency_s"] == pytest.approx(1.0)
    assert len(s["rms_per_5min"]) == 1
    assert s["errors"] == ["boom"]


def test_build_summary_without_output():
    s = probe.build_summary(
        [{"t": 0.0, "kind": "connect_failed", "error": "nope"}],
        model="m",
        api_version="sdk-default",
        voice="Charon",
        compression=False,
        resumption=False,
        slice_s=0.0,
        frames_sent=0,
        frames_total=10,
        output_pcm=b"",
        output_sr=24000,
        errors=["nope"],
    )
    assert s["voice"] == "Charon"
    assert s["connections"] == 0
    assert s["output_audio_s"] == 0.0
    assert s["output_to_input_ratio"] is None
    assert s["first_output_latency_s"] is None
    assert s["max_output_gap_s"] == 0.0
    assert s["rms_per_5min"] == []
    assert s["voiced_output_s"] == 0.0
    assert s["max_voiced_gap_s"] == 0.0
    assert s["first_voiced_latency_s"] is None
    assert s["drain_end_reason"] is None


def test_build_summary_voiced_fields_ignore_streamed_silence():
    sr = 24000
    events = [
        {"t": 0.0, "kind": "connect"},
        {"t": 0.5, "kind": "send_start"},
        {"t": 1.0, "kind": "audio", "n_bytes": 4800, "voiced": False},
        {"t": 2.0, "kind": "audio", "n_bytes": 4800, "voiced": True},
        {"t": 2.1, "kind": "audio", "n_bytes": 4800, "voiced": False},
        {"t": 5.0, "kind": "audio", "n_bytes": 9600, "voiced": True},
        {"t": 5.1, "kind": "audio", "n_bytes": 4800, "voiced": False},
        {"t": 9.0, "kind": "drain_end", "reason": "quiet"},
    ]
    s = probe.build_summary(
        events,
        model="m",
        api_version="v1beta",
        voice=None,
        compression=True,
        resumption=True,
        slice_s=10.0,
        frames_sent=100,
        frames_total=100,
        output_pcm=b"",
        output_sr=sr,
        errors=[],
    )
    assert s["voiced_output_s"] == pytest.approx(0.3)
    assert s["max_voiced_gap_s"] == pytest.approx(3.0)
    assert s["max_output_gap_s"] == pytest.approx(2.9)
    assert s["first_output_latency_s"] == pytest.approx(0.5)
    assert s["first_voiced_latency_s"] == pytest.approx(1.5)
    assert s["drain_end_reason"] == "quiet"


# ── the session loop against a fake Live server ─────────────────────────────────


def _msg(**kw):
    base = {
        "data": None,
        "server_content": None,
        "go_away": None,
        "session_resumption_update": None,
        "usage_metadata": None,
    }
    base.update(kw)
    return SimpleNamespace(**base)


def _content(**kw):
    base = {
        "input_transcription": None,
        "output_transcription": None,
        "turn_complete": None,
        "generation_complete": None,
        "interrupted": None,
    }
    base.update(kw)
    return SimpleNamespace(**base)


def _resumption(handle):
    return _msg(
        session_resumption_update=SimpleNamespace(
            new_handle=handle,
            resumable=handle is not None,
            last_consumed_client_message_index=None,
        )
    )


_CLOSE = object()


class FakeSession:
    """One fake Live connection. `on_frame(session, n)` reacts to the n-th frame
    (1-based, per connection) by queueing server messages; `_CLOSE` in the queue
    makes `receive()` raise like the SDK does on a closed websocket. Like the
    SDK, `receive()` ends its iteration after a `turn_complete` message."""

    def __init__(self, server, on_frame):
        self.server = server
        self.on_frame = on_frame
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
        self.send_times.append(self.server.clock.now())
        self.frames.append(audio)
        self.server.all_frames.append(audio)
        if self.on_frame is not None:
            self.on_frame(self, len(self.frames))
        if self.server.send_latency_s:
            # A real websocket send takes time; a fixed-sleep pacer would add it
            # on top of every frame period, the drift-corrected one must not.
            await self.server.clock.sleep(self.server.send_latency_s)

    async def receive(self):
        while True:
            m = await self.queue.get()
            if m is _CLOSE:
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
    ):
        self.scripts = list(scripts)
        self.on_stream_end = on_stream_end
        self.clock = clock or RealClock()
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
            s = FakeSession(self, script)
            self.sessions.append(s)
            yield s

        return cm()


def _frames(n: int) -> list[bytes]:
    return [bytes([i]) * 3200 for i in range(n)]


def _fast(**kw) -> probe.ProbeOptions:
    return _opts(frame_s=0.001, quiet_s=0.05, tail_cap_s=2.0, **kw)


def _run(server, frames, opts, redact_word=None):
    events = probe.EventLog(None)
    state = probe.ProbeState()
    asyncio.run(
        probe.run_probe(
            frames,
            server.connect,
            lambda b: b,
            opts,
            events,
            state,
            secret=redact_word,
        )
    )
    return events, state


def _echo(session, n):
    # Each input frame yields 4800 bytes (100 ms @ 24 kHz) of output audio.
    session.queue.put_nowait(_msg(data=b"\x10" * 4800))


def test_single_connection_streams_every_frame_once_then_drains_quietly():
    server = FakeServer([_echo])
    frames = _frames(5)
    events, state = _run(server, frames, _fast())
    assert server.all_frames == frames
    assert server.stream_ends == 1
    assert state.frames_sent == 5
    assert len(state.out) == 5 * 4800
    assert [n for _, n in state.chunks] == [4800] * 5
    kinds = [e["kind"] for e in events.events]
    assert kinds.count("connect") == 1
    assert "reconnect" not in kinds
    ends = [e for e in events.events if e["kind"] == "drain_end"]
    assert [e["reason"] for e in ends] == ["quiet"]
    audio = [e for e in events.events if e["kind"] == "audio"]
    assert all(e["voiced"] is True for e in audio)


def test_transcriptions_and_flags_are_recorded():
    def script(session, n):
        if n == 1:
            session.queue.put_nowait(
                _msg(
                    server_content=_content(
                        input_transcription=SimpleNamespace(text="Hello "),
                        output_transcription=SimpleNamespace(text="Ahoj "),
                        turn_complete=True,
                        generation_complete=True,
                    )
                )
            )
        if n == 2:
            # After a turn_complete the fake's receive() ended; the probe must
            # re-enter receive() to see this.
            session.queue.put_nowait(
                _msg(
                    server_content=_content(
                        output_transcription=SimpleNamespace(text="svet")
                    ),
                    usage_metadata=SimpleNamespace(
                        prompt_token_count=10,
                        total_token_count=12,
                        response_token_count=2,
                    ),
                )
            )

    events, state = _run(FakeServer([script]), _frames(3), _fast())
    assert "".join(state.input_parts) == "Hello "
    assert "".join(state.output_parts) == "Ahoj svet"
    kinds = [e["kind"] for e in events.events]
    assert "turn_complete" in kinds
    assert "generation_complete" in kinds
    usage = [e for e in events.events if e["kind"] == "usage_metadata"]
    assert usage and usage[0]["total_token_count"] == 12


def test_go_away_reconnects_with_the_latest_handle_and_resends_nothing():
    def first(session, n):
        session.queue.put_nowait(_msg(data=b"\x01" * 480))
        if n == 2:
            session.queue.put_nowait(_resumption("h1"))
        if n == 3:
            session.queue.put_nowait(_resumption("h2"))
            session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="10s")))

    server = FakeServer([first, _echo])
    frames = _frames(200)
    events, state = _run(server, frames, _fast())
    assert len(server.configs) == 2
    assert server.configs[0]["session_resumption"] == {"handle": None}
    assert server.configs[1]["session_resumption"] == {"handle": "h2"}
    # No frame is sent twice and none is skipped across the reconnect.
    assert server.all_frames == frames
    assert server.sessions[1].frames[0] == frames[len(server.sessions[0].frames)]
    # audio_stream_end only once, on the connection that sent the last frame.
    assert server.stream_ends == 1
    assert server.sessions[0].stream_end == 0
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert len(rec) == 1
    assert rec[0]["reason"] == "go_away"
    assert rec[0]["handle_present"] is True
    assert rec[0]["frame_index"] == len(server.sessions[0].frames)
    ga = [e for e in events.events if e["kind"] == "go_away"]
    assert ga[0]["time_left"] == "10s"


def test_closed_before_everything_is_sent_reconnects():
    def first(session, n):
        if n == 1:
            session.queue.put_nowait(_resumption("h1"))
        if n == 2:
            session.queue.put_nowait(_CLOSE)

    server = FakeServer([first, _echo])
    frames = _frames(200)
    events, state = _run(server, frames, _fast(), redact_word="websocket")
    assert len(server.configs) == 2
    assert server.configs[1]["session_resumption"] == {"handle": "h1"}
    assert state.frames_sent == 200
    closed = [e for e in events.events if e["kind"] == "closed"]
    assert closed
    # The error text is redacted with the secret.
    assert "websocket" not in closed[0]["error"]
    assert "<redacted>" in closed[0]["error"]
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert rec[0]["reason"] == "closed"


def test_closed_after_everything_is_sent_does_not_reconnect():
    def only(session, n):
        session.queue.put_nowait(_msg(data=b"\x01" * 480))

    server = FakeServer(
        [only], on_stream_end=lambda session: session.queue.put_nowait(_CLOSE)
    )
    frames = _frames(3)
    opts = _opts(frame_s=0.001, quiet_s=5.0, tail_cap_s=5.0)
    events, _ = _run(server, frames, opts)
    assert len(server.configs) == 1
    kinds = [e["kind"] for e in events.events]
    assert "closed" in kinds
    assert "reconnect" not in kinds


def test_a_refused_reconnect_is_recorded_and_stops_cleanly():
    def first(session, n):
        if n == 1:
            session.queue.put_nowait(_resumption("h1"))
        if n == 2:
            session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="1s")))

    server = FakeServer([first, "refuse"])
    events, state = _run(server, _frames(200), _fast())
    failed = [e for e in events.events if e["kind"] == "reconnect_failed"]
    assert len(failed) == 1
    assert failed[0]["handle_present"] is True
    assert "1008" in failed[0]["error"]
    assert state.errors and "1008" in state.errors[0]
    assert state.frames_sent < 200
    assert server.stream_ends == 0


def test_first_connect_failure_is_recorded():
    server = FakeServer(["refuse"])
    events, state = _run(server, _frames(3), _fast())
    kinds = [e["kind"] for e in events.events]
    assert "connect_failed" in kinds
    assert "connect" not in kinds
    assert state.frames_sent == 0
    assert state.errors


def test_without_resumption_the_reconnect_starts_a_fresh_session():
    def first(session, n):
        if n == 2:
            session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="1s")))

    server = FakeServer([first, _echo])
    frames = _frames(5)
    events, state = _run(server, frames, _fast(resumption=False))
    assert all("session_resumption" not in c for c in server.configs)
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert rec[0]["handle_present"] is False
    assert server.all_frames == frames


def test_tail_cap_ends_a_receive_that_never_goes_quiet():
    def chatty(session, n):
        if n == 1:

            async def spam():
                while True:
                    session.queue.put_nowait(_msg(data=b"\x01" * 48))
                    await asyncio.sleep(0.005)

            session.spam = asyncio.get_running_loop().create_task(spam())

    server = FakeServer([chatty])
    opts = _opts(frame_s=0.001, quiet_s=1.0, tail_cap_s=0.2)
    events, state = _run(server, _frames(2), opts)
    server.sessions[0].spam.cancel()
    ends = [e for e in events.events if e["kind"] == "drain_end"]
    assert ends and ends[0]["reason"] == "tail_cap"


# ── review round 1: pacing, silence-aware drain, GoAway grace, unknown fields ───


def test_send_loop_paces_at_real_time_without_drift_or_bursts():
    # On a virtual clock with a 15 ms send latency, the drift-corrected pacer
    # sends frame j at EXACTLY t0 + j*frame_s: the latency is absorbed by the
    # next (shorter) sleep. A fixed sleep(frame_s) would put frame j at
    # t0 + j*(frame_s + latency), an unpaced loop at t0 + j*latency.
    fs = 0.1
    vc = VirtualClock()
    server = FakeServer([None], send_latency_s=0.015, clock=vc)
    frames = _frames(200)
    opts = _opts(frame_s=fs, quiet_s=0.05, tail_cap_s=1.0, clock=vc.now, sleep=vc.sleep)
    _run(server, frames, opts)
    times = server.sessions[0].send_times
    assert len(times) == 200
    for j, t in enumerate(times):
        assert abs(t - (times[0] + j * fs)) < 1e-9, f"frame {j} off schedule"


def test_pacing_is_re_anchored_on_each_connection():
    # The second connection opens 0.3 s (virtual) late: re-anchored, the frames
    # it still owes are paced again from its own start; a schedule kept on the
    # first connection's anchor would burst them out at once to "catch up".
    fs = 0.02

    def first(session, n):
        if n == 10:
            session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="0.5s")))

    vc = VirtualClock()
    server = FakeServer([first, None], reconnect_delay_s=0.3, clock=vc)
    frames = _frames(30)
    opts = _opts(frame_s=fs, quiet_s=0.05, tail_cap_s=1.0, clock=vc.now, sleep=vc.sleep)
    _run(server, frames, opts)
    assert len(server.sessions) == 2
    times = server.sessions[1].send_times
    assert len(times) >= 10
    for j, t in enumerate(times):
        assert abs(t - (times[0] + j * fs)) < 1e-9, f"frame {j} of conn 2 burst"
    assert server.all_frames == frames


def test_streamed_silence_does_not_hold_the_drain_open():
    # The Live session streams SILENCE after speech until closed; the drain must
    # end on "quiet" (no VOICED output for quiet_s), not wait for the tail cap.
    def script(session, n):
        session.queue.put_nowait(_msg(data=b"\x10" * 480))
        if n == 3:

            async def silence():
                while True:
                    session.queue.put_nowait(_msg(data=b"\x00" * 480))
                    await asyncio.sleep(0.005)

            session.silence = asyncio.get_running_loop().create_task(silence())

    server = FakeServer([script])
    opts = _opts(frame_s=0.001, quiet_s=0.3, tail_cap_s=2.0)
    events, state = _run(server, _frames(3), opts)
    ends = [e for e in events.events if e["kind"] == "drain_end"]
    assert [e["reason"] for e in ends] == ["quiet"]
    audio = [e for e in events.events if e["kind"] == "audio"]
    assert any(e["voiced"] is False for e in audio)
    assert state.last_voiced_t is not None


def test_go_away_keeps_receiving_the_old_connection_output_before_reconnecting():
    late = b"\x20" * 960

    def first(session, n):
        session.queue.put_nowait(_msg(data=b"\x10" * 480))
        if n == 2:
            session.queue.put_nowait(_resumption("h1"))
        if n == 10:
            session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="10s")))

            async def trailing_translation():
                await asyncio.sleep(0.35)  # > one supervisor poll (0.2 s)
                session.queue.put_nowait(_msg(data=late))

            session.late = asyncio.get_running_loop().create_task(
                trailing_translation()
            )

    server = FakeServer([first, _echo])
    frames = _frames(200)
    opts = _opts(frame_s=0.001, quiet_s=0.8, tail_cap_s=2.0)
    events, state = _run(server, frames, opts)
    # The old connection's trailing output arrived BEFORE the reconnect.
    kinds = [e["kind"] for e in events.events]
    late_idx = next(
        i
        for i, e in enumerate(events.events)
        if e["kind"] == "audio" and e["n_bytes"] == len(late)
    )
    assert late_idx < kinds.index("reconnect")
    assert late in bytes(state.out)
    # The sender stopped at a frame boundary once the GoAway arrived.
    assert len(server.sessions[0].frames) <= 11
    assert server.all_frames == frames
    rec = next(e for e in events.events if e["kind"] == "reconnect")
    assert rec["frames_since_handle"] == len(server.sessions[0].frames) - 2
    grace = next(e for e in events.events if e["kind"] == "go_away_grace")
    assert grace["grace_s"] == pytest.approx(0.8)


def test_go_away_during_the_drain_never_resends_audio_stream_end():
    def only(session, n):
        session.queue.put_nowait(_msg(data=b"\x10" * 480))

    def go_away_at_stream_end(session):
        # The GoAway lands in the same instant as audio_stream_end: its 0.2 s
        # grace ends long before the 1 s drain goes quiet, so the probe
        # reconnects with nothing left to send.
        session.queue.put_nowait(_msg(go_away=SimpleNamespace(time_left="1.2s")))

    server = FakeServer([only, None], on_stream_end=go_away_at_stream_end)
    frames = _frames(3)
    opts = _opts(frame_s=0.001, quiet_s=1.0, tail_cap_s=3.0)
    events, _ = _run(server, frames, opts)
    assert any(e["kind"] == "go_away" for e in events.events)
    assert len(server.sessions) == 2
    assert server.sessions[1].frames == []
    assert server.stream_ends == 1
    assert server.all_frames == frames


def test_unrecognised_message_fields_are_recorded():
    def script(session, n):
        if n == 1:
            session.queue.put_nowait(
                _msg(voice_activity=SimpleNamespace(voice_activity_type="START"))
            )
            session.queue.put_nowait(
                _msg(
                    server_content=_content(
                        interim_input_transcription=SimpleNamespace(text="he"),
                        waiting_for_input=True,
                    )
                )
            )

    events, _ = _run(FakeServer([script]), _frames(2), _fast())
    other = [e for e in events.events if e["kind"] == "other_fields"]
    names = {n for e in other for n in e["fields"]}
    assert "voice_activity" in names
    assert "server_content.interim_input_transcription" in names
    assert "server_content.waiting_for_input" in names


def test_main_stdout_is_one_ascii_json_line_and_the_key_is_redacted(
    tmp_path, monkeypatch, capsys
):
    word = "ghostword"
    monkeypatch.setenv("GEMINI_API_KEY", word)
    missing = str(tmp_path / f"no-ffmpég-{word}")
    out_dir = tmp_path / "out"
    rc = probe.main(
        ["--audio", "in.flac", "--ffmpeg", missing, "--out-dir", str(out_dir)]
    )
    assert rc == 1
    lines = capsys.readouterr().out.splitlines()
    assert len(lines) == 1
    assert lines[0].isascii()
    summary = json.loads(lines[0])
    assert summary["connections"] == 0
    assert summary["errors"]
    assert word not in lines[0]
    assert "<redacted>" in summary["errors"][0]
    for name in (
        "output.wav",
        "summary.json",
        "events.jsonl",
        "input_text.txt",
        "output_text.txt",
    ):
        path = out_dir / name
        assert path.exists(), name
        assert word not in path.read_bytes().decode("utf-8", "replace")
