#!/usr/bin/env python3
"""base.py — the DubEngine protocol every dubbing engine implements, plus the
shared per-line synthesis driver and CLI wrapper.

A `DubEngine` is deliberately tiny (epic #174 D0 design): clone a voice from one
reference clip, then synthesize Slovak text with that cloned voice. The winner's
engine file becomes the reference for the D4 Rust `DubEngine` impl, so the
surface is kept minimal and explicit.

`synthesize_lines` clones once and synthesizes each numbered line to
`line_XXX.wav` under an output directory, timing each call, and writes a
`manifest.json` the runner reads back. `engine_cli` wraps that as a command-line
entry point so an engine that must run on another box (Chatterbox on the dev2
GPU) is invoked over ssh with exactly the same contract as the in-process one.
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path
from typing import Callable, Protocol, runtime_checkable


@runtime_checkable
class DubEngine(Protocol):
    """A text-to-speech engine that can clone a voice and speak Slovak with it."""

    name: str

    def clone_voice(self, sample_wav_path: str) -> str:
        """Clone the speaker from a short reference clip. Returns an opaque
        voice reference (a vendor voice id, or the sample path for a zero-shot
        engine) understood by this engine's `synthesize`."""
        ...

    def synthesize(self, text: str, voice_ref: str, lang: str = "sk") -> bytes:
        """Synthesize `text` in `lang` with the cloned voice. Returns WAV bytes."""
        ...


def synthesize_lines(
    engine: DubEngine,
    sample_wav_path: str,
    lines: list[dict],
    out_dir: str,
    lang: str = "sk",
) -> dict:
    """Clone the voice once, synthesize every line, and write the wavs + a
    manifest. `lines` is an ordered list of dicts with `index` and `text`
    (the Slovak sentence). Returns the manifest dict (also written to
    `<out_dir>/manifest.json`)."""
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)

    clone_t0 = time.monotonic()
    voice_ref = engine.clone_voice(sample_wav_path)
    clone_s = round(time.monotonic() - clone_t0, 2)

    entries: list[dict] = []
    total_synth_s = 0.0
    for line in lines:
        idx = int(line["index"])
        text = line["text"]
        wav_path = out / f"line_{idx:03d}.wav"
        t0 = time.monotonic()
        audio = engine.synthesize(text, voice_ref, lang=lang)
        synth_s = time.monotonic() - t0
        total_synth_s += synth_s
        wav_path.write_bytes(audio)
        entries.append(
            {
                "index": idx,
                "text": text,
                "wav": str(wav_path),
                "bytes": len(audio),
                "synth_s": round(synth_s, 3),
            }
        )

    manifest = {
        "engine": engine.name,
        "lang": lang,
        "sample_wav": sample_wav_path,
        "voice_ref": voice_ref,
        "clone_s": clone_s,
        "total_synth_s": round(total_synth_s, 2),
        "line_count": len(entries),
        "lines": entries,
    }
    (out / "manifest.json").write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    return manifest


def engine_cli(
    engine_factory: Callable[[], DubEngine], argv: list[str] | None = None
) -> int:
    """Shared CLI: `--sample <wav> --lines-json <json> --out-dir <dir>
    [--lang sk]`. `--lines-json` is a file containing an ordered list of
    `{index, text}` objects. Fails loudly (never swallows) — a synthesis error
    aborts with a non-zero exit."""
    p = argparse.ArgumentParser(description="Synthesize dub lines with a DubEngine.")
    p.add_argument("--sample", required=True, help="reference clip for voice cloning")
    p.add_argument("--lines-json", required=True, help="JSON list of {index, text}")
    p.add_argument("--out-dir", required=True, help="output directory for line wavs")
    p.add_argument("--lang", default="sk")
    args = p.parse_args(argv)

    lines = json.loads(Path(args.lines_json).read_text(encoding="utf-8"))
    engine = engine_factory()
    manifest = synthesize_lines(
        engine, args.sample, lines, args.out_dir, lang=args.lang
    )
    # Machine-readable line so the caller (possibly over ssh) can find the output.
    print(
        json.dumps(
            {
                "manifest": str(Path(args.out_dir) / "manifest.json"),
                "line_count": manifest["line_count"],
            }
        )
    )
    return 0
