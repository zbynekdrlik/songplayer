"""Tests for the #184 round-H continuous-session probe
(`eval/dubbing/live_translate_continuous_probe.py`) — the pure helpers
(pacing, framing, reconnect decision, gap / RMS / placement / summary math,
redaction, the Live config, the ffmpeg decode args) and `main()`. The session
loop against a fake Live server is in `test_live_translate_continuous_probe_session.py`.
No network and no google-genai installed.
"""

from __future__ import annotations

import json
import math
import pathlib
import struct
import subprocess
import sys

import pytest

from eval.dubbing import live_translate_continuous_probe as probe
from eval.dubbing.tests.live_fakes import FakeServer, probe_opts

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


def test_live_config_default_arm_is_speaker_copy_with_compression_and_resumption():
    cfg = probe.live_config(probe_opts(), handle=None)
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
    cfg = probe.live_config(probe_opts(voice="Charon"), handle=None)
    assert cfg["speech_config"] == {
        "voice_config": {"prebuilt_voice_config": {"voice_name": "Charon"}}
    }


def test_live_config_carries_the_resumption_handle():
    cfg = probe.live_config(probe_opts(), handle="h-42")
    assert cfg["session_resumption"] == {"handle": "h-42"}


def test_live_config_without_compression_or_resumption():
    cfg = probe.live_config(probe_opts(compression=False, resumption=False), handle="x")
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


def test_main_exits_1_when_no_connection_ever_opened(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("GEMINI_API_KEY", "ghostword")
    monkeypatch.setattr(probe, "decode_slice", lambda args: b"\x01\x00" * 3200)

    async def refused_live(args, key, frames, opts, events, state):
        await probe.run_probe(
            frames, FakeServer(["refuse"]).connect, lambda b: b, opts, events, state
        )

    monkeypatch.setattr(probe, "_run_live", refused_live)
    rc = probe.main(["--audio", "in.flac", "--out-dir", str(tmp_path / "o")])
    assert rc == 1
    summary = json.loads(capsys.readouterr().out)
    assert summary["connections"] == 0
    assert summary["frames_total"] == 2
    assert any("1008" in e for e in summary["errors"])
