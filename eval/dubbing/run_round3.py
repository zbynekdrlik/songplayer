#!/usr/bin/env python3
"""run_round3.py — assemble a round-3 open-weight candidate's mixes for #175.

Round 3 renders the remaining VERIFIED open-weight Slovak candidates from the
round-2 Hugging Face discovery (OmniVoice, chatterbox-tts-slovak, F5_TTS_Slovak)
on the SAME 7 sentences (seg_spec indices 2..8) and the two finalists on the full
34-sentence segment, so the owner's verdict rests on a complete open-weight table.

The synthesis runs on the dev2 GPU (`ow_synth3.py`), writing `line_XXX.wav` +
`manifest.json` under `~/devel/dubbing-eval/ow/<model>/` with dev2 paths. This
driver runs on dev1: it re-points the manifest at the pulled-down copies
(`localize_manifest`) and then reuses round 2's generic, owner-approved mixer
(`run_round2.process`) — DRY: identical two-mix pipeline (`dub only` +
`dub + original −18 dB`, loudnorm −16 LUFS), no re-implementation.

Runtime only (dev1): needs numpy + soundfile + ffmpeg. NOT run in CI (the
`eval-checks` job exercises the pure `localize_manifest` + engine helpers + fit).
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from eval.dubbing import run_round2


def localize_manifest(manifest_path: str, wav_dir: str) -> dict:
    """Return the manifest with every line's `wav` re-pointed at `wav_dir`
    (`<wav_dir>/line_<index:03d>.wav`), because the manifest written on dev2
    carries dev2 paths but the mixer runs on dev1 against the pulled-down copies.

    Fails loudly if a referenced line wav is missing under `wav_dir` — a partial
    render must never silently produce a truncated mix.
    """
    manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
    wd = Path(wav_dir)
    for entry in manifest["lines"]:
        idx = int(entry["index"])
        local = wd / f"line_{idx:03d}.wav"
        if not local.exists():
            raise RuntimeError(f"localize_manifest: missing line wav {local}")
        entry["wav"] = str(local)
    return manifest


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--model-dir", required=True, help="dir with manifest.json + line wavs"
    )
    p.add_argument(
        "--label", required=True, help="output basename, e.g. chatterbox_sk_clone"
    )
    p.add_argument("--seg-spec", required=True)
    p.add_argument("--indices", default="2-8", help="'all', '2-8', or '2,3,4'")
    p.add_argument("--voice-stem", required=True)
    p.add_argument("--ambient-stem", required=True)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--voice-db", type=float, default=run_round2.VOICE_UNDER_DB)
    p.add_argument("--target-lufs", type=float, default=run_round2.TARGET_LUFS)
    args = p.parse_args(argv)

    model_dir = Path(args.model_dir)
    manifest = localize_manifest(str(model_dir / "manifest.json"), str(model_dir))
    local_manifest = model_dir / "manifest_dev1.json"
    local_manifest.write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    seg_spec = json.loads(Path(args.seg_spec).read_text(encoding="utf-8"))
    all_idx = [int(line["index"]) for line in seg_spec["lines"]]
    indices = run_round2.parse_indices(args.indices, all_idx)
    stats = run_round2.process(
        label=args.label,
        manifest_path=str(local_manifest),
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
