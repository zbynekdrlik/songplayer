"""Tests for scripts/dub_worker.py (#183 D4, #184 round H step 2).

The dub child's runtime work (the Live API, ffmpeg) cannot run in CI, but its
decisions can: the input decode argv, the latency measure + clamp, the placement
of the ONE continuous output stream on the video timeline (per-connection
cursors), the SK subtitle timing, the transcripts JSON in the shape the Rust
`dabing::subtitles::DubTranscripts` reads, the placed-WAV render, the argv
defaults (speaker voice, the verified model), the removal of the superseded
per-chunk work files, and the whole `live-translate` orchestration with the
session + ffmpeg seams faked. numpy + stdlib only (the eval-checks CI job has no
google-genai and no ffmpeg).
"""

import importlib.util
import json
import os
import sys
import wave
from types import SimpleNamespace

import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# `scripts/` on the path so `dub_worker`'s module-level imports (`dub_loudness`,
# `dub_live_session`, `win_replace`) resolve the same way they do on the box.
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)


def _load_dub_worker():
    path = os.path.join(_SCRIPTS_DIR, "dub_worker.py")
    spec = importlib.util.spec_from_file_location("dub_worker", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dw = _load_dub_worker()
dls = dw.dls


def chunk(arrival_s, conn=1, n_samples=2400, offset=0, voiced=True, active=True):
    return dls.OutputChunk(
        arrival_s=arrival_s,
        conn=conn,
        offset=offset,
        n_bytes=n_samples * 2,
        voiced=voiced,
        active=active,
    )


# ── argv ───────────────────────────────────────────────────────────────────────


def test_decode_args_target_16k_mono_s16le_on_stdout():
    args = dw.decode_args("ffmpeg", "/c/a_audio_vocals.flac")
    assert args[0] == "ffmpeg"
    assert args[args.index("-i") + 1] == "/c/a_audio_vocals.flac"
    assert args[args.index("-ar") + 1] == "16000"
    assert args[args.index("-ac") + 1] == "1"
    assert args[args.index("-f") + 1] == "s16le"
    assert args[-1] == "-"


def test_cli_defaults_are_the_speaker_voice_and_the_verified_model():
    args = dw.parse_args(
        [
            "live-translate",
            "--audio",
            "a.flac",
            "--out",
            "o.flac",
            "--transcripts",
            "t.json",
            "--work-dir",
            "w",
        ]
    )
    assert args.voice == "speaker"
    assert args.model == "gemini-3.5-live-translate-preview"
    # `speaker` pins nothing: the session config carries NO speech_config.
    assert dls.voice_for_config(args.voice) is None
    cfg = dls.live_config(
        dls.SessionOptions(voice=dls.voice_for_config(args.voice)), None
    )
    assert "speech_config" not in cfg


def test_cli_passes_model_and_a_pinned_voice_through():
    args = dw.parse_args(
        [
            "live-translate",
            "--audio",
            "a.flac",
            "--out",
            "o.flac",
            "--transcripts",
            "t.json",
            "--work-dir",
            "w",
            "--model",
            "gemini-4-live-translate",
            "--voice",
            "Charon",
        ]
    )
    assert args.model == "gemini-4-live-translate"
    assert dls.voice_for_config(args.voice) == "Charon"


def test_the_superseded_chunk_plan_flag_is_gone():
    with pytest.raises(SystemExit):
        dw.parse_args(
            [
                "live-translate",
                "--audio",
                "a",
                "--out",
                "o",
                "--transcripts",
                "t",
                "--work-dir",
                "w",
                "--chunk-plan",
                "p.json",
            ]
        )


# ── latency ────────────────────────────────────────────────────────────────────


def test_latency_is_clamped_to_one_to_six_seconds():
    assert dw.clamp_latency_ms(3100) == 3100
    assert dw.clamp_latency_ms(200) == 1000
    assert dw.clamp_latency_ms(-500) == 1000
    assert dw.clamp_latency_ms(13_000) == 6000
    assert dw.clamp_latency_ms(1000) == 1000
    assert dw.clamp_latency_ms(6000) == 6000


def test_measured_latency_discounts_the_input_onset():
    # First voiced output 13.1 s after frame 0; the speech starts 10 s in.
    assert dw.measure_latency_ms(113.1, 100.0, 10_000) == 3100
    # No onset known -> measured from frame 0.
    assert dw.measure_latency_ms(103.1, 100.0, None) == 3100
    # No voiced output / no send -> unknown.
    assert dw.measure_latency_ms(None, 100.0, 0) is None
    assert dw.measure_latency_ms(103.0, None, 0) is None


# ── placement on the video timeline ────────────────────────────────────────────


def test_timeline_ms_is_arrival_minus_t0_minus_latency_never_negative():
    assert dw.timeline_ms(105.0, 100.0, 3000) == 2000
    assert dw.timeline_ms(101.0, 100.0, 3000) == 0


def test_one_connection_is_one_continuous_stream_anchored_at_arrival():
    sr = 24000
    # t0 = 100 s, latency 3 s. Chunk 1 arrives at 103.5 s -> 0.5 s on the video.
    chunks = [
        chunk(103.5),  # 0.1 s each (2400 samples)
        chunk(103.5),  # a burst: same arrival -> right after the first
        chunk(103.6),  # arrival 0.6 s, but the stream is already at 0.7 s
        chunk(110.0),  # a stall: re-syncs to arrival (7.0 s)
    ]
    starts = dw.place_output(chunks, 100.0, 3000, sr)
    assert starts == [12_000, 14_400, 16_800, 168_000]


def test_connections_keep_separate_cursors_so_the_overlap_never_shifts_later_audio():
    sr = 24000
    # Connection 1 drains 1 s of trailing translation arriving at 110.0..110.9
    # while connection 2 starts at 110.2: with ONE cursor connection 2 would be
    # pushed 1 s late; with a cursor per connection it lands at its arrival.
    old = [chunk(110.0 + i * 0.1, conn=1) for i in range(10)]
    new = [chunk(110.2 + i * 0.1, conn=2) for i in range(3)]
    merged = sorted(old + new, key=lambda c: c.arrival_s)
    starts = dw.place_output(merged, 100.0, 3000, sr)
    by_conn = {1: [], 2: []}
    for c, s in zip(merged, starts):
        by_conn[c.conn].append(s)
    assert by_conn[1] == [168_000 + i * 2400 for i in range(10)]
    assert by_conn[2] == [172_800 + i * 2400 for i in range(3)]


def test_sk_timed_is_on_the_video_timeline_and_monotonic():
    # (arrival_s, text, connection) — the connection field since review round 1.
    parts = [
        (104.0, "Ahoj ", 1),
        (104.0, "", 1),
        (103.5, "svet.", 1),
        (108.2, "Ďalej", 1),
    ]
    got = dw.sk_timed_from(parts, 100.0, 3000)
    assert got == [
        {"t_ms": 1000, "text": "Ahoj "},
        {"t_ms": 1000, "text": "svet."},  # never earlier than the previous one
        {"t_ms": 5200, "text": "Ďalej"},
    ]


# ── the transcripts JSON (the D3 contract) ──────────────────────────────────────


def test_transcripts_are_one_chunk_on_the_video_timeline():
    t = dw.build_transcripts(
        "Hello world",
        [{"t_ms": 400, "text": "Hello world"}],
        "Ahoj svet.",
        [{"t_ms": 1000, "text": "Ahoj svet."}],
        2_160_000,
    )
    assert t["engine"] == "gemini-live-translate"
    assert t["target_lang"] == "sk"
    assert t["chunks"] == [
        {
            "index": 0,
            "start_ms": 0,
            "end_ms": 2_160_000,
            "at_ms": 0,
            "tempo": 1.0,
            "en": "Hello world",
            "en_timed": [{"t_ms": 400, "text": "Hello world"}],
            "sk": "Ahoj svet.",
            "sk_timed": [{"t_ms": 1000, "text": "Ahoj svet."}],
        }
    ]
    json.dumps(t, allow_nan=False)


# ── the EN timing (#184 H3) ─────────────────────────────────────────────────────


def test_en_timed_subtracts_the_measured_en_latency():
    # #184 H4: Live Translate emits the input transcription phrase by phrase,
    # seconds after the audio was sent, so the EN arrival carries its own
    # measured latency, subtracted exactly like the SK's.
    parts = [
        (105.0, "Hello ", 1),
        (105.0, "", 1),
        (104.5, "world.", 1),
        (108.2, "Next", 1),
    ]
    assert dw.en_timed_from(parts, 100.0, 4000) == [
        {"t_ms": 1000, "text": "Hello "},
        {"t_ms": 1000, "text": "world."},  # never earlier than the previous one
        {"t_ms": 4200, "text": "Next"},
    ]
    # Same arrivals, same latency -> the same timeline as the SK.
    assert dw.en_timed_from(parts, 100.0, 4000) == dw.sk_timed_from(parts, 100.0, 4000)


def test_en_timed_minus_the_latency_clamps_to_zero():
    parts = [(101.0, "Early ", 1), (103.25, "on.", 1)]
    assert dw.en_timed_from(parts, 100.0, 3000) == [
        {"t_ms": 0, "text": "Early "},
        {"t_ms": 250, "text": "on."},
    ]


def test_en_timed_overlap_is_ordered_and_capped_by_connection():
    # The same connection ordering + overlap cap as the SK: the old connection's
    # trailing input text comes first and is capped at the new connection's
    # first fragment, so the new connection keeps its own arrival times.
    parts = [
        (114.0, "End ", 1),
        (114.4, "New ", 2),
        (114.6, "of it.", 1),
        (115.0, "one.", 2),
    ]
    assert dw.en_timed_from(parts, 100.0, 4000) == [
        {"t_ms": 10000, "text": "End "},
        {"t_ms": 10400, "text": "of it."},
        {"t_ms": 10400, "text": "New "},
        {"t_ms": 11000, "text": "one."},
    ]


def test_en_latency_is_measured_from_the_first_input_transcription_and_the_onset():
    # The FIRST input-transcription arrival (the earliest non-empty one, not the
    # list order) - t0 - the input onset, with the SK's measure and clamp.
    parts = [(115.2, "second ", 1), (114.5, "First ", 1), (113.0, "", 1)]
    assert dw.en_latency_ms(parts, 100.0, 10_000) == (4500, 4500)
    # No onset known -> measured from frame 0.
    assert dw.en_latency_ms([(104.5, "Hi", 1)], 100.0, None) == (4500, 4500)
    # Outside 1-6 s -> clamped; the raw value is kept for the log.
    assert dw.en_latency_ms([(100.3, "Hi", 1)], 100.0, 0) == (1000, 300)
    assert dw.en_latency_ms([(113.0, "Hi", 1)], 100.0, 0) == (6000, 13000)


def test_no_input_transcription_gives_en_latency_zero():
    assert dw.en_latency_ms([], 100.0, 0) == (0, None)
    assert dw.en_latency_ms([(104.0, "", 1)], 100.0, 0) == (0, None)
    assert dw.en_latency_ms([(104.0, "Hi", 1)], None, 0) == (0, None)


def test_the_events_log_sits_next_to_the_dub():
    assert (
        dw.events_path_for("/cache/Song_Artist_abc123_normalized_dub.flac")
        == "/cache/Song_Artist_abc123_normalized_dub_events.jsonl"
    )
    assert (
        dw.events_path_for("/c/X_Y_id_normalized_gf_dub.flac")
        == "/c/X_Y_id_normalized_gf_dub_events.jsonl"
    )


# ── the placed WAV ─────────────────────────────────────────────────────────────


def test_wav_header_is_a_16_bit_mono_pcm_header(tmp_path):
    path = tmp_path / "h.wav"
    path.write_bytes(dw.wav_header(3, 24000) + b"\x01\x00\x02\x00\x03\x00")
    with wave.open(str(path), "rb") as w:
        assert w.getnchannels() == 1
        assert w.getsampwidth() == 2
        assert w.getframerate() == 24000
        assert w.getnframes() == 3
        assert w.readframes(3) == b"\x01\x00\x02\x00\x03\x00"


def test_render_places_sums_overlaps_clips_and_drops_past_the_end(tmp_path):
    import numpy as np

    raw = tmp_path / "raw"
    a = np.array([1000, 2000], dtype="<i2").tobytes()
    b = np.array([31000, 30000, 5], dtype="<i2").tobytes()
    raw.write_bytes(a + b)
    chunks = [
        chunk(0.0, conn=1, n_samples=2, offset=0),
        chunk(0.0, conn=2, n_samples=3, offset=len(a)),
    ]
    wav = tmp_path / "placed.wav"
    dw.render_placed_wav(str(raw), chunks, [1, 2], 4, str(wav))
    with wave.open(str(wav), "rb") as w:
        assert w.getnframes() == 4
        got = np.frombuffer(w.readframes(4), dtype="<i2").tolist()
    # [0, a0, a1+b0 (clipped), b1] — b2 falls past the end and is dropped.
    assert got == [0, 1000, 32767, 30000]


def test_render_an_empty_output_is_a_valid_empty_wav(tmp_path):
    raw = tmp_path / "raw"
    raw.write_bytes(b"")
    wav = tmp_path / "placed.wav"
    dw.render_placed_wav(str(raw), [], [], 0, str(wav))
    with wave.open(str(wav), "rb") as w:
        assert w.getnframes() == 0


def test_stream_filter_resamples_the_one_input_into_mix():
    assert dw.stream_filter() == "[0:a]aresample=48000[mix]"


# ── the superseded per-chunk work files ────────────────────────────────────────


def test_legacy_work_files_are_the_round_c_to_e2_chunk_cache():
    names = [
        "chunk_0.wav",
        "chunk_0.json",
        "chunk_12.in.pcm",
        "chunk_plan.json",
        "loudness.json",
        "heartbeat",
        "events.jsonl",
    ]
    # The work-dir `events.jsonl` is superseded by `<base>_dub_events.jsonl`
    # next to the dub (#184 H4): a stale one would mislead a timing analysis.
    assert dw.legacy_work_files(names) == [
        "chunk_0.json",
        "chunk_0.wav",
        "chunk_12.in.pcm",
        "chunk_plan.json",
        "events.jsonl",
    ]


def test_remove_legacy_work_files_deletes_only_them(tmp_path):
    for n in ("chunk_0.wav", "chunk_plan.json", "loudness.json"):
        (tmp_path / n).write_text("x")
    dw._remove_legacy_work_files(str(tmp_path))
    assert sorted(os.listdir(tmp_path)) == ["loudness.json"]


# ── the whole live-translate run, the session + ffmpeg seams faked ─────────────


def test_live_translate_places_the_stream_and_writes_the_d3_transcripts(
    tmp_path, monkeypatch, capsys
):
    import numpy as np

    work = tmp_path / "w"
    work.mkdir()
    (work / "chunk_3.wav").write_text("old")  # a superseded resume leftover
    (work / "events.jsonl").write_text("old")  # the pre-H4 event log location
    out = tmp_path / "x_dub.flac"
    events_file = tmp_path / "x_dub_events.jsonl"
    events_file.write_text('{"kind": "stale"}\n')  # a re-dub overwrites it
    transcripts = tmp_path / "x_dub_transcripts.json"

    # 3 s of input: 1 s of silence, then speech (the onset is at 1.0 s).
    silence = np.zeros(16000, dtype="<i2").tobytes()
    speech = np.full(32000, 4000, dtype="<i2").tobytes()
    monkeypatch.setattr(dw, "_decode_input", lambda audio: silence + speech)

    seen = {}

    def fake_session(frames, model, voice, work_dir, sink, events):
        seen.update(model=model, voice=voice, frames=len(frames))
        state = dls.SessionState(frames_sent=len(frames), t0_s=10.0)
        state.drain_end_reason = "quiet"
        for i, arrival in enumerate((14.0, 14.1)):  # first voiced 4.0 s after t0
            data = np.full(2400, 1000 + i, dtype="<i2").tobytes()
            state.chunks.append(
                dls.OutputChunk(
                    arrival_s=arrival,
                    conn=1,
                    offset=sink.append(data),
                    n_bytes=len(data),
                    voiced=True,
                    active=True,
                )
            )
        # The first input transcription arrives 3.5 s after t0.
        state.input_parts = [(13.5, "Hello ", 1), (14.2, "world", 1)]
        state.output_parts = [(14.0, "Ahoj ", 1), (14.5, "svet.", 1)]
        events.log("connect", connection=1)
        for _, text, conn in state.input_parts:
            events.log("input_transcription", connection=conn, text=text)
        for _, text, conn in state.output_parts:
            events.log("output_transcription", connection=conn, text=text)
        return state

    assembled = {}

    def fake_assemble(audio, wav, out_path, work_dir):
        with wave.open(wav, "rb") as w:
            assembled["frames"] = np.frombuffer(
                w.readframes(w.getnframes()), dtype="<i2"
            ).copy()
            assembled["rate"] = w.getframerate()
        assembled["audio"] = audio
        with open(out_path, "w") as f:
            f.write("DUB")
        return {}

    monkeypatch.setattr(dw, "_run_session", fake_session)
    monkeypatch.setattr(dw, "_assemble_dub", fake_assemble)
    args = dw.parse_args(
        [
            "live-translate",
            "--audio",
            "vocals.flac",
            "--out",
            str(out),
            "--transcripts",
            str(transcripts),
            "--work-dir",
            str(work),
        ]
    )
    dw.cmd_live_translate(args)

    assert seen == {"model": dw.MODEL, "voice": None, "frames": 30}
    # latency = (14.0 - 10.0) s - the 1.0 s input onset = 3000 ms.
    placed = assembled["frames"]
    assert assembled["rate"] == 24000
    assert assembled["audio"] == "vocals.flac"
    assert placed.size == 3 * 24000  # the input length
    assert (placed[:24000] == 0).all()  # (14.0-10.0)s - 3 s = 1.0 s on the video
    assert (placed[24000:26400] == 1000).all()
    assert (placed[26400:28800] == 1001).all()
    assert (placed[28800:] == 0).all()
    # The raw + placed intermediates and the superseded chunk cache are gone.
    # The event log moved next to the dub (#184 H4); the old one is gone too.
    assert sorted(os.listdir(work)) == [
        "heartbeat",
        "session_summary.json",
    ]
    lines = events_file.read_text(encoding="utf-8").splitlines()
    kinds = [json.loads(line)["kind"] for line in lines]
    assert "stale" not in kinds
    assert kinds.count("input_transcription") == 2
    assert kinds.count("output_transcription") == 2
    summary = json.loads((work / "session_summary.json").read_text())
    assert summary["latency_ms"] == 3000
    assert summary["measured_latency_ms"] == 3000
    assert summary["input_onset_ms"] == 1000
    # EN latency = (13.5 - 10.0) s - the 1.0 s input onset = 2500 ms.
    assert summary["en_latency_ms"] == 2500
    assert summary["en_measured_latency_ms"] == 2500
    assert summary["connections"] == 1
    t = json.loads(transcripts.read_text(encoding="utf-8"))
    assert t["chunks"][0]["en"] == "Hello world"
    # EN = input arrival - t0 (10.0) - the measured EN latency (2500 ms).
    assert t["chunks"][0]["en_timed"] == [
        {"t_ms": 1000, "text": "Hello "},
        {"t_ms": 1700, "text": "world"},
    ]
    assert t["chunks"][0]["sk"] == "Ahoj svet."
    assert t["chunks"][0]["end_ms"] == 3000
    assert t["chunks"][0]["sk_timed"] == [
        {"t_ms": 1000, "text": "Ahoj "},
        {"t_ms": 1500, "text": "svet."},
    ]
    captured = capsys.readouterr()
    stdout = json.loads(captured.out.strip().splitlines()[-1])
    assert stdout["out_path"] == str(out)
    assert stdout["transcripts_path"] == str(transcripts)
    assert stdout["session"]["latency_ms"] == 3000
    assert stdout["session"]["en_latency_ms"] == 2500
    # The summary line the SongPlayer log shows carries both latencies.
    assert "latency 3000 ms (measured 3000 ms" in captured.err
    assert "EN latency 2500 ms (measured 2500 ms)" in captured.err


def test_live_translate_with_no_voiced_output_fails_loudly(tmp_path, monkeypatch):
    import numpy as np

    monkeypatch.setattr(
        dw, "_decode_input", lambda audio: np.full(1600, 4000, "<i2").tobytes()
    )
    monkeypatch.setattr(
        dw,
        "_run_session",
        lambda *a: dls.SessionState(frames_sent=1, t0_s=1.0),
    )
    args = SimpleNamespace(
        audio="a.flac",
        out=str(tmp_path / "o.flac"),
        transcripts=str(tmp_path / "t.json"),
        work_dir=str(tmp_path / "w"),
        model=dw.MODEL,
        voice="speaker",
    )
    with pytest.raises(RuntimeError, match="no voiced output"):
        dw.cmd_live_translate(args)
    assert not os.path.exists(tmp_path / "o.flac")


# ── review round 1 ─────────────────────────────────────────────────────────────


def test_the_overlap_transcripts_are_ordered_by_connection_not_interleaved():
    # During the overlap the OLD connection's trailing fragments arrive after
    # the NEW one's first fragments; the old connection translates EARLIER
    # input, so its text comes first. Times stay non-decreasing — by capping
    # the OLD connection's late fragments at the new connection's first
    # fragment (review round 2), never by pushing the new connection's
    # subtitles later than its audio (its audio is placed at its arrival).
    parts = [
        (110.0, "Koniec ", 1),
        (110.4, "Nová ", 2),
        (110.6, "vety.", 1),
        (111.0, "veta.", 2),
    ]
    assert dw.sk_timed_from(parts, 100.0, 3000) == [
        {"t_ms": 7000, "text": "Koniec "},
        {"t_ms": 7400, "text": "vety."},
        {"t_ms": 7400, "text": "Nová "},
        {"t_ms": 8000, "text": "veta."},
    ]
    # Both transcriptions are `(arrival_s, text, conn)` since #184 H3.
    joined = dw.joined_by_connection(
        [(1.0, "B", 2), (1.1, "a ", 1), (1.2, "C", 2), (1.3, "b ", 1)]
    )
    assert joined == "a b BC"


def test_placement_drops_streamed_silence_until_a_stream_that_ran_ahead_is_back():
    sr = 24000
    # A 2 s voiced burst arrives at once at 105.0 (video 2.0 s), then the
    # stream continues in real time: silence, then speech. The burst leaves the
    # cursor 2 s AHEAD of arrival; the silence chunks are dropped until the lead
    # is within the tolerance, so the next speech is not 2 s late.
    burst = [chunk(105.0, n_samples=4800) for _ in range(10)]  # 10 x 0.2 s
    silence = [chunk(105.2 + i * 0.2, n_samples=4800, voiced=False) for i in range(9)]
    speech = [chunk(107.0, n_samples=2400)]
    starts = dw.place_output(burst + silence + speech, 100.0, 3000, sr)
    # The burst is one continuous stream: voiced audio is NEVER dropped.
    assert starts[:10] == [48_000 + i * 4800 for i in range(10)]
    kept_silence = [s for s in starts[10:19] if s is not None]
    assert len(kept_silence) < 9  # some streamed silence was skipped
    # The next speech lands within the tolerance of its arrival (4.0 s).
    lead_ms = (starts[19] - 96_000) * 1000 // sr
    assert 0 <= lead_ms <= dw.CATCH_UP_TOLERANCE_MS


def test_placement_keeps_silence_when_the_stream_is_on_time():
    chunks = [chunk(103.0 + i * 0.1, voiced=(i % 2 == 0)) for i in range(6)]
    starts = dw.place_output(chunks, 100.0, 3000, 24000)
    assert starts == [i * 2400 for i in range(6)]


def test_render_skips_dropped_chunks(tmp_path):
    import numpy as np

    raw = tmp_path / "raw"
    raw.write_bytes(np.array([7, 7, 9, 9], dtype="<i2").tobytes())
    chunks = [
        chunk(0.0, n_samples=2, offset=0),
        chunk(0.0, n_samples=2, offset=4, voiced=False),
    ]
    wav = tmp_path / "placed.wav"
    dw.render_placed_wav(str(raw), chunks, [0, None], 4, str(wav))
    with wave.open(str(wav), "rb") as w:
        got = np.frombuffer(w.readframes(4), dtype="<i2").tolist()
    assert got == [7, 7, 0, 0]


def test_a_failed_run_removes_the_raw_output(tmp_path, monkeypatch):
    import numpy as np

    monkeypatch.setattr(
        dw, "_decode_input", lambda audio: np.full(1600, 4000, "<i2").tobytes()
    )

    def dies_mid_session(frames, model, voice, work_dir, sink, events):
        sink.append(b"\x00\x10" * 2400)
        raise RuntimeError("the Live session was refused (1008)")

    monkeypatch.setattr(dw, "_run_session", dies_mid_session)
    work = tmp_path / "w"
    args = SimpleNamespace(
        audio="a.flac",
        out=str(tmp_path / "o.flac"),
        transcripts=str(tmp_path / "t.json"),
        work_dir=str(work),
        model=dw.MODEL,
        voice="speaker",
    )
    with pytest.raises(RuntimeError, match="refused"):
        dw.cmd_live_translate(args)
    assert not (work / dw.RAW_OUTPUT).exists()
    assert not (work / dw.PLACED_WAV).exists()
    # The evidence stays, next to where the dub would be (#184 H4).
    assert (tmp_path / "o_events.jsonl").exists()


# ── review round 2 ─────────────────────────────────────────────────────────────


def test_a_cleanup_failure_never_hides_the_real_error(tmp_path, monkeypatch):
    # On Windows a file still mapped by a failing render cannot be removed: the
    # PermissionError of the cleanup must not replace the ORIGINAL error.
    work = tmp_path / "w"
    work.mkdir()
    (work / dw.PLACED_WAV).write_bytes(b"x")

    def fails(args, raw_path, wav_path):
        raise RuntimeError("the real failure")

    def locked(path):
        raise PermissionError(13, "The process cannot access the file", path)

    monkeypatch.setattr(dw, "_translate", fails)
    monkeypatch.setattr(dw.os, "remove", locked)
    args = SimpleNamespace(
        audio="a.flac",
        out=str(tmp_path / "o.flac"),
        transcripts=str(tmp_path / "t.json"),
        work_dir=str(work),
        model=dw.MODEL,
        voice="speaker",
    )
    with pytest.raises(RuntimeError, match="the real failure"):
        dw.cmd_live_translate(args)


# ── review round 3 ─────────────────────────────────────────────────────────────


def test_the_failed_cleanup_is_logged(tmp_path, monkeypatch, capsys):
    path = tmp_path / dw.PLACED_WAV
    path.write_bytes(b"x")

    def locked(p):
        raise PermissionError(13, "The process cannot access the file", p)

    monkeypatch.setattr(dw.os, "remove", locked)
    dw._remove_intermediates(str(path))
    assert "could not remove the intermediate" in capsys.readouterr().err


def test_the_overlap_cap_covers_every_later_connection():
    # Connection 3's first text arrives before connection 2's (a middle
    # connection that only spoke late): every earlier connection's fragments
    # are capped at the EARLIEST later first-arrival, so connection 3 keeps its
    # own time (7500 ms) instead of being pushed to 8000 ms.
    parts = [
        (110.0, "a ", 1),
        (111.0, "b ", 1),
        (110.5, "N3 ", 3),
        (112.0, "late2 ", 2),
    ]
    assert dw.sk_timed_from(parts, 100.0, 3000) == [
        {"t_ms": 7000, "text": "a "},
        {"t_ms": 7500, "text": "b "},
        {"t_ms": 7500, "text": "late2 "},
        {"t_ms": 7500, "text": "N3 "},
    ]
