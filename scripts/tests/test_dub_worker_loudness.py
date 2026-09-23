"""#184 round F — the dub is loudness-matched to the audio it translates.

Owner verdict 2026-09-23 (#184 comment 5793796815): "ked su pomery rovnake tak
dabingovi hlas je tichsi ako orginal!" — on video 344 the original and its vocals
stem measure -14.5 LUFS, the dub -16.1 LUFS, because the assembly normalized the
dub with a single-pass dynamic `loudnorm=I=-16` while the original is two-pass
`loudnorm I=-14`. Round F measures the INPUT's integrated loudness, clamps it to
-24..-10 LUFS and applies a two-pass LINEAR loudnorm to that target.

The pure rules live in `scripts/dub_loudness.py` (shipped next to the worker);
the three-call ffmpeg orchestration is `dub_worker._assemble_dub`. stdlib only
(no ffmpeg in the eval-checks job): `_run_stderr` is monkeypatched to return
captured loudnorm stderr (and to create the file the real ffmpeg would write).
"""

import importlib.util
import json
import math
import os
import sys

import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# `scripts/` on the path so `dub_worker`'s `import dub_loudness` resolves the same
# way it does on the box (the child's own dir is on sys.path there).
if _SCRIPTS_DIR not in sys.path:
    sys.path.insert(0, _SCRIPTS_DIR)


def _load(name: str, alias: str):
    path = os.path.join(_SCRIPTS_DIR, f"{name}.py")
    spec = importlib.util.spec_from_file_location(alias, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dl = _load("dub_loudness", "dub_loudness_under_test")
dw = _load("dub_worker", "dub_worker_loudness")


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


# A `loudnorm=…:print_format=json` block measured on a speech input (values in
# ffmpeg's own string form).
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

# The assembled mix: +3.32 dB of gain to -14.52 lands its -5.50 dBTP peak at
# -2.18 dBTP, under the -1.5 ceiling, and LRA 7.30 <= 11 — so ffmpeg's linear
# conditions hold (`offset_tp <= target_tp`, `measured_lra <= target_lra`).
MIX_BLOCK = {
    "input_i": "-17.84",
    "input_tp": "-5.50",
    "input_lra": "7.30",
    "input_thresh": "-28.12",
    "output_i": "-14.61",
    "output_tp": "-2.20",
    "output_lra": "6.90",
    "output_thresh": "-24.88",
    "normalization_type": "dynamic",
    "target_offset": "0.34",
}

APPLIED_BLOCK = {
    "input_i": "-17.84",
    "input_tp": "-5.50",
    "input_lra": "7.30",
    "input_thresh": "-28.12",
    "output_i": "-14.49",
    "output_tp": "-2.18",
    "output_lra": "7.30",
    "output_thresh": "-24.76",
    "normalization_type": "linear",
    "target_offset": "-0.03",
}


# ── loudness_target: the measured input loudness, clamped to -24..-10 LUFS ──


def test_loudness_target_follows_the_measured_input():
    assert dl.loudness_target(-14.52) == -14.52
    assert dl.loudness_target(-20.0) == -20.0


def test_loudness_target_clamps_a_very_quiet_input_to_minus_24():
    assert dl.loudness_target(-31.7) == -24.0
    assert dl.loudness_target(float("-inf")) == -24.0


def test_loudness_target_clamps_a_very_loud_input_to_minus_10():
    assert dl.loudness_target(-6.2) == -10.0
    assert dl.loudness_target(0.0) == -10.0


def test_loudness_target_keeps_the_exact_bounds():
    assert dl.loudness_target(-24.0) == -24.0
    assert dl.loudness_target(-10.0) == -10.0


def test_loudness_target_rejects_nan():
    with pytest.raises(ValueError):
        dl.loudness_target(float("nan"))


def test_loudness_bounds_are_minus_24_and_minus_10():
    assert (dl.DUB_LOUDNESS_MIN, dl.DUB_LOUDNESS_MAX) == (-24.0, -10.0)


# ── parse_loudnorm_json: the JSON block out of ffmpeg's stderr ──


def test_parse_loudnorm_json_reads_the_captured_block():
    got = dl.parse_loudnorm_json(_loudnorm_stderr(SOURCE_BLOCK))
    assert got["input_i"] == -14.52
    assert got["input_tp"] == -0.87
    assert got["input_lra"] == 6.30
    assert got["input_thresh"] == -24.71
    assert got["target_offset"] == 0.02
    assert got["output_i"] == -16.02
    assert got["normalization_type"] == "dynamic"


def test_parse_loudnorm_json_handles_windows_line_endings():
    got = dl.parse_loudnorm_json(_loudnorm_stderr(APPLIED_BLOCK, newline="\r\n"))
    assert got["input_i"] == -17.84
    assert got["output_i"] == -14.49
    assert got["normalization_type"] == "linear"


def test_parse_loudnorm_json_skips_a_later_non_loudnorm_brace():
    # A `{…}` AFTER the loudnorm block that is not loudnorm JSON (e.g. a
    # metadata echo) must be skipped, not mistaken for the block.
    text = _loudnorm_stderr(MIX_BLOCK) + '    comment : {"x": 1}\n    note : {broken\n'
    got = dl.parse_loudnorm_json(text)
    assert got["input_i"] == -17.84
    assert got["target_offset"] == 0.34


def test_parse_loudnorm_json_reads_minus_inf_for_silence():
    block = dict(SOURCE_BLOCK, input_i="-inf", input_thresh="-inf")
    got = dl.parse_loudnorm_json(_loudnorm_stderr(block))
    assert got["input_i"] == float("-inf")


def test_parse_loudnorm_json_without_a_block_raises():
    with pytest.raises(ValueError):
        dl.parse_loudnorm_json("ffmpeg version 7.1\nsize=N/A time=00:00:01.00\n")


def test_parse_loudnorm_json_with_a_truncated_block_raises():
    block = {k: v for k, v in SOURCE_BLOCK.items() if k != "input_thresh"}
    with pytest.raises(ValueError):
        dl.parse_loudnorm_json(_loudnorm_stderr(block))


# ── the loudnorm filter strings ──


def test_loudnorm_analysis_filter_exact_string():
    assert dl.loudnorm_analysis_filter(-14.52) == (
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:print_format=json"
    )


def test_build_loudnorm_second_pass_exact_string():
    measured = dl.parse_loudnorm_json(_loudnorm_stderr(MIX_BLOCK))
    assert dl.build_loudnorm_second_pass(measured, -14.52) == (
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:"
        "measured_I=-17.84:measured_LRA=7.30:measured_TP=-5.50:"
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
    assert dl.build_loudnorm_second_pass(measured, -24.0) == (
        "loudnorm=I=-24.00:TP=-1.5:LRA=11:"
        "measured_I=-20.00:measured_LRA=11.46:measured_TP=-3.00:"
        "measured_thresh=-30.50:offset=-0.00:linear=true:print_format=json"
    )


def test_build_loudnorm_second_pass_rejects_a_silent_mix():
    measured = dl.parse_loudnorm_json(
        _loudnorm_stderr(dict(MIX_BLOCK, input_i="-inf", input_thresh="-inf"))
    )
    with pytest.raises(ValueError):
        dl.build_loudnorm_second_pass(measured, -14.52)


# ── argv builders + file helpers ──


def test_loudness_measure_args_analyse_the_input_to_null():
    assert dl.loudness_measure_args("ffmpeg", "orig.flac") == [
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
    args = dl.assembly_args("ffmpeg", ["c0.wav", "c1.wav"], "MIX", "LN", None, 48000)
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


def test_assembly_args_final_pass_writes_stereo_at_the_given_rate():
    args = dl.assembly_args("ffmpeg", ["c0.wav"], "MIX", "LN", "dub.flac", 48000)
    assert args[-7:] == ["-map", "[out]", "-ar", "48000", "-ac", "2", "dub.flac"]
    assert "MIX;[mix]LN[out]" in args


def test_partial_out_path_keeps_the_extension():
    # ffmpeg picks the muxer from the extension, so the partial keeps `.flac`.
    assert dl.partial_out_path("/c/x_dub.flac") == "/c/x_dub.part.flac"
    assert dl.partial_out_path("C:\\c\\y_dub.flac") == "C:\\c\\y_dub.part.flac"


def test_json_safe_stats_maps_non_finite_to_none():
    got = dl.json_safe_stats(
        {
            "source_i": float("-inf"),
            "target_i": -24.0,
            "mix_i": float("nan"),
            "output_i": None,
            "normalization_type": "linear",
        }
    )
    assert got == {
        "source_i": None,
        "target_i": -24.0,
        "mix_i": None,
        "output_i": None,
        "normalization_type": "linear",
    }
    # Strict JSON (no -Infinity / NaN tokens).
    json.dumps(got, allow_nan=False)


# ── _assemble_dub: measure input → analyse the mix → apply linear 2nd pass ──


def _fake_ffmpeg(monkeypatch, replies, fail_on=None):
    """Monkeypatch the worker's ffmpeg seam: `_run_stderr` returns the canned
    stderr per call and, like real ffmpeg, writes the output file of a non-null
    pass; `fail_on` (1-based) raises on that call. Returns the recorded argv."""
    calls = []

    def fake_run_stderr(args):
        calls.append(list(args))
        if fail_on == len(calls):
            raise RuntimeError("command failed (ff, rc=1): boom")
        if args[-1] != "-":
            with open(args[-1], "w", encoding="utf-8") as f:
                f.write("NEW")
        return replies[len(calls) - 1]

    heartbeats = []
    monkeypatch.setattr(dw, "_run_stderr", fake_run_stderr)
    monkeypatch.setattr(dw, "_ffmpeg", lambda: "ff")
    monkeypatch.setattr(
        dw, "_heartbeat", lambda work_dir: heartbeats.append(len(calls))
    )
    return calls, heartbeats


def test_assemble_dub_matches_the_dub_to_the_measured_input(tmp_path, monkeypatch):
    calls, heartbeats = _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(SOURCE_BLOCK),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(APPLIED_BLOCK),
        ],
    )
    work = str(tmp_path)
    wavs = [os.path.join(work, "chunk_0.wav"), os.path.join(work, "chunk_1.wav")]
    placements = [(1.0, 0), (1.05, 60_000)]
    out = os.path.join(work, "x_dub.flac")

    stats = dw._assemble_dub("orig.flac", wavs, placements, out, work)

    assert len(calls) == 3
    # 1. the INPUT (the audio the dub translates) is measured first.
    assert calls[0] == dl.loudness_measure_args("ff", "orig.flac")
    mix = dw.build_mix_filter(placements)
    # 2. the assembled mix is analysed against the measured target, to null.
    assert calls[1] == dl.assembly_args(
        "ff",
        wavs,
        mix,
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:print_format=json",
        None,
        dw.FINAL_SR,
    )
    # 3. the linear second pass with the mix's measurements writes the PARTIAL.
    assert calls[2] == dl.assembly_args(
        "ff",
        wavs,
        mix,
        "loudnorm=I=-14.52:TP=-1.5:LRA=11:"
        "measured_I=-17.84:measured_LRA=7.30:measured_TP=-5.50:"
        "measured_thresh=-28.12:offset=0.34:linear=true:print_format=json",
        dl.partial_out_path(out),
        dw.FINAL_SR,
    )
    # The finished partial replaced the dub; no partial is left behind.
    with open(out, encoding="utf-8") as f:
        assert f.read() == "NEW"
    assert not os.path.exists(dl.partial_out_path(out))
    # One heartbeat before each of the three full-length passes.
    assert heartbeats == [0, 1, 2]
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
    quiet = dict(SOURCE_BLOCK, input_i="-29.40")
    calls, _ = _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(quiet),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(APPLIED_BLOCK),
        ],
    )
    work = str(tmp_path)
    out = os.path.join(work, "o_dub.flac")
    stats = dw._assemble_dub(
        "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
    )
    assert stats["source_i"] == -29.4
    assert stats["target_i"] == -24.0
    assert (
        "loudnorm=I=-24.00:TP=-1.5:LRA=11:print_format=json"
        in calls[1][calls[1].index("-filter_complex") + 1]
    )
    assert math.isfinite(stats["output_i"])


def test_assemble_dub_failed_final_pass_keeps_the_previous_dub(tmp_path, monkeypatch):
    # A rebuild whose final pass dies must NOT destroy the good dub on disk.
    work = str(tmp_path)
    out = os.path.join(work, "x_dub.flac")
    with open(out, "w", encoding="utf-8") as f:
        f.write("OLD")
    _fake_ffmpeg(
        monkeypatch,
        [_loudnorm_stderr(SOURCE_BLOCK), _loudnorm_stderr(MIX_BLOCK)],
        fail_on=3,
    )
    with pytest.raises(RuntimeError):
        dw._assemble_dub(
            "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
        )
    with open(out, encoding="utf-8") as f:
        assert f.read() == "OLD"
    assert not os.path.exists(dl.partial_out_path(out))


def _three_passes(monkeypatch):
    return _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(SOURCE_BLOCK),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(APPLIED_BLOCK),
        ],
    )


def test_assemble_dub_promotes_the_partial_with_the_posix_rename(tmp_path, monkeypatch):
    # #184 round F2 (box 2026-09-23, comment 5795075881): `os.replace` =
    # MoveFileExW(REPLACE_EXISTING) fails with WinError 5 while SongPlayer holds
    # `_dub.flac` open (the video loaded in SP-dabing). The partial is promoted by
    # `win_replace.replace_file` (a POSIX-semantics rename on Windows), never by
    # `os.replace`.
    work = str(tmp_path)
    out = os.path.join(work, "x_dub.flac")
    with open(out, "w", encoding="utf-8") as f:
        f.write("OLD")
    _three_passes(monkeypatch)
    promoted = []

    def spy_replace_file(src, dst):
        promoted.append((src, dst))
        os.rename(src, dst)

    def no_os_replace(src, dst):
        raise AssertionError(f"os.replace({src!r}, {dst!r}) used for the dub")

    monkeypatch.setattr(dw.wr, "replace_file", spy_replace_file)
    monkeypatch.setattr(dw.os, "replace", no_os_replace)

    dw._assemble_dub(
        "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
    )
    monkeypatch.undo()

    assert promoted == [(dl.partial_out_path(out), out)]
    with open(out, encoding="utf-8") as f:
        assert f.read() == "NEW"
    assert not os.path.exists(dl.partial_out_path(out))


def test_assemble_dub_failed_promotion_keeps_the_previous_dub(tmp_path, monkeypatch):
    # The replace itself fails (e.g. a pre-1709 Windows without FileRenameInfoEx, or
    # a reader opened WITHOUT FILE_SHARE_DELETE): the run fails loudly with that
    # error, the previous good dub stays and the partial is removed.
    work = str(tmp_path)
    out = os.path.join(work, "x_dub.flac")
    with open(out, "w", encoding="utf-8") as f:
        f.write("OLD")
    _three_passes(monkeypatch)

    def denied(src, dst):
        raise PermissionError(13, "Access is denied", src, None, dst)

    monkeypatch.setattr(dw.wr, "replace_file", denied)
    with pytest.raises(PermissionError):
        dw._assemble_dub(
            "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
        )
    with open(out, encoding="utf-8") as f:
        assert f.read() == "OLD"
    assert not os.path.exists(dl.partial_out_path(out))


def test_assemble_dub_unparseable_final_report_keeps_the_previous_dub(
    tmp_path, monkeypatch
):
    # ffmpeg exited 0 but printed no loudnorm block: the partial is NOT promoted.
    work = str(tmp_path)
    out = os.path.join(work, "x_dub.flac")
    with open(out, "w", encoding="utf-8") as f:
        f.write("OLD")
    _fake_ffmpeg(
        monkeypatch,
        [_loudnorm_stderr(SOURCE_BLOCK), _loudnorm_stderr(MIX_BLOCK), "no json\n"],
    )
    with pytest.raises(ValueError):
        dw._assemble_dub(
            "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
        )
    with open(out, encoding="utf-8") as f:
        assert f.read() == "OLD"
    assert not os.path.exists(dl.partial_out_path(out))


def test_assemble_dub_warns_when_loudnorm_fell_back_to_dynamic(tmp_path, monkeypatch):
    logs = []
    _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(SOURCE_BLOCK),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(dict(APPLIED_BLOCK, normalization_type="dynamic")),
        ],
    )
    monkeypatch.setattr(dw, "_log", logs.append)
    work = str(tmp_path)
    stats = dw._assemble_dub(
        "orig.flac",
        [os.path.join(work, "chunk_0.wav")],
        [(1.0, 0)],
        os.path.join(work, "x_dub.flac"),
        work,
    )
    assert stats["normalization_type"] == "dynamic"
    assert any("WARNING" in m and "dynamic" in m for m in logs)
    # The summary line stays the LAST log line (the Rust side logs the tail).
    assert logs[-1].startswith("dub loudness: source")


def test_assemble_dub_linear_pass_logs_no_warning(tmp_path, monkeypatch):
    logs = []
    _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(SOURCE_BLOCK),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(APPLIED_BLOCK),
        ],
    )
    monkeypatch.setattr(dw, "_log", logs.append)
    work = str(tmp_path)
    dw._assemble_dub(
        "orig.flac",
        [os.path.join(work, "chunk_0.wav")],
        [(1.0, 0)],
        os.path.join(work, "x_dub.flac"),
        work,
    )
    assert not any("WARNING" in m for m in logs)


def test_assemble_dub_writes_strict_json_for_a_silent_source(tmp_path, monkeypatch):
    silent = dict(SOURCE_BLOCK, input_i="-inf", input_thresh="-inf")
    _fake_ffmpeg(
        monkeypatch,
        [
            _loudnorm_stderr(silent),
            _loudnorm_stderr(MIX_BLOCK),
            _loudnorm_stderr(APPLIED_BLOCK),
        ],
    )
    work = str(tmp_path)
    dw._assemble_dub(
        "orig.flac",
        [os.path.join(work, "chunk_0.wav")],
        [(1.0, 0)],
        os.path.join(work, "x_dub.flac"),
        work,
    )
    with open(os.path.join(work, "loudness.json"), encoding="utf-8") as f:
        text = f.read()
    assert "Infinity" not in text
    got = json.loads(text)
    assert got["source_i"] is None
    assert got["target_i"] == -24.0


def test_assemble_dub_clears_a_stale_partial_first(tmp_path, monkeypatch):
    # A child hard-killed mid final pass (stall timeout, server exit) never runs
    # the cleanup, leaving `<base>_dub.part.flac` behind; the next assembly must
    # remove it up front, even when that assembly itself fails early.
    work = str(tmp_path)
    out = os.path.join(work, "x_dub.flac")
    part = dl.partial_out_path(out)
    with open(part, "w", encoding="utf-8") as f:
        f.write("STALE")
    _fake_ffmpeg(monkeypatch, [], fail_on=1)
    with pytest.raises(RuntimeError):
        dw._assemble_dub(
            "orig.flac", [os.path.join(work, "chunk_0.wav")], [(1.0, 0)], out, work
        )
    assert not os.path.exists(part)
