"""#184 round F — the dub is loudness-matched to the audio it translates.

Owner verdict 2026-09-23 (#184 comment 5793796815): "ked su pomery rovnake tak
dabingovi hlas je tichsi ako orginal!" — on video 344 the original and its vocals
stem measure -14.5 LUFS, the dub -16.1 LUFS, because the assembly normalized the
dub with a single-pass dynamic `loudnorm=I=-16` while the original is two-pass
`loudnorm I=-14`. Round F measures the INPUT's integrated loudness, clamps it to
-24..-10 LUFS and applies a two-pass LINEAR loudnorm to that target.

stdlib only (no ffmpeg in the eval-checks job): the pure helpers are tested
directly, and the three-call ffmpeg orchestration is tested with `_run_stderr`
monkeypatched to return captured loudnorm stderr.
"""

import importlib.util
import json
import math
import os
import sys

import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)


def _load_dub_worker():
    path = os.path.join(_SCRIPTS_DIR, "dub_worker.py")
    spec = importlib.util.spec_from_file_location("dub_worker_loudness", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dw = _load_dub_worker()


def _loudnorm_stderr(fields: dict, newline: str = "\n") -> str:
    """A captured-shape ffmpeg stderr: the `[Parsed_loudnorm_0 @ …]` header, the
    tab-indented JSON block of STRING values, then the trailing out/size lines."""
    body = ("," + newline).join(f'\t"{k}" : "{v}"' for k, v in fields.items())
    return (
        "[Parsed_loudnorm_0 @ 0x55d0c8a4e8c0] "
        + newline
        + "{"
        + newline
        + body
        + newline
        + "}"
        + newline
        + "[out#0/null @ 0x55d0c8a2f040] video:0KiB audio:56250KiB subtitle:0KiB"
        + newline
        + "size=N/A time=00:10:00.00 bitrate=N/A speed= 812x"
        + newline
    )


# A real `loudnorm=…:print_format=json` block measured on a speech input
# (values in ffmpeg's own string form).
SOURCE_BLOCK = {
    "input_i": "-14.52",
    "input_tp": "-0.87",
    "input_lra": "6.30",
    "input_thresh": "-24.71",
    "output_i": "-16.02",
    "output_tp": "-1.50",
    "output_lra": "5.80",
    "output_thresh": "-26.19",
    "normalization_type": "dynamic",
    "target_offset": "0.02",
}

MIX_BLOCK = {
    "input_i": "-17.84",
    "input_tp": "-2.10",
    "input_lra": "7.30",
    "input_thresh": "-28.12",
    "output_i": "-14.61",
    "output_tp": "-1.50",
    "output_lra": "6.90",
    "output_thresh": "-24.88",
    "normalization_type": "dynamic",
    "target_offset": "0.34",
}

APPLIED_BLOCK = {
    "input_i": "-17.84",
    "input_tp": "-2.10",
    "input_lra": "7.30",
    "input_thresh": "-28.12",
    "output_i": "-14.49",
    "output_tp": "-1.78",
    "output_lra": "7.30",
    "output_thresh": "-24.76",
    "normalization_type": "linear",
    "target_offset": "-0.03",
}


# ── loudness_target: the measured input loudness, clamped to -24..-10 LUFS ──


def test_loudness_target_follows_the_measured_input():
    assert dw.loudness_target(-14.52) == -14.52
    assert dw.loudness_target(-20.0) == -20.0


def test_loudness_target_clamps_a_very_quiet_input_to_minus_24():
    assert dw.loudness_target(-31.7) == -24.0
    assert dw.loudness_target(float("-inf")) == -24.0


def test_loudness_target_clamps_a_very_loud_input_to_minus_10():
    assert dw.loudness_target(-6.2) == -10.0
    assert dw.loudness_target(0.0) == -10.0


def test_loudness_target_keeps_the_exact_bounds():
    assert dw.loudness_target(-24.0) == -24.0
    assert dw.loudness_target(-10.0) == -10.0


def test_loudness_target_rejects_nan():
    with pytest.raises(ValueError):
        dw.loudness_target(float("nan"))


def test_loudness_bounds_are_minus_24_and_minus_10():
    assert (dw.DUB_LOUDNESS_MIN, dw.DUB_LOUDNESS_MAX) == (-24.0, -10.0)


# ── parse_loudnorm_json: the JSON block out of ffmpeg's stderr ──


def test_parse_loudnorm_json_reads_the_captured_block():
    got = dw.parse_loudnorm_json(_loudnorm_stderr(SOURCE_BLOCK))
    assert got["input_i"] == -14.52
    assert got["input_tp"] == -0.87
    assert got["input_lra"] == 6.30
    assert got["input_thresh"] == -24.71
    assert got["target_offset"] == 0.02
    assert got["output_i"] == -16.02
    assert got["normalization_type"] == "dynamic"


def test_parse_loudnorm_json_handles_windows_line_endings():
    got = dw.parse_loudnorm_json(_loudnorm_stderr(APPLIED_BLOCK, newline="\r\n"))
    assert got["input_i"] == -17.84
    assert got["output_i"] == -14.49
    assert got["normalization_type"] == "linear"


def test_parse_loudnorm_json_takes_the_loudnorm_block_not_an_earlier_brace():
    # Stream metadata printed before the filter output can contain braces; the
    # parser must return the loudnorm block (the one carrying `input_i`).
    noise = "Input #0, flac, from 'a.flac':\n  Metadata:\n    comment : {\"x\": 1}\n"
    got = dw.parse_loudnorm_json(noise + _loudnorm_stderr(MIX_BLOCK))
    assert got["input_i"] == -17.84
    assert got["target_offset"] == 0.34


def test_parse_loudnorm_json_reads_minus_inf_for_silence():
    block = dict(SOURCE_BLOCK, input_i="-inf", input_thresh="-inf")
    got = dw.parse_loudnorm_json(_loudnorm_stderr(block))
    assert got["input_i"] == float("-inf")


def test_parse_loudnorm_json_without_a_block_raises():
    with pytest.raises(ValueError):
        dw.parse_loudnorm_json("ffmpeg version 7.1\nsize=N/A time=00:00:01.00\n")


def test_parse_loudnorm_json_with_a_truncated_block_raises():
    block = {k: v for k, v in SOURCE_BLOCK.items() if k != "input_thresh"}
    with pytest.raises(ValueError):
        dw.parse_loudnorm_json(_loudnorm_stderr(block))


# ── the loudnorm filter strings ──


def test_loudnorm_analysis_filter_exact_string():
    assert dw.loudnorm_analysis_filter(-14.52) == (
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:print_format=json"
    )


def test_build_loudnorm_second_pass_exact_string():
    measured = dw.parse_loudnorm_json(_loudnorm_stderr(MIX_BLOCK))
    assert dw.build_loudnorm_second_pass(measured, -14.52) == (
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:"
        "measured_I=-17.84:measured_LRA=7.30:measured_TP=-2.10:"
        "measured_thresh=-28.12:offset=0.34:linear=true:print_format=json"
    )


def test_build_loudnorm_second_pass_formats_two_decimals():
    measured = {
        "input_i": -20.0,
        "input_lra": 11.456,
        "input_tp": -3.0,
        "input_thresh": -30.5,
        "target_offset": -0.004,
    }
    assert dw.build_loudnorm_second_pass(measured, -24.0) == (
        "loudnorm=I=-24.00:TP=-1.5:LRA=11:"
        "measured_I=-20.00:measured_LRA=11.46:measured_TP=-3.00:"
        "measured_thresh=-30.50:offset=-0.00:linear=true:print_format=json"
    )


def test_build_loudnorm_second_pass_rejects_a_silent_mix():
    measured = dw.parse_loudnorm_json(
        _loudnorm_stderr(dict(MIX_BLOCK, input_i="-inf", input_thresh="-inf"))
    )
    with pytest.raises(ValueError):
        dw.build_loudnorm_second_pass(measured, -14.52)


# ── argv builders ──


def test_loudness_measure_args_analyse_the_input_to_null():
    assert dw.loudness_measure_args("ffmpeg", "orig.flac") == [
        "ffmpeg",
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-i",
        "orig.flac",
        "-vn",
        "-af",
        "loudnorm=I=-16.00:TP=-1.5:LRA=11:print_format=json",
        "-f",
        "null",
        "-",
    ]


def test_assembly_args_analysis_pass_goes_to_null():
    args = dw.assembly_args("ffmpeg", ["c0.wav", "c1.wav"], "MIX", "LN", None)
    assert args == [
        "ffmpeg",
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-y",
        "-i",
        "c0.wav",
        "-i",
        "c1.wav",
        "-filter_complex",
        "MIX;[mix]LN[out]",
        "-map",
        "[out]",
        "-f",
        "null",
        "-",
    ]


def test_assembly_args_final_pass_writes_48k_stereo():
    args = dw.assembly_args("ffmpeg", ["c0.wav"], "MIX", "LN", "dub.flac")
    assert args[-7:] == [
        "-map",
        "[out]",
        "-ar",
        "48000",
        "-ac",
        "2",
        "dub.flac",
    ]
    assert "MIX;[mix]LN[out]" in args


# ── _assemble_dub: measure input → analyse the mix → apply linear 2nd pass ──


def test_assemble_dub_matches_the_dub_to_the_measured_input(tmp_path, monkeypatch):
    calls = []
    replies = [
        _loudnorm_stderr(SOURCE_BLOCK),
        _loudnorm_stderr(MIX_BLOCK),
        _loudnorm_stderr(APPLIED_BLOCK),
    ]

    def fake_run_stderr(args):
        calls.append(list(args))
        return replies[len(calls) - 1]

    monkeypatch.setattr(dw, "_run_stderr", fake_run_stderr)
    monkeypatch.setattr(dw, "_ffmpeg", lambda: "ff")
    work = str(tmp_path)
    wavs = [os.path.join(work, "chunk_0.wav"), os.path.join(work, "chunk_1.wav")]
    placements = [(1.0, 0), (1.05, 60_000)]
    out = os.path.join(work, "x_dub.flac")

    stats = dw._assemble_dub("orig.flac", wavs, placements, out, work)

    assert len(calls) == 3
    # 1. the INPUT (the audio the dub translates) is measured first.
    assert calls[0] == dw.loudness_measure_args("ff", "orig.flac")
    mix = dw.build_mix_filter(placements)
    # 2. the assembled mix is analysed against the measured target, to null.
    assert calls[1] == dw.assembly_args(
        "ff", wavs, mix, "loudnorm=I=-14.52:TP=-1.5:LRA=11:print_format=json", None
    )
    # 3. the linear second pass with the mix's measurements writes the dub.
    assert calls[2] == dw.assembly_args(
        "ff",
        wavs,
        mix,
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:"
        "measured_I=-17.84:measured_LRA=7.30:measured_TP=-2.10:"
        "measured_thresh=-28.12:offset=0.34:linear=true:print_format=json",
        out,
    )
    assert stats == {
        "source_i": -14.52,
        "target_i": -14.52,
        "mix_i": -17.84,
        "output_i": -14.49,
        "normalization_type": "linear",
    }
    # The stats are kept next to the chunks as box-side evidence.
    with open(os.path.join(work, "loudness.json"), encoding="utf-8") as f:
        assert json.load(f) == stats


def test_assemble_dub_clamps_a_quiet_input(tmp_path, monkeypatch):
    calls = []
    quiet = dict(SOURCE_BLOCK, input_i="-29.40")
    replies = [
        _loudnorm_stderr(quiet),
        _loudnorm_stderr(MIX_BLOCK),
        _loudnorm_stderr(APPLIED_BLOCK),
    ]

    def fake_run_stderr(args):
        calls.append(list(args))
        return replies[len(calls) - 1]

    monkeypatch.setattr(dw, "_run_stderr", fake_run_stderr)
    monkeypatch.setattr(dw, "_ffmpeg", lambda: "ff")
    work = str(tmp_path)
    stats = dw._assemble_dub(
        "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], "o.flac", work
    )
    assert stats["source_i"] == -29.4
    assert stats["target_i"] == -24.0
    assert (
        "loudnorm=I=-24.00:TP=-1.5:LRA=11:print_format=json"
        in calls[1][calls[1].index("-filter_complex") + 1]
    )
    assert math.isfinite(stats["output_i"])
