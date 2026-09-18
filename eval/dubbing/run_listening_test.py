#!/usr/bin/env python3
"""run_listening_test.py — assemble one engine's dub from its synthesized lines.

Given the segment spec (EN sentence spans + SK translations + segment length),
one engine's per-line synthesized wavs (the `manifest.json` written by
`engines/base.py::synthesize_lines`), and the segment's voice + ambient stems,
this:

  1. measures each synthesized line's natural duration,
  2. plans placement with the pure `fit.py` policy (fit / tempo / overflow / cut),
  3. applies the plan to each clip (ffmpeg `atempo` for the ±15 % speed-up, a
     hard cut + fade for the cut branch), resampling to a common 48 kHz stereo,
  4. lays the clips onto the dub track (`mix.build_dub_track`),
  5. writes the three listening variants (`mix.build_variants`), and
  6. emits a stats JSON (timing-fit counts, latency per minute of speech).

Runtime only (dev1) — needs ffmpeg + numpy + soundfile; NOT run in CI (the
`eval-checks` job exercises the pure `fit.py` via `tests/test_fit.py`).
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

WORK_SR = 48000


def wav_duration_ms(path: str) -> int:
    info = sf.info(path)
    return int(round(info.frames * 1000.0 / info.samplerate))


def ffmpeg_tempo_resample(in_path: str, out_path: str, tempo: float) -> None:
    """Apply `atempo` (pitch-preserving speed change) and resample to WORK_SR
    stereo. `atempo` accepts 0.5–2.0, so our 1.0–1.15 range is a single stage."""
    cmd = [
        "ffmpeg",
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        in_path,
        "-af",
        f"atempo={tempo:.4f}",
        "-ar",
        str(WORK_SR),
        "-ac",
        "2",
        out_path,
    ]
    subprocess.run(cmd, check=True)


def apply_cut_fade(audio: np.ndarray, placed_dur_ms: int, fade_ms: int) -> np.ndarray:
    """Truncate to `placed_dur_ms` and apply a linear fade-out of `fade_ms`."""
    n = mix.ms_to_samples(placed_dur_ms, WORK_SR)
    a = mix.to_stereo(audio)[:n]
    if fade_ms > 0 and a.shape[0] > 0:
        f = min(mix.ms_to_samples(fade_ms, WORK_SR), a.shape[0])
        ramp = np.linspace(1.0, 0.0, f, dtype=np.float32)
        a = a.copy()
        a[-f:] *= ramp[:, None]
    return a


def process_engine(
    *,
    engine_name: str,
    manifest_path: str,
    seg_spec: dict,
    voice_stem: str,
    ambient_stem: str,
    out_dir: str,
) -> dict:
    manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)

    wav_by_index = {int(e["index"]): e["wav"] for e in manifest["lines"]}
    segment_end_ms = int(seg_spec["segment_end_ms"])

    plan_lines: list[dict] = []
    for line in seg_spec["lines"]:
        idx = int(line["index"])
        wav = wav_by_index.get(idx)
        if wav is None:
            raise RuntimeError(f"{engine_name}: no synthesized wav for line {idx}")
        plan_lines.append(
            {
                "index": idx,
                "start_ms": int(line["start_ms"]),
                "end_ms": int(line["end_ms"]),
                "nat_dur_ms": wav_duration_ms(wav),
            }
        )

    placements, counts = fit.plan_segment(plan_lines, segment_end_ms)

    clips: list[tuple[int, np.ndarray]] = []
    per_line: list[dict] = []
    with tempfile.TemporaryDirectory(prefix="dubfit_") as tmp:
        for p in placements:
            src = wav_by_index[p.index]
            tempo_path = str(Path(tmp) / f"t_{p.index:03d}.wav")
            ffmpeg_tempo_resample(src, tempo_path, p.tempo)
            audio, _ = sf.read(tempo_path, dtype="float32", always_2d=True)
            clip = apply_cut_fade(audio, p.placed_dur_ms, p.fade_ms)
            clips.append((p.at_ms, clip))
            per_line.append(p.as_dict())

    dub_track = mix.build_dub_track(clips, WORK_SR, segment_end_ms)
    voice = mix._read_stereo(voice_stem, WORK_SR)
    ambient = mix._read_stereo(ambient_stem, WORK_SR)
    variants = mix.build_variants(dub_track, voice, ambient)

    variant_paths: dict[str, str] = {}
    for name, audio in variants.items():
        vp = str(out / f"{engine_name}_{name}.wav")
        sf.write(vp, audio, WORK_SR)
        variant_paths[name] = vp

    speech_min = sum(p["nat_dur_ms"] for p in per_line) / 60000.0
    total_synth_s = float(manifest.get("total_synth_s", 0.0))
    stats = {
        "engine": engine_name,
        "line_count": len(per_line),
        "fit_counts": dict(counts),
        "cut_lines": sum(1 for p in per_line if p["cut"]),
        "overflow_lines": sum(1 for p in per_line if p["overflow"]),
        "total_overflow_ms": sum(p["overflow_ms"] for p in per_line),
        "total_cut_ms": sum(p["cut_ms"] for p in per_line),
        "max_tempo_used": max((p["tempo"] for p in per_line), default=1.0),
        "clone_s": manifest.get("clone_s"),
        "total_synth_s": round(total_synth_s, 2),
        "speech_minutes": round(speech_min, 3),
        "synth_latency_s_per_speech_min": (
            round(total_synth_s / speech_min, 2) if speech_min > 0 else None
        ),
        "variants": variant_paths,
        "placements": per_line,
    }
    (out / f"{engine_name}_stats.json").write_text(
        json.dumps(stats, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    return stats


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--engine-name", required=True)
    p.add_argument("--manifest", required=True, help="engine synthesis manifest.json")
    p.add_argument("--seg-spec", required=True, help="segment spec JSON (spans + sk)")
    p.add_argument("--voice-stem", required=True, help="original voice stem WAV @48k")
    p.add_argument("--ambient-stem", required=True, help="ambient stem WAV @48k")
    p.add_argument("--out-dir", required=True)
    args = p.parse_args(argv)

    seg_spec = json.loads(Path(args.seg_spec).read_text(encoding="utf-8"))
    stats = process_engine(
        engine_name=args.engine_name,
        manifest_path=args.manifest,
        seg_spec=seg_spec,
        voice_stem=args.voice_stem,
        ambient_stem=args.ambient_stem,
        out_dir=args.out_dir,
    )
    print(json.dumps({k: stats[k] for k in ("engine", "fit_counts", "variants")}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
