#!/usr/bin/env python3
"""dub_loudness.py — the loudness rules of the Slovak dub assembly (#184 round F).

Owner verdict 2026-09-23 (#184 comment 5793796815): with equal fader values the
dub played quieter than the original ("ked su pomery rovnake tak dabingovi hlas
je tichsi ako orginal!"). The dub is therefore normalized to the MEASURED
integrated loudness of the audio it translates (clamped to -24..-10 LUFS) with a
two-pass LINEAR ffmpeg `loudnorm`. `dub_worker._assemble_dub` runs the three
ffmpeg passes; everything here is pure (stdlib only, no I/O) and unit-tested in
`scripts/tests/test_dub_worker_loudness.py`. Shipped next to `dub_worker.py` by
the Rust worker (`dabing::worker::embedded_tool_scripts`).
"""

from __future__ import annotations

import json
import math
import os

# #184 round F: the dub is loudness-matched to the audio it translates (owner
# verdict 2026-09-23, #184 comment 5793796815: "ked su pomery rovnake tak dabingovi
# hlas je tichsi ako orginal!"). The target is the INPUT's measured integrated
# loudness, clamped to this range so a pathological input (near-silent, clipped)
# never produces an absurd dub level.
DUB_LOUDNESS_MIN = -24.0
DUB_LOUDNESS_MAX = -10.0
DUB_TRUE_PEAK = -1.5  # dBTP ceiling for the dub (unchanged from the old pass)
DUB_LRA = 11  # loudnorm LRA target (unchanged from the old pass)
# The input-measurement pass only needs `input_*`, which does not depend on the
# pass's own target; a fixed nominal target keeps its argv stable.
DUB_MEASURE_NOMINAL_I = -16.0
# Every value a two-pass loudnorm needs from the first pass's JSON block.
LOUDNORM_KEYS = ("input_i", "input_tp", "input_lra", "input_thresh", "target_offset")


def loudness_target(measured_i: float) -> float:
    """#184 round F: the dub's integrated-loudness target (LUFS) = the measured
    loudness of the audio it translates, clamped to
    `[DUB_LOUDNESS_MIN, DUB_LOUDNESS_MAX]`. Equal fader = equal loudness against
    the voice track the fader sits next to. `-inf` (a silent input) clamps to the
    floor; `NaN` is a broken measurement and raises. Pure — unit-tested."""
    if math.isnan(measured_i):
        raise ValueError("input loudness measurement is NaN")
    return min(DUB_LOUDNESS_MAX, max(DUB_LOUDNESS_MIN, measured_i))


def parse_loudnorm_json(stderr: str) -> dict:
    """Extract ffmpeg `loudnorm=…:print_format=json`'s JSON block from its stderr
    (the LAST `{…}` block that carries `input_i`; handles `\r\n`). Returns the
    `LOUDNORM_KEYS` as floats (`-inf` for silence), plus `output_i` (float) and
    `normalization_type` (str) when present. Raises `ValueError` when no complete
    block is found. Pure — unit-tested on captured stderr."""
    end = len(stderr)
    while True:
        start = stderr.rfind("{", 0, end)
        if start == -1:
            raise ValueError("no loudnorm JSON block in the ffmpeg output")
        close = stderr.find("}", start)
        if close != -1:
            try:
                block = json.loads(stderr[start : close + 1])
            except json.JSONDecodeError:
                block = None
            if isinstance(block, dict) and "input_i" in block:
                break
        end = start
    missing = [k for k in LOUDNORM_KEYS if k not in block]
    if missing:
        raise ValueError(f"loudnorm JSON block lacks {', '.join(missing)}")
    out = {k: float(block[k]) for k in LOUDNORM_KEYS}
    if "output_i" in block:
        out["output_i"] = float(block["output_i"])
    if "normalization_type" in block:
        out["normalization_type"] = str(block["normalization_type"])
    return out


def loudnorm_analysis_filter(target: float) -> str:
    """The first-pass (analysis) loudnorm for `target` LUFS — prints the JSON
    block `parse_loudnorm_json` reads. Pure — unit-tested."""
    return f"loudnorm=I={target:.2f}:TP={DUB_TRUE_PEAK}:LRA={DUB_LRA}:print_format=json"


def build_loudnorm_second_pass(measured: dict, target: float) -> str:
    """The second-pass loudnorm: `target` LUFS with the first pass's measurements
    in LINEAR mode (one constant gain — no dynamic compression, so the dub keeps
    its own dynamics and lands on the target). ffmpeg falls back to dynamic only
    if the gain would breach `TP` or the input LRA exceeds `LRA`; the applied
    `normalization_type` is logged by `_assemble_dub`. `print_format=json` so the
    applied pass reports its `output_i`. A non-finite measurement (a silent mix)
    raises. Pure — unit-tested on the exact string."""
    vals = [measured[k] for k in LOUDNORM_KEYS]
    if not all(math.isfinite(v) for v in vals):
        raise ValueError(
            f"the assembled dub mix has no measurable loudness: {measured}"
        )
    return (
        f"loudnorm=I={target:.2f}:TP={DUB_TRUE_PEAK}:LRA={DUB_LRA}:"
        f"measured_I={measured['input_i']:.2f}:"
        f"measured_LRA={measured['input_lra']:.2f}:"
        f"measured_TP={measured['input_tp']:.2f}:"
        f"measured_thresh={measured['input_thresh']:.2f}:"
        f"offset={measured['target_offset']:.2f}:linear=true:print_format=json"
    )


def loudness_measure_args(ffmpeg: str, audio: str) -> list[str]:
    """ffmpeg argv that measures `audio`'s integrated loudness (a loudnorm
    analysis pass to the null muxer). Pure — unit-tested."""
    return [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-i",
        audio,
        "-vn",
        "-af",
        loudnorm_analysis_filter(DUB_MEASURE_NOMINAL_I),
        "-f",
        "null",
        "-",
    ]


def assembly_args(
    ffmpeg: str,
    wavs: list[str],
    mix_filter: str,
    loudnorm: str,
    out: str | None,
    sample_rate: int,
) -> list[str]:
    """ffmpeg argv that lays the chunk WAVs on the timeline (`mix_filter`, the
    `build_mix_filter` graph ending in `[mix]`) and runs `loudnorm` on the mix.
    `out=None` → the analysis pass to the null muxer; else the final stereo
    dub file at `sample_rate`. Pure — unit-tested."""
    args = [ffmpeg, "-hide_banner", "-nostdin", "-nostats", "-y"]
    for w in wavs:
        args += ["-i", w]
    args += ["-filter_complex", f"{mix_filter};[mix]{loudnorm}[out]", "-map", "[out]"]
    if out is None:
        return args + ["-f", "null", "-"]
    return args + ["-ar", str(sample_rate), "-ac", "2", out]


def partial_out_path(out: str) -> str:
    """Where the final pass writes before it is promoted over `out` — the same
    directory and extension (ffmpeg picks the muxer from the extension), with a
    `.part` infix. Pure — unit-tested."""
    root, ext = os.path.splitext(out)
    return f"{root}.part{ext}"


def json_safe_stats(stats: dict) -> dict:
    """`stats` with every non-finite float (`-inf` for a silent source, `NaN`)
    replaced by `None`, so `loudness.json` is strict JSON. Pure — unit-tested."""
    return {
        k: (None if isinstance(v, float) and not math.isfinite(v) else v)
        for k, v in stats.items()
    }
