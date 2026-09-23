"""#184 round H step 2 — the production dub is ONE continuous Live Translate
session (`scripts/dub_live_session.py`), driven here against a FAKE Live server
(`dub_live_fakes.py` — the external Gemini Live service is the only thing
faked), so the overlap reconnect, the global 1.0x schedule, resumption-handle
tracking, the voiced-output drain and every failure branch are proven with no
network and no google-genai installed.
"""

from __future__ import annotations

import asyncio
from types import SimpleNamespace

import pytest

from dub_live_fakes import (
    CLOSE,
    RUN_TIMEOUT_S,
    FakeServer,
    VirtualClock,
    echo_frame,
    fast_opts,
    go_away,
    live_content,
    live_msg,
    live_resumption,
    pcm_frame_list,
    run_fake,
    session_opts,
)
import dub_live_session as dls


def kinds(events):
    return [e["kind"] for e in events.events]


# ── pure helpers ──────────────────────────────────────────────────────────────


def test_frame_deadline_is_computed_never_accumulated():
    assert dls.frame_deadline(10.0, 0) == 10.0
    assert dls.frame_deadline(10.0, 21_600) == pytest.approx(2170.0)
    assert dls.frame_deadline(0.0, 3, 0.5) == 1.5


def test_pcm_frames_pads_the_last_frame():
    frames = dls.pcm_frames(b"\x01" * 7000)
    assert [len(f) for f in frames] == [3200, 3200, 3200]
    assert frames[-1].endswith(b"\x00" * (9600 - 7000))
    assert dls.pcm_frames(b"") == []


def test_speaker_voice_means_no_prebuilt_voice():
    assert dls.voice_for_config("speaker") is None
    assert dls.voice_for_config(" Speaker ") is None
    assert dls.voice_for_config("") is None
    assert dls.voice_for_config(None) is None
    assert dls.voice_for_config("Charon") == "Charon"
    assert dls.voice_for_config(" Kore ") == "Kore"


def test_config_is_sk_without_echo_and_the_speaker_voice_has_no_speech_config():
    cfg = dls.live_config(session_opts(), None)
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
    # A pinned prebuilt voice (the `dub_voice` setting) adds speech_config.
    pinned = dls.live_config(session_opts(voice="Charon"), "h1")
    assert pinned["speech_config"] == {
        "voice_config": {"prebuilt_voice_config": {"voice_name": "Charon"}}
    }
    assert pinned["session_resumption"] == {"handle": "h1"}


def test_default_model_is_the_verified_live_translate_preview():
    assert dls.MODEL == "gemini-3.5-live-translate-preview"


def test_first_voiced_frame_is_the_input_onset():
    silent = b"\x00" * 3200
    loud = b"\x00\x20" * 1600
    assert dls.first_voiced_frame([silent, silent, loud, silent]) == 2
    assert dls.first_voiced_frame([silent, silent]) is None
    assert dls.first_voiced_frame([]) is None


def test_go_away_drain_ends_a_margin_before_time_left():
    assert dls.go_away_drain_s("50s", 8.0) == 49.0
    assert dls.go_away_drain_s("0.5s", 8.0) == 0.0
    assert dls.go_away_drain_s(None, 8.0) == 8.0
    assert dls.go_away_drain_s("soon", 8.0) == 8.0
    assert dls.go_away_drain_s("xs", 8.0) == 8.0


def test_pcm_dbfs_and_redact():
    assert dls.pcm_dbfs(b"") == dls.SILENCE_DBFS
    assert dls.pcm_dbfs(b"\x00" * 100) == dls.SILENCE_DBFS
    assert dls.pcm_dbfs(b"\x00\x10" * 100) > dls.VOICED_DBFS
    assert dls.redact("key=abc123 x", "abc123") == "key=<redacted> x"
    assert dls.redact("text", None) == "text"


# ── one connection ────────────────────────────────────────────────────────────


def test_single_connection_streams_every_frame_once_then_drains_quietly():
    server = FakeServer([echo_frame])
    frames = pcm_frame_list(5)
    events, state, sink, _ = run_fake(server, frames, fast_opts())
    assert server.all_frames == frames
    assert server.stream_ends == 1
    assert state.frames_sent == 5
    assert state.stream_end_sent is True
    assert state.t0_s is not None
    assert len(sink.data) == 5 * 4800
    assert [c.n_bytes for c in state.chunks] == [4800] * 5
    assert [c.offset for c in state.chunks] == [0, 4800, 9600, 14400, 19200]
    assert all(c.conn == 1 and c.voiced and c.active for c in state.chunks)
    assert kinds(events).count("connect") == 1
    assert "reconnect" not in kinds(events)
    assert state.drain_end_reason == "quiet"


def test_transcriptions_are_recorded_with_their_arrival_times():
    def script(session, n):
        if n == 1:
            session.queue.put_nowait(
                live_msg(
                    server_content=live_content(
                        input_transcription=SimpleNamespace(text="Hello "),
                        output_transcription=SimpleNamespace(text="Ahoj "),
                        turn_complete=True,
                    )
                )
            )
        if n == 2:
            # After a turn_complete the SDK's receive() ends; the session must
            # re-enter it to see this.
            session.queue.put_nowait(
                live_msg(
                    server_content=live_content(
                        output_transcription=SimpleNamespace(text="svet")
                    )
                )
            )

    _, state, _, _ = run_fake(FakeServer([script]), pcm_frame_list(3), fast_opts())
    assert "".join(state.input_parts) == "Hello "
    assert [t for _, t in state.output_parts] == ["Ahoj ", "svet"]
    times = [a for a, _ in state.output_parts]
    assert times == sorted(times)
    assert all(a >= 0 for a in times)


def test_streamed_silence_does_not_hold_the_drain_open():
    def script(session, n):
        session.queue.put_nowait(live_msg(data=b"\x00\x10" * 240))
        if n == 3:

            async def silence():
                while True:
                    session.queue.put_nowait(live_msg(data=b"\x00" * 480))
                    await asyncio.sleep(0.005)

            session.silence = asyncio.get_running_loop().create_task(silence())

    opts = fast_opts(quiet_s=0.3, tail_cap_s=2.0)
    _, state, _, _ = run_fake(FakeServer([script]), pcm_frame_list(3), opts)
    assert state.drain_end_reason == "quiet"
    assert any(not c.voiced for c in state.chunks)


def test_tail_cap_ends_a_session_that_never_goes_quiet():
    def chatty(session, n):
        if n == 1:

            async def spam():
                while True:
                    session.queue.put_nowait(live_msg(data=b"\x00\x10" * 24))
                    await asyncio.sleep(0.005)

            session.spam = asyncio.get_running_loop().create_task(spam())

    opts = fast_opts(quiet_s=1.0, tail_cap_s=0.2)
    _, state, _, _ = run_fake(FakeServer([chatty]), pcm_frame_list(2), opts)
    assert state.drain_end_reason == "tail_cap"


# ── the overlap reconnect (the round-H step-2 change) ─────────────────────────


def _go_away_script(at=10, handles=((2, "h1"), (3, "h2")), time_left="50s"):
    def first(session, n):
        session.queue.put_nowait(live_msg(data=b"\x00\x10" * 240))
        for frame_n, handle in handles:
            if n == frame_n:
                session.queue.put_nowait(live_resumption(handle))
        if n == at:
            session.queue.put_nowait(go_away(time_left))

    return first


def test_go_away_opens_the_new_connection_while_the_old_one_is_still_fed():
    # The new connection only opens after the server received 5 MORE frames: they
    # can only have gone to the OLD connection — the input never pauses.
    server = FakeServer([_go_away_script(), echo_frame], open_after_frames={2: 5})
    frames = pcm_frame_list(200)
    events, state, _, _ = run_fake(server, frames, fast_opts(quiet_s=0.2))
    assert len(server.sessions) == 2
    old, new = server.sessions
    assert len(old.frames) >= 15  # fed past its GoAway (frame 10) while opening
    # Resumed with the latest handle of the active connection.
    assert server.configs[0]["session_resumption"] == {"handle": None}
    assert server.configs[1]["session_resumption"] == {"handle": "h2"}
    # Every frame exactly once, in order, split old | new at one boundary.
    assert server.all_frames == frames
    assert server.frame_conn == [1] * len(old.frames) + [2] * len(new.frames)
    assert new.frames[0] == frames[len(old.frames)]
    # audio_stream_end once, on the connection that sent the last frame.
    assert server.stream_ends == 1 and new.stream_end == 1 and old.stream_end == 0
    # Overlap: connection 2 opened BEFORE connection 1 closed.
    ks = kinds(events)
    connect2 = next(
        i
        for i, e in enumerate(events.events)
        if e["kind"] == "connect" and e["connection"] == 2
    )
    closed1 = next(
        i
        for i, e in enumerate(events.events)
        if e["kind"] == "closed" and e["connection"] == 1
    )
    assert connect2 < closed1
    assert ks.index("go_away") < ks.index("reconnect") < connect2
    rec = next(e for e in events.events if e["kind"] == "reconnect")
    assert rec["reason"] == "go_away"
    assert rec["handle_present"] is True
    assert rec["frame_index"] >= 10
    # The old connection was stopped once drained, not left open.
    assert any(e["kind"] == "drained" and e["connection"] == 1 for e in events.events)
    # Output of both connections merged in arrival order, tagged per connection.
    assert {c.conn for c in state.chunks} == {1, 2}
    arrivals = [c.arrival_s for c in state.chunks]
    assert arrivals == sorted(arrivals)


def test_pacing_stays_on_one_schedule_across_the_go_away_overlap():
    # On a virtual clock, frame k goes out at EXACTLY anchor + k*frame_s on BOTH
    # connections: no pause at the GoAway (the probe's 8 s grace), no re-anchor.
    fs = 0.1
    vc = VirtualClock()
    server = FakeServer(
        [_go_away_script(at=10), None], clock=vc, open_after_frames={2: 5}
    )
    frames = pcm_frame_list(40)
    opts = fast_opts(frame_s=fs, quiet_s=0.05, clock=vc.now, sleep=vc.sleep)
    run_fake(server, frames, opts)
    assert len(server.sessions) == 2
    sent = sorted(
        (i, t) for s in server.sessions for i, t in zip(s.global_indices, s.send_times)
    )
    assert [i for i, _ in sent] == list(range(40))
    t0 = sent[0][1]
    for k, t in sent:
        assert abs(t - (t0 + k * fs)) < 1e-9, f"frame {k} off schedule"
    assert len(server.sessions[1].send_times) >= 10


def test_resumption_handles_come_only_from_the_active_connection():
    def first(session, n):
        if n == 2:
            session.queue.put_nowait(live_resumption("h1"))
        if n == 3:
            session.queue.put_nowait(go_away("50s"))

            async def stale_after_switch():
                # A late handle from the now-draining OLD connection must not
                # become the resume point (it would rewind to before the switch).
                while len(session.server.sessions) < 2 or not (
                    session.server.sessions[1].frames
                ):
                    await asyncio.sleep(0)
                session.queue.put_nowait(live_resumption("stale"))

            session.stale = asyncio.get_running_loop().create_task(stale_after_switch())

    def second(session, n):
        if n == 1:
            session.queue.put_nowait(live_resumption("fresh"))
        if n == 20:
            session.queue.put_nowait(CLOSE)

    server = FakeServer([first, second, echo_frame])
    frames = pcm_frame_list(200)
    run_fake(server, frames, fast_opts(frame_s=0.002, quiet_s=0.5))
    assert len(server.configs) == 3
    assert server.configs[1]["session_resumption"] == {"handle": "h1"}
    assert server.configs[2]["session_resumption"] == {"handle": "fresh"}
    assert server.all_frames == frames


def test_go_away_after_the_stream_end_does_not_reconnect():
    def only(session, n):
        session.queue.put_nowait(live_msg(data=b"\x00\x10" * 240))

    server = FakeServer(
        [only],
        on_stream_end=lambda session: session.queue.put_nowait(go_away("30s")),
    )
    events, state, _, _ = run_fake(server, pcm_frame_list(3), fast_opts(quiet_s=0.2))
    assert len(server.configs) == 1
    assert "go_away" in kinds(events)
    assert "reconnect" not in kinds(events)
    assert state.drain_end_reason in ("quiet", "closed")


def test_a_close_before_everything_is_sent_reconnects_with_the_handle():
    def first(session, n):
        if n == 1:
            session.queue.put_nowait(live_resumption("h1"))
        if n == 2:
            session.queue.put_nowait(CLOSE)

    server = FakeServer([first, echo_frame])
    frames = pcm_frame_list(200)
    events, state, _, _ = run_fake(server, frames, fast_opts(), redact_word="websocket")
    assert len(server.configs) == 2
    assert server.configs[1]["session_resumption"] == {"handle": "h1"}
    assert state.frames_sent == 200
    assert server.all_frames == frames
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert [e["reason"] for e in rec] == ["closed"]
    closed = [e for e in events.events if e["kind"] == "closed" and e["error"]]
    # The error text is redacted with the secret.
    assert closed and "websocket" not in closed[0]["error"]
    assert "<redacted>" in closed[0]["error"]


def test_a_close_after_everything_is_sent_ends_the_drain_without_reconnecting():
    def only(session, n):
        session.queue.put_nowait(live_msg(data=b"\x00\x10" * 240))

    server = FakeServer(
        [only], on_stream_end=lambda session: session.queue.put_nowait(CLOSE)
    )
    events, state, _, _ = run_fake(
        server, pcm_frame_list(3), fast_opts(quiet_s=5.0, tail_cap_s=5.0)
    )
    assert len(server.configs) == 1
    assert "reconnect" not in kinds(events)
    assert state.drain_end_reason == "closed"


def test_a_failed_send_reconnects_without_losing_the_frame():
    server = FakeServer([echo_frame, echo_frame], fail_at={1: 4})
    frames = pcm_frame_list(8)
    events, state, _, _ = run_fake(server, frames, fast_opts())
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert [e["reason"] for e in rec] == ["closed"]
    assert rec[0]["frame_index"] == 3
    assert server.all_frames == frames
    assert state.frames_sent == 8


# ── failures fail the dub (the Rust worker's backoff retries it) ──────────────


def _run_expect_failure(server, frames, opts):
    events = dls.EventLog(None)
    session = dls.ContinuousSession(
        frames, server.connect, lambda b: b, opts, events, dls.MemorySink()
    )
    with pytest.raises(dls.SessionFailed) as exc:
        asyncio.run(asyncio.wait_for(session.run(), timeout=RUN_TIMEOUT_S))
    return exc.value, events, session.state


def test_a_refused_reconnect_fails_the_session_with_a_clear_error():
    server = FakeServer([_go_away_script(at=4, handles=((2, "h1"),)), "refuse"])
    err, events, state = _run_expect_failure(server, pcm_frame_list(200), fast_opts())
    assert "reconnect" in str(err) and "1008" in str(err)
    failed = [e for e in events.events if e["kind"] == "reconnect_failed"]
    assert len(failed) == 1 and failed[0]["handle_present"] is True
    assert state.frames_sent < 200
    assert server.stream_ends == 0


def test_a_refused_first_connect_fails_the_session():
    server = FakeServer(["refuse"])
    err, events, state = _run_expect_failure(server, pcm_frame_list(3), fast_opts())
    assert "connect refused" in str(err)
    assert "connect_failed" in kinds(events)
    assert state.frames_sent == 0


def test_the_connection_cap_fails_the_session():
    def close_after_first(session, n):
        if n == 1:
            session.queue.put_nowait(CLOSE)

    server = FakeServer([close_after_first] * 3)
    err, events, _ = _run_expect_failure(
        server, pcm_frame_list(50), fast_opts(max_connections=3)
    )
    assert "max connections (3)" in str(err)
    assert len(server.configs) == 3


# ── the log lines the box acceptance reads ─────────────────────────────────────


def test_progress_and_reconnect_lines_are_logged():
    server = FakeServer([_go_away_script(at=10), echo_frame])
    frames = pcm_frame_list(dls.PROGRESS_EVERY_FRAMES)
    _, _, _, lines = run_fake(server, frames, fast_opts(frame_s=0.0))
    text = "\n".join(lines)
    assert "live: connect 1 (handle_present=False, frame 0)" in text
    assert "live: go_away time_left 50s on connection 1 at frame" in text
    assert "live: reconnect (go_away) handle_present=True frame_index=" in text
    assert "live: connect 2 (handle_present=True" in text
    assert "live: switch to connection 2 at frame" in text
    assert f"live: frame 600/{dls.PROGRESS_EVERY_FRAMES} output " in text
    assert "live: audio_stream_end after frame 600/600" in text


# ── the summary ────────────────────────────────────────────────────────────────


def test_summary_ratio_counts_the_stream_not_the_overlap_silence():
    c = dls.OutputChunk
    state = dls.SessionState(frames_sent=10, t0_s=1.0, drain_end_reason="quiet")
    one_s = 48_000  # 1 s of 24 kHz s16le
    state.chunks = [
        c(arrival_s=4.0, conn=1, offset=0, n_bytes=one_s, voiced=True, active=True),
        c(arrival_s=5.0, conn=1, offset=0, n_bytes=one_s, voiced=False, active=True),
        # Draining: trailing speech counts, streamed silence does not.
        c(arrival_s=6.0, conn=1, offset=0, n_bytes=one_s, voiced=True, active=False),
        c(arrival_s=7.0, conn=1, offset=0, n_bytes=one_s, voiced=False, active=False),
        c(arrival_s=6.5, conn=2, offset=0, n_bytes=one_s, voiced=True, active=True),
    ]
    events = [
        {"kind": "connect"},
        {"kind": "connect"},
        {"kind": "reconnect"},
        {"kind": "go_away"},
        {"kind": "session_resumption_update", "handle_present": True},
        {"kind": "session_resumption_update", "handle_present": False},
    ]
    s = dls.build_summary(state, events, input_s=4.0)
    assert s["connections"] == 2
    assert s["reconnects"] == 1
    assert s["go_aways"] == 1
    assert s["resumptions_offered"] == 1
    assert s["output_audio_s"] == 4.0
    assert s["output_to_input_ratio"] == 1.0
    assert s["overlap_output_s"] == 2.0
    assert s["voiced_output_s"] == 3.0
    assert s["max_voiced_gap_s"] == 2.0  # 4.0 -> 6.0
    assert s["first_voiced_latency_s"] == 3.0
    assert s["first_output_latency_s"] == 3.0
    assert s["drain_end_reason"] == "quiet"
