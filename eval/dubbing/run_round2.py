#!/usr/bin/env python3
"""run_round2.py — assemble one round-2 candidate's mixes for the #175 listening test.

Round 2 (owner ruling on #174): only two mixes, NO same-colour blend —
`dub only` (SK dub + ambient) and `dub + original −18 dB` (SK dub + the original
voice attenuated to −18 dB + ambient). Works on either the fixed 7-sentence set
(`seg_spec` indices 2..8) or the full 34-sentence segment (finalists), selected
with `--indices`. Every output is loudness-normalized to −16 LUFS (the round-2
target the owner asked for; louder than the −14 used for the source segment so
the quiet-original mix stays intelligible).

Pipeline (reuses the committed round-1 helpers so timing behaviour is identical):
  1. select the seg_spec lines in `--indices`, re-base them to a window at t=0,
  2. measure each synthesized line's natural duration, plan with pure `fit.py`,
  3. tempo/resample + cut/fade each clip (`run_listening_test` helpers) to 48 kHz,
  4. lay the clips onto the dub track (`mix.build_dub_track`),
  5. slice the voice + ambient stems to the same window,
  6. build the two variants (`mix.build_variants(voice_under_db=-18)`),
  7. loudnorm each to −16 LUFS with ffmpeg, and emit a stats JSON.

Runtime only (dev1) — needs ffmpeg + numpy + soundfile; NOT run in CI (the
`eval-checks` job exercises the pure `fit.py` + `wavutil` + `voices` via tests).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
import soundfile as sf

from eval.dubbing import fit, mix
from eval.dubbing.run_listening_test import (
    WORK_SR,
    apply_cut_fade,
    ffmpeg_tempo_resample,
    wav_duration_ms,
)

TARGET_LUFS = -16.0
VOICE_UNDER_DB = -18.0
TAIL_MS = 500  # silence kept after the last selected line's slot


def parse_indices(spec: str, all_indices: list[int]) -> list[int]:
    """Parse an `--indices` selection: `all`, `2-8`, or `2,3,4`."""
    spec = spec.strip().lower()
    if spec in ("all", "*", ""):
        return list(all_indices)
    out: list[int] = []
    for part in spec.split(","):
        part = part.strip()
        if "-" in part:
            a, b = part.split("-", 1)
            out.extend(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
    return [i for i in out if i in set(all_indices)]


def loudnorm(in_path: str, out_path: str, target_lufs: float = TARGET_LUFS) -> None:
    """One-pass EBU R128 loudnorm to `target_lufs`, 48 kHz stereo, 16-bit WAV."""
    cmd = [
        "ffmpeg",
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        in_path,
        "-af",
        f"loudnorm=I={target_lufs}:TP=-1.5:LRA=11",
        "-ar",
        str(WORK_SR),
        "-ac",
        "2",
        "-c:a",
        "pcm_s16le",
        out_path,
    ]
    subprocess.run(cmd, check=True)


def _slice_stem(path: str, start_ms: int, end_ms: int) -> np.ndarray:
    """Read a stem and return the [start_ms, end_ms) window as 48 kHz stereo."""
    audio, sr = sf.read(path, dtype="float32", always_2d=True)
    if sr != WORK_SR:
        raise RuntimeError(f"{path}: sample rate {sr} != {WORK_SR}; resample first")
    a = mix.to_stereo(audio)
    s = mix.ms_to_samples(start_ms, WORK_SR)
    e = mix.ms_to_samples(end_ms, WORK_SR)
    window = a[s:e]
    want = e - s
    if window.shape[0] < want:  # pad if the window runs past the stem end
        pad = np.zeros((want - window.shape[0], 2), dtype=np.float32)
        window = np.concatenate([window, pad], axis=0)
    return window


def process(
    *,
    label: str,
    manifest_path: str,
    seg_spec: dict,
    indices: list[int],
    voice_stem: str,
    ambient_stem: str,
    out_dir: str,
    voice_db: float = VOICE_UNDER_DB,
    target_lufs: float = TARGET_LUFS,
) -> dict:
    manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    wav_by_index = {int(e["index"]): e["wav"] for e in manifest["lines"]}

    sel = [line for line in seg_spec["lines"] if int(line["index"]) in set(indices)]
    if not sel:
        raise RuntimeError(f"{label}: no seg_spec lines match indices {indices}")
    window_start = int(sel[0]["start_ms"])
    window_end = min(int(seg_spec["segment_end_ms"]), int(sel[-1]["end_ms"]) + TAIL_MS)
    window_dur = window_end - window_start

    plan_lines: list[dict] = []
    for line in sel:
        idx = int(line["index"])
        wav = wav_by_index.get(idx)
        if wav is None:
            raise RuntimeError(f"{label}: no synthesized wav for line {idx}")
        plan_lines.append(
            {
                "index": idx,
                "start_ms": int(line["start_ms"]) - window_start,
                "end_ms": int(line["end_ms"]) - window_start,
                "nat_dur_ms": wav_duration_ms(wav),
            }
        )

    placements, counts = fit.plan_segment(plan_lines, window_dur)

    clips: list[tuple[int, np.ndarray]] = []
    per_line: list[dict] = []
    with tempfile.TemporaryDirectory(prefix="dubr2_") as tmp:
        for p in placements:
            tempo_path = str(Path(tmp) / f"t_{p.index:03d}.wav")
            ffmpeg_tempo_resample(wav_by_index[p.index], tempo_path, p.tempo)
            audio, _ = sf.read(tempo_path, dtype="float32", always_2d=True)
            clip = apply_cut_fade(audio, p.placed_dur_ms, p.fade_ms)
            clips.append((p.at_ms, clip))
            per_line.append(p.as_dict())

    dub_track = mix.build_dub_track(clips, WORK_SR, window_dur)
    voice = _slice_stem(voice_stem, window_start, window_end)
    ambient = _slice_stem(ambient_stem, window_start, window_end)
    variants = mix.build_variants(dub_track, voice, ambient, voice_under_db=voice_db)

    # Only the two owner-approved mixes; no original-only, no same-colour blend.
    wanted = {"dub_only": "dub_only", "dub_plus_voice": "dub_plus_orig18"}
    variant_paths: dict[str, str] = {}
    with tempfile.TemporaryDirectory(prefix="dubr2mix_") as tmp:
        for src_name, out_name in wanted.items():
            raw = str(Path(tmp) / f"{out_name}.wav")
            sf.write(raw, variants[src_name], WORK_SR)
            final = str(out / f"{label}_{out_name}.wav")
            loudnorm(raw, final, target_lufs=target_lufs)
            variant_paths[out_name] = final

    speech_ms = sum(p["nat_dur_ms"] for p in per_line)
    speech_min = speech_ms / 60000.0
    total_synth_s = float(manifest.get("total_synth_s", 0.0))
    stats = {
        "label": label,
        "engine": manifest.get("engine"),
        "indices": indices,
        "window_ms": [window_start, window_end],
        "line_count": len(per_line),
        "fit_counts": dict(counts),
        "cut_lines": sum(1 for p in per_line if p["cut"]),
        "overflow_lines": sum(1 for p in per_line if p["overflow"]),
        "total_overflow_ms": sum(p["overflow_ms"] for p in per_line),
        "total_cut_ms": sum(p["cut_ms"] for p in per_line),
        "speech_minutes": round(speech_min, 3),
        "clone_s": manifest.get("clone_s"),
        "total_synth_s": round(total_synth_s, 2),
        "synth_latency_s_per_speech_min": (
            round(total_synth_s / speech_min, 2) if speech_min > 0 else None
        ),
        "variants": variant_paths,
    }
    (out / f"{label}_stats.json").write_text(
        json.dumps(stats, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    return stats


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--label", required=True, help="output basename, e.g. gemini_3_1_Kore"
    )
    p.add_argument("--manifest", required=True)
    p.add_argument("--seg-spec", required=True)
    p.add_argument("--indices", default="2-8", help="'all', '2-8', or '2,3,4'")
    p.add_argument("--voice-stem", required=True)
    p.add_argument("--ambient-stem", required=True)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--voice-db", type=float, default=VOICE_UNDER_DB)
    p.add_argument("--target-lufs", type=float, default=TARGET_LUFS)
    args = p.parse_args(argv)

    seg_spec = json.loads(Path(args.seg_spec).read_text(encoding="utf-8"))
    all_idx = [int(line["index"]) for line in seg_spec["lines"]]
    indices = parse_indices(args.indices, all_idx)
    stats = process(
        label=args.label,
        manifest_path=args.manifest,
        seg_spec=seg_spec,
        indices=indices,
        voice_stem=args.voice_stem,
        ambient_stem=args.ambient_stem,
        out_dir=args.out_dir,
        voice_db=args.voice_db,
        target_lufs=args.target_lufs,
    )
    print(json.dumps({k: stats[k] for k in ("label", "fit_counts", "variants")}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
