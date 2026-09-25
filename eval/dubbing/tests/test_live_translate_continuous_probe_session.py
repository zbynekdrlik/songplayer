"""Session-loop tests for the #184 round-H continuous-session probe: `run_probe`
driven against a FAKE Live server (`live_fakes.py` — the external Gemini Live
service is the only thing faked), so pacing, reconnect / resumption-handle /
resume-from-next-frame, the GoAway grace, the voiced-output drain and the
teardown are proven with no network and no google-genai installed.
"""

from __future__ import annotations

import asyncio
from types import SimpleNamespace

import pytest

from eval.dubbing.tests.live_fakes import (
    CLOSE,
    FakeServer,
    VirtualClock,
    echo_frame,
    fast_opts,
    live_content,
    live_msg,
    live_resumption,
    pcm_frame_list,
    probe_opts,
    run_fake,
)


def test_single_connection_streams_every_frame_once_then_drains_quietly():
    server = FakeServer([echo_frame])
    frames = pcm_frame_list(5)
    events, state = run_fake(server, frames, fast_opts())
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
    setup = [e for e in events.events if e["kind"] == "setup_complete"]
    assert setup == [
        {
            "t": setup[0]["t"],
            "kind": "setup_complete",
            "present": True,
            "session_id": "s",
        }
    ]


def test_transcriptions_and_flags_are_recorded():
    def script(session, n):
        if n == 1:
            session.queue.put_nowait(
                live_msg(
                    server_content=live_content(
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
                live_msg(
                    server_content=live_content(
                        output_transcription=SimpleNamespace(text="svet")
                    ),
                    usage_metadata=SimpleNamespace(
                        prompt_token_count=10,
                        total_token_count=12,
                        response_token_count=2,
                    ),
                )
            )

    events, state = run_fake(FakeServer([script]), pcm_frame_list(3), fast_opts())
    assert "".join(state.input_parts) == "Hello "
    assert "".join(state.output_parts) == "Ahoj svet"
    kinds = [e["kind"] for e in events.events]
    assert "turn_complete" in kinds
    assert "generation_complete" in kinds
    usage = [e for e in events.events if e["kind"] == "usage_metadata"]
    assert usage and usage[0]["total_token_count"] == 12


def test_go_away_reconnects_with_the_latest_handle_and_resends_nothing():
    def first(session, n):
        session.queue.put_nowait(live_msg(data=b"\x01" * 480))
        if n == 2:
            session.queue.put_nowait(live_resumption("h1"))
        if n == 3:
            session.queue.put_nowait(live_resumption("h2"))
            session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="10s")))

    server = FakeServer([first, echo_frame])
    frames = pcm_frame_list(200)
    events, state = run_fake(server, frames, fast_opts())
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
            session.queue.put_nowait(live_resumption("h1"))
        if n == 2:
            session.queue.put_nowait(CLOSE)

    server = FakeServer([first, echo_frame])
    frames = pcm_frame_list(200)
    events, state = run_fake(server, frames, fast_opts(), redact_word="websocket")
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
        session.queue.put_nowait(live_msg(data=b"\x01" * 480))

    server = FakeServer(
        [only], on_stream_end=lambda session: session.queue.put_nowait(CLOSE)
    )
    frames = pcm_frame_list(3)
    opts = probe_opts(frame_s=0.001, quiet_s=5.0, tail_cap_s=5.0)
    events, _ = run_fake(server, frames, opts)
    assert len(server.configs) == 1
    kinds = [e["kind"] for e in events.events]
    assert "closed" in kinds
    assert "reconnect" not in kinds


def test_a_refused_reconnect_is_recorded_and_stops_cleanly():
    def first(session, n):
        if n == 1:
            session.queue.put_nowait(live_resumption("h1"))
        if n == 2:
            session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="1s")))

    server = FakeServer([first, "refuse"])
    events, state = run_fake(server, pcm_frame_list(200), fast_opts())
    failed = [e for e in events.events if e["kind"] == "reconnect_failed"]
    assert len(failed) == 1
    assert failed[0]["handle_present"] is True
    assert "1008" in failed[0]["error"]
    assert state.errors and "1008" in state.errors[0]
    assert state.frames_sent < 200
    assert server.stream_ends == 0


def test_first_connect_failure_is_recorded():
    server = FakeServer(["refuse"])
    events, state = run_fake(server, pcm_frame_list(3), fast_opts())
    kinds = [e["kind"] for e in events.events]
    assert "connect_failed" in kinds
    assert "connect" not in kinds
    assert state.frames_sent == 0
    assert state.errors


def test_without_resumption_the_reconnect_starts_a_fresh_session():
    def first(session, n):
        if n == 2:
            session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="1s")))

    server = FakeServer([first, echo_frame])
    frames = pcm_frame_list(5)
    events, state = run_fake(server, frames, fast_opts(resumption=False))
    assert all("session_resumption" not in c for c in server.configs)
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert rec[0]["handle_present"] is False
    assert server.all_frames == frames


def test_tail_cap_ends_a_receive_that_never_goes_quiet():
    def chatty(session, n):
        if n == 1:

            async def spam():
                while True:
                    session.queue.put_nowait(live_msg(data=b"\x01" * 48))
                    await asyncio.sleep(0.005)

            session.spam = asyncio.get_running_loop().create_task(spam())

    server = FakeServer([chatty])
    opts = probe_opts(frame_s=0.001, quiet_s=1.0, tail_cap_s=0.2)
    events, state = run_fake(server, pcm_frame_list(2), opts)
    server.sessions[0].spam.cancel()
    ends = [e for e in events.events if e["kind"] == "drain_end"]
    assert ends and ends[0]["reason"] == "tail_cap"
    # The cap counts from audio_stream_end, not from the last voiced chunk (a
    # never-silent session would otherwise never be capped).
    end_t = next(e["t"] for e in events.events if e["kind"] == "audio_stream_end")
    assert ends[0]["t"] - end_t < 0.2 + 0.5


# ── review round 1: pacing, silence-aware drain, GoAway grace, unknown fields ───


def test_send_loop_paces_at_real_time_without_drift_or_bursts():
    # On a virtual clock with a 15 ms send latency, the drift-corrected pacer
    # sends frame j at EXACTLY t0 + j*frame_s: the latency is absorbed by the
    # next (shorter) sleep. A fixed sleep(frame_s) would put frame j at
    # t0 + j*(frame_s + latency), an unpaced loop at t0 + j*latency.
    fs = 0.1
    vc = VirtualClock()
    server = FakeServer([None], send_latency_s=0.015, clock=vc)
    frames = pcm_frame_list(200)
    opts = probe_opts(
        frame_s=fs, quiet_s=0.05, tail_cap_s=1.0, clock=vc.now, sleep=vc.sleep
    )
    run_fake(server, frames, opts)
    times = server.sessions[0].send_times
    assert len(times) == 200
    t0 = server.sessions[0].opened_at  # the first frame goes out at once
    for j, t in enumerate(times):
        assert abs(t - (t0 + j * fs)) < 1e-9, f"frame {j} off schedule"


def test_pacing_is_re_anchored_on_each_connection():
    # The second connection opens 0.3 s (virtual) late: re-anchored, the frames
    # it still owes are paced again from its own start; a schedule kept on the
    # first connection's anchor would burst them out at once to "catch up".
    fs = 0.02

    def first(session, n):
        if n == 10:
            session.queue.put_nowait(
                live_msg(go_away=SimpleNamespace(time_left="0.5s"))
            )

    vc = VirtualClock()
    server = FakeServer([first, None], reconnect_delay_s=0.3, clock=vc)
    frames = pcm_frame_list(30)
    opts = probe_opts(
        frame_s=fs, quiet_s=0.05, tail_cap_s=1.0, clock=vc.now, sleep=vc.sleep
    )
    run_fake(server, frames, opts)
    assert len(server.sessions) == 2
    # Each connection's first frame goes out the moment it opens (a schedule
    # still indexed by the absolute frame number would make connection 2 wait
    # k0 periods — ~600 s after a 10-min GoAway) and the rest follow exactly
    # one period apart.
    for s in server.sessions:
        assert s.send_times, f"connection {s.index} sent nothing"
        for j, t in enumerate(s.send_times):
            want = s.opened_at + j * fs
            assert abs(t - want) < 1e-9, f"conn {s.index} frame {j} off schedule"
    assert len(server.sessions[1].send_times) >= 10
    assert server.all_frames == frames


def test_streamed_silence_does_not_hold_the_drain_open():
    # The Live session streams SILENCE after speech until closed; the drain must
    # end on "quiet" (no VOICED output for quiet_s), not wait for the tail cap.
    def script(session, n):
        session.queue.put_nowait(live_msg(data=b"\x10" * 480))
        if n == 3:

            async def silence():
                while True:
                    session.queue.put_nowait(live_msg(data=b"\x00" * 480))
                    await asyncio.sleep(0.005)

            session.silence = asyncio.get_running_loop().create_task(silence())

    server = FakeServer([script])
    opts = probe_opts(frame_s=0.001, quiet_s=0.3, tail_cap_s=2.0)
    events, state = run_fake(server, pcm_frame_list(3), opts)
    ends = [e for e in events.events if e["kind"] == "drain_end"]
    assert [e["reason"] for e in ends] == ["quiet"]
    audio = [e for e in events.events if e["kind"] == "audio"]
    assert any(e["voiced"] is False for e in audio)
    assert state.last_voiced_t is not None


def test_go_away_keeps_receiving_the_old_connection_output_before_reconnecting():
    late = b"\x20" * 960

    def first(session, n):
        session.queue.put_nowait(live_msg(data=b"\x10" * 480))
        if n == 2:
            session.queue.put_nowait(live_resumption("h1"))
        if n == 10:
            session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="10s")))

            async def trailing_translation():
                await asyncio.sleep(0.35)  # > one supervisor poll (0.2 s)
                session.queue.put_nowait(live_msg(data=late))

            session.late = asyncio.get_running_loop().create_task(
                trailing_translation()
            )

    server = FakeServer([first, echo_frame])
    frames = pcm_frame_list(200)
    opts = probe_opts(frame_s=0.001, quiet_s=0.8, tail_cap_s=2.0)
    events, state = run_fake(server, frames, opts)
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
    assert len(server.sessions[0].frames) == 10
    assert server.all_frames == frames
    rec = next(e for e in events.events if e["kind"] == "reconnect")
    assert rec["frames_since_handle"] == len(server.sessions[0].frames) - 2
    grace = next(e for e in events.events if e["kind"] == "go_away_grace")
    assert grace["grace_s"] == pytest.approx(0.8)


def test_go_away_during_the_drain_never_resends_audio_stream_end():
    def only(session, n):
        session.queue.put_nowait(live_msg(data=b"\x10" * 480))

    def go_away_at_stream_end(session):
        # The GoAway lands in the same instant as audio_stream_end: its 0.2 s
        # grace ends long before the 1 s drain goes quiet, so the probe
        # reconnects with nothing left to send.
        session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="1.2s")))

    server = FakeServer([only, None], on_stream_end=go_away_at_stream_end)
    frames = pcm_frame_list(3)
    opts = probe_opts(frame_s=0.001, quiet_s=1.0, tail_cap_s=3.0)
    events, _ = run_fake(server, frames, opts)
    assert any(e["kind"] == "go_away" for e in events.events)
    assert len(server.sessions) == 2
    assert server.sessions[1].frames == []
    assert server.stream_ends == 1
    assert server.all_frames == frames


def test_unrecognised_message_fields_are_recorded():
    def script(session, n):
        if n == 1:
            session.queue.put_nowait(
                live_msg(voice_activity=SimpleNamespace(voice_activity_type="START"))
            )
            session.queue.put_nowait(
                live_msg(
                    server_content=live_content(
                        interim_input_transcription=SimpleNamespace(text="he"),
                        waiting_for_input=True,
                    )
                )
            )

    events, _ = run_fake(FakeServer([script]), pcm_frame_list(2), fast_opts())
    other = [e for e in events.events if e["kind"] == "other_fields"]
    names = {n for e in other for n in e["fields"]}
    assert "voice_activity" in names
    assert "server_content.interim_input_transcription" in names
    assert "server_content.waiting_for_input" in names


# ── review round 2: teardown, failure branches, model_turn parts ────────────────


def test_teardown_lets_an_in_flight_send_finish_so_no_frame_is_sent_twice():
    # The GoAway (grace 0) is decided while frame 10's send is still in flight:
    # the server already has frame 10, so cancelling that send would leave
    # frames_sent at 9 and the next connection would send frame 10 again.
    def first(session, n):
        if n == 10:
            session.queue.put_nowait(
                live_msg(go_away=SimpleNamespace(time_left="0.5s"))
            )

    server = FakeServer([first, echo_frame], slow_at={1: (10, 0.3)})
    frames = pcm_frame_list(20)
    events, state = run_fake(server, frames, probe_opts(frame_s=0.001, quiet_s=0.1))
    assert len(server.sessions) == 2
    assert server.all_frames == frames  # every frame exactly once, in order
    assert state.frames_sent == 20


def test_a_failed_send_is_a_close_and_reconnects_without_losing_the_frame():
    server = FakeServer([echo_frame, echo_frame], fail_at={1: 4})
    frames = pcm_frame_list(8)
    events, state = run_fake(server, frames, fast_opts())
    closed = [e for e in events.events if e["kind"] == "closed"]
    assert any("1006" in e["error"] for e in closed)
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert [e["reason"] for e in rec] == ["closed"]
    assert rec[0]["frame_index"] == 3
    assert server.all_frames == frames


def test_reconnects_stop_at_max_connections():
    def close_after_first(session, n):
        if n == 1:
            session.queue.put_nowait(CLOSE)

    server = FakeServer([close_after_first] * 3)
    events, state = run_fake(server, pcm_frame_list(50), fast_opts(max_connections=3))
    assert len(server.configs) == 3
    assert [e["reason"] for e in events.events if e["kind"] == "stop"] == [
        "max_connections"
    ]
    assert any("max connections (3)" in e for e in state.errors)


def test_a_close_during_the_go_away_grace_still_reconnects():
    # After audio_stream_end the server sends a GoAway and then drops the socket
    # inside the grace: that is the announced GoAway, not an unexplained close
    # (a close after everything was sent would NOT reconnect).
    def go_away_then_close(session):
        session.queue.put_nowait(live_msg(go_away=SimpleNamespace(time_left="5s")))
        session.queue.put_nowait(CLOSE)

    def only(session, n):
        session.queue.put_nowait(live_msg(data=b"\x10" * 480))

    server = FakeServer([only, None], on_stream_end=go_away_then_close)
    events, _ = run_fake(
        server, pcm_frame_list(3), probe_opts(frame_s=0.001, quiet_s=1.0)
    )
    assert len(server.sessions) == 2
    rec = [e for e in events.events if e["kind"] == "reconnect"]
    assert [e["reason"] for e in rec] == ["go_away"]
    assert server.stream_ends == 1


def test_non_audio_model_turn_parts_are_recorded():
    def script(session, n):
        if n == 1:
            part = SimpleNamespace(inline_data=None, text="(thinking aloud)")
            session.queue.put_nowait(
                live_msg(
                    server_content=live_content(
                        model_turn=SimpleNamespace(parts=[part], role="model")
                    )
                )
            )

    events, _ = run_fake(FakeServer([script]), pcm_frame_list(2), fast_opts())
    other = [e for e in events.events if e["kind"] == "other_fields"]
    names = {n for e in other for n in e["fields"]}
    assert "server_content.model_turn.parts.text" in names
