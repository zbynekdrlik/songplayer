"""Tests for backends/gemini38_flash.py — the #144 one-call arm.

CI's eval-checks job has NO google-genai installed: every test here drives the
backend through its injected `caller` / `slicer` seams (the model call is an
external network service — the only thing a test may fake), so importing the
module must never import `google.genai`.
"""

from __future__ import annotations

import json
import sys
import wave
from pathlib import Path

import pytest

from eval.lyrics.backends import gemini38_flash as g38


def _write_wav(path: Path, seconds: float, rate: int = 16_000) -> Path:
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(b"\x00\x00" * int(seconds * rate))
    return path


def _ok(lines: list[dict]) -> g38.CallResult:
    return g38.CallResult(
        text=json.dumps({"lines": lines}), finish_reason="STOP", usage=None
    )


# ── module import / constants ────────────────────────────────────────────────


def test_module_import_does_not_pull_google_genai() -> None:
    assert "google.genai" not in sys.modules


def test_default_model_and_labels() -> None:
    assert g38.DEFAULT_MODEL == "gemini-3.8-flash"
    assert g38.backend_label("whole") == "gemini38-flash-whole"
    assert g38.backend_label("win60") == "gemini38-flash-win60"
    with pytest.raises(ValueError):
        g38.backend_label("win30")


def test_response_schema_requires_the_four_line_fields() -> None:
    item = g38.RESPONSE_JSON_SCHEMA["properties"]["lines"]["items"]
    assert set(item["required"]) == {"text", "start_ms", "end_ms", "text_sk"}
    assert item["properties"]["start_ms"]["type"] == "integer"
    assert item["properties"]["text_sk"]["type"] == "string"
    assert g38.RESPONSE_JSON_SCHEMA["required"] == ["lines"]


def test_prompt_loads_between_markers() -> None:
    prompt = g38.load_prompt()
    assert "PROMPT-START" not in prompt
    assert "start_ms" in prompt and "text_sk" in prompt


def test_default_audio_path_uses_eval_cache_vocal16k() -> None:
    p = g38.default_audio_path(Path("C:/cache"), "abc123")
    assert p == Path("C:/cache") / "abc123_vocal16k.wav"


def test_key_pool_splits_csv_and_drops_blanks() -> None:
    assert g38.key_pool("k1, k2 ,,k3") == ["k1", "k2", "k3"]
    assert g38.key_pool("") == []
    assert g38.key_pool(None) == []


def test_redact_removes_every_key() -> None:
    assert g38.redact("bad key k2 at url?key=k1", ["k1", "k2"]) == (
        "bad key <redacted> at url?key=<redacted>"
    )


def test_wav_duration_ms(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "a.wav", 2.5)
    assert g38.wav_duration_ms(wav) == 2_500


# ── response parsing ─────────────────────────────────────────────────────────


def test_payload_to_lines_valid() -> None:
    payload = {
        "lines": [
            {"text": " Holy ", "start_ms": 1000, "end_ms": 2000, "text_sk": " Svätý "},
            {
                "text": "Worthy",
                "start_ms": "3000",
                "end_ms": 4000.0,
                "text_sk": "Hoden",
            },
        ]
    }
    lines, stats = g38.payload_to_lines(payload)
    assert lines == [
        {"text": "Holy", "start_ms": 1000, "end_ms": 2000, "text_sk": "Svätý"},
        {"text": "Worthy", "start_ms": 3000, "end_ms": 4000, "text_sk": "Hoden"},
    ]
    assert stats == {
        "n_raw": 2,
        "n_blank_text": 0,
        "n_invalid_timing": 0,
        "n_missing_sk": 0,
    }


def test_payload_to_lines_missing_timing_field_is_an_error() -> None:
    with pytest.raises(g38.ResponseError, match="start_ms"):
        g38.payload_to_lines(
            {"lines": [{"text": "Holy", "end_ms": 2000, "text_sk": "x"}]}
        )


def test_payload_to_lines_missing_text_field_is_an_error() -> None:
    with pytest.raises(g38.ResponseError, match="text"):
        g38.payload_to_lines({"lines": [{"start_ms": 1, "end_ms": 2, "text_sk": "x"}]})


def test_payload_to_lines_non_integer_timing_is_an_error() -> None:
    with pytest.raises(g38.ResponseError):
        g38.payload_to_lines(
            {"lines": [{"text": "a", "start_ms": "soon", "end_ms": 2, "text_sk": "x"}]}
        )


def test_payload_to_lines_missing_lines_array_is_an_error() -> None:
    with pytest.raises(g38.ResponseError, match="lines"):
        g38.payload_to_lines({"verses": []})


def test_payload_to_lines_drops_and_counts_impossible_timing_never_repairs() -> None:
    payload = {
        "lines": [
            {"text": "backwards", "start_ms": 5000, "end_ms": 4000, "text_sk": "x"},
            {"text": "negative", "start_ms": -10, "end_ms": 400, "text_sk": "x"},
            {"text": "ok", "start_ms": 6000, "end_ms": 7000, "text_sk": ""},
            {"text": "  ", "start_ms": 8000, "end_ms": 9000, "text_sk": "x"},
        ]
    }
    lines, stats = g38.payload_to_lines(payload)
    assert lines == [{"text": "ok", "start_ms": 6000, "end_ms": 7000, "text_sk": None}]
    assert stats["n_invalid_timing"] == 2
    assert stats["n_blank_text"] == 1
    assert stats["n_missing_sk"] == 1


def test_parse_response_text_rejects_non_json() -> None:
    with pytest.raises(g38.ResponseError):
        g38.parse_response_text("Sorry, I cannot help with that.")
    with pytest.raises(g38.ResponseError):
        g38.parse_response_text("")


def test_parse_response_text_tolerates_fence() -> None:
    assert g38.parse_response_text('```json\n{"lines": []}\n```') == {"lines": []}


def test_interpret_call_recitation_is_an_error() -> None:
    call = g38.CallResult(text=None, finish_reason="RECITATION", usage=None)
    with pytest.raises(g38.ResponseError, match="RECITATION"):
        g38.interpret_call(call)


def test_interpret_call_no_text_is_an_error() -> None:
    with pytest.raises(g38.ResponseError):
        g38.interpret_call(g38.CallResult(text=None, finish_reason="STOP", usage=None))


# ── run_fixture: whole ───────────────────────────────────────────────────────


def test_run_fixture_whole_valid(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 10.0)
    calls: list[Path] = []

    def caller(path: Path) -> g38.CallResult:
        calls.append(path)
        return _ok(
            [
                {"text": "Holy", "start_ms": 1000, "end_ms": 2000, "text_sk": "Svätý"},
                {"text": "ghost", "start_ms": 12_000, "end_ms": 13_000, "text_sk": "d"},
            ]
        )

    row = g38.run_fixture(
        video_id="vid",
        mode="whole",
        audio=wav,
        caller=caller,
        slicer=None,
        work_dir=tmp_path,
        model="gemini-3.8-flash",
    )
    assert calls == [wav]
    assert row["error"] is None
    assert row["backend_id"] == "gemini38-flash-whole"
    assert row["video_id"] == "vid"
    assert [ln["text"] for ln in row["lines"]] == ["Holy"]
    assert row["duration_ms"] == 10_000
    md = row["metadata"]
    assert md["model"] == "gemini-3.8-flash"
    assert md["audio_duration_ms"] == 10_000
    assert md["n_lines_past_audio_end"] == 1  # clipped out AND counted
    assert md["n_calls"] == 1


def test_run_fixture_whole_malformed_is_an_error_row_never_synthesized(
    tmp_path: Path,
) -> None:
    wav = _write_wav(tmp_path / "v.wav", 10.0)
    row = g38.run_fixture(
        video_id="vid",
        mode="whole",
        audio=wav,
        caller=lambda p: g38.CallResult(
            text="{not json", finish_reason="STOP", usage=None
        ),
        slicer=None,
        work_dir=tmp_path,
        model="m",
    )
    assert row["error"] and "JSON" in row["error"]
    assert row["lines"] == []


def test_run_fixture_whole_empty_lines_is_an_error_row(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 10.0)
    row = g38.run_fixture(
        video_id="vid",
        mode="whole",
        audio=wav,
        caller=lambda p: _ok([]),
        slicer=None,
        work_dir=tmp_path,
        model="m",
    )
    assert row["error"] and "empty" in row["error"]
    assert row["lines"] == []


def test_run_fixture_caller_exception_is_an_error_row(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 10.0)

    def boom(path: Path) -> g38.CallResult:
        raise RuntimeError("503 UNAVAILABLE")

    row = g38.run_fixture(
        video_id="vid",
        mode="whole",
        audio=wav,
        caller=boom,
        slicer=None,
        work_dir=tmp_path,
        model="m",
    )
    assert "503" in row["error"]
    assert row["lines"] == []


def test_run_fixture_missing_audio_is_an_error_row(tmp_path: Path) -> None:
    row = g38.run_fixture(
        video_id="vid",
        mode="whole",
        audio=tmp_path / "nope.wav",
        caller=lambda p: _ok([]),
        slicer=None,
        work_dir=tmp_path,
        model="m",
    )
    assert row["error"] and "nope.wav" in row["error"]


# ── run_fixture: win60 ───────────────────────────────────────────────────────


def test_run_fixture_win60_offsets_dedup_and_window_errors(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 130.0)  # windows 0-60, 55-115, 110-130
    sliced: list[tuple[int, int]] = []

    def slicer(src: Path, dst: Path, start_ms: int, end_ms: int) -> None:
        sliced.append((start_ms, end_ms))
        dst.write_bytes(b"x")

    responses = iter(
        [
            _ok(
                [
                    {
                        "text": "Holy is the Lord",
                        "start_ms": 56_000,
                        "end_ms": 58_000,
                        "text_sk": "a",
                    }
                ]
            ),
            _ok(
                [
                    {
                        "text": "holy is the lord",
                        "start_ms": 1_000,
                        "end_ms": 3_000,
                        "text_sk": "a",
                    },
                    {
                        "text": "Worthy",
                        "start_ms": 10_000,
                        "end_ms": 12_000,
                        "text_sk": "b",
                    },
                    # past THIS window's audio (60 s window) -> counted, dropped
                    {
                        "text": "ghost",
                        "start_ms": 61_000,
                        "end_ms": 62_000,
                        "text_sk": "c",
                    },
                ]
            ),
            g38.CallResult(text="garbage", finish_reason="STOP", usage=None),
        ]
    )
    row = g38.run_fixture(
        video_id="vid",
        mode="win60",
        audio=wav,
        caller=lambda p: next(responses),
        slicer=slicer,
        work_dir=tmp_path,
        model="m",
    )
    assert sliced == [(0, 60_000), (55_000, 115_000), (110_000, 130_000)]
    assert row["error"] is None  # one bad window does not void the good ones
    assert row["backend_id"] == "gemini38-flash-win60"
    assert [(ln["text"], ln["start_ms"]) for ln in row["lines"]] == [
        ("Holy is the Lord", 56_000),
        ("Worthy", 65_000),
    ]
    md = row["metadata"]
    assert md["n_windows"] == 3
    assert md["n_window_errors"] == 1
    assert md["n_duplicates_dropped"] == 1
    assert md["n_lines_past_audio_end"] == 1
    assert md["n_calls"] == 3
    assert [w["error"] is None for w in md["windows"]] == [True, True, False]
    assert md["windows"][1]["start_ms"] == 55_000


def test_run_fixture_win60_empty_window_is_not_an_error(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 50.0)

    def slicer(src: Path, dst: Path, start_ms: int, end_ms: int) -> None:
        dst.write_bytes(b"x")

    row = g38.run_fixture(
        video_id="vid",
        mode="win60",
        audio=wav,
        caller=lambda p: _ok([]),
        slicer=slicer,
        work_dir=tmp_path,
        model="m",
    )
    # an instrumental window legitimately has no lines; the whole song having
    # none is still reported as an error (nothing was transcribed)
    assert row["metadata"]["n_window_errors"] == 0
    assert row["error"] and "no lines" in row["error"]


def test_run_fixture_win60_all_windows_failed_is_an_error_row(tmp_path: Path) -> None:
    wav = _write_wav(tmp_path / "v.wav", 70.0)

    def slicer(src: Path, dst: Path, start_ms: int, end_ms: int) -> None:
        dst.write_bytes(b"x")

    row = g38.run_fixture(
        video_id="vid",
        mode="win60",
        audio=wav,
        caller=lambda p: g38.CallResult(
            text=None, finish_reason="RECITATION", usage=None
        ),
        slicer=slicer,
        work_dir=tmp_path,
        model="m",
    )
    assert row["error"] and "RECITATION" in row["error"]
    assert row["lines"] == []
    assert row["metadata"]["n_window_errors"] == 2


def test_ffmpeg_slice_command_uses_atrim() -> None:
    cmd = g38.ffmpeg_slice_cmd(
        "ffmpeg", Path("in.wav"), Path("out.wav"), 55_000, 115_000
    )
    assert cmd[0] == "ffmpeg"
    af = cmd[cmd.index("-af") + 1]
    assert af == "atrim=start=55.000:end=115.000,asetpts=PTS-STARTPTS"
    assert cmd[-1] == "out.wav"
