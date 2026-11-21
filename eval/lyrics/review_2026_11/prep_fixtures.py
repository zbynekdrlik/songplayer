#!/usr/bin/env python3
"""prep_fixtures.py — rebuild the fixture vocal WAVs on dev1 (#349).

The 2026-08/09 sweeps read ``<video_id>_vocal16k.wav`` from win-resolume's
eval-cache. The #349 review runs on dev1 and must not touch win-resolume,
so this rebuilds the same shape locally with the PRODUCTION chain as it is
today (#144): yt-dlp bestaudio -> Kim Mel-Band RoFormer vocals
(``vocals_mel_band_roformer.ckpt``, the stems worker's model) -> anvuew
dereverb (``dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt``, the
lyrics worker's model) -> 16 kHz mono float32, peak-clamped to 1.0.

It also keeps the full mix as ``<video_id>_mix16k.wav`` (16 kHz mono) for
models designed for full mixes.

Usage:
    python prep_fixtures.py --out-dir DIR --models-dir DIR [--only ID ...]
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

KIM_MODEL = "vocals_mel_band_roformer.ckpt"
DEREVERB_MODEL = "dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"
MANIFEST = Path(__file__).resolve().parents[1] / "manifest.json"
POISONED = "Xvm4_fWkXe8"


def fixture_ids(only: list[str]) -> list[str]:
    ids = [f["video_id"] for f in json.loads(MANIFEST.read_text())["fixtures"]]
    ids = [i for i in ids if i != POISONED]
    return [i for i in ids if not only or i in only]


def download(video_id: str, work: Path) -> Path:
    tmpl = str(work / f"{video_id}.%(ext)s")
    subprocess.run(
        [
            sys.executable,
            "-m",
            "yt_dlp",
            "-q",
            "-f",
            "bestaudio",
            "-x",
            "--audio-format",
            "wav",
            "-o",
            tmpl,
            f"https://www.youtube.com/watch?v={video_id}",
        ],
        check=True,
    )
    out = work / f"{video_id}.wav"
    if not out.exists():
        raise RuntimeError(f"yt-dlp produced no {out}")
    return out


def tokens(name: str) -> set[str]:
    """Every parenthesised stem token in an audio-separator output name.

    Match on these, never the raw name: Kim's model name itself contains
    'vocals', and the dereverb output of the vocals stem carries both
    '(vocals)' and '(reverb)' / '(noreverb)'."""
    return {t.lower() for t in re.findall(r"\(([^)]+)\)", name)}


def pick(files: list[str], wanted: str, base: Path) -> Path:
    for f in files:
        if wanted in tokens(Path(f).name):
            p = Path(f)
            return p if p.is_absolute() else base / p
    raise RuntimeError(f"no ({wanted}) stem in {files}")


def to_16k_mono(src: Path, dst: Path) -> None:
    import librosa
    import numpy as np
    import soundfile as sf

    audio, _ = librosa.load(str(src), sr=16000, mono=True)
    peak = float(np.max(np.abs(audio))) if audio.size else 0.0
    if peak > 1.0:
        audio = audio / peak
    sf.write(str(dst), audio, 16000, subtype="FLOAT", format="WAV")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--models-dir", type=Path, required=True)
    ap.add_argument("--only", nargs="*", default=[])
    args = ap.parse_args(argv)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    from audio_separator.separator import Separator

    work = Path(tempfile.mkdtemp(prefix="prep349_"))
    failed = []
    try:
        kim = Separator(
            model_file_dir=str(args.models_dir),
            output_dir=str(work),
            output_format="WAV",
            use_soundfile=True,
        )
        kim.load_model(KIM_MODEL)
        der = Separator(
            model_file_dir=str(args.models_dir),
            output_dir=str(work),
            output_format="WAV",
            use_soundfile=True,
        )
        der.load_model(DEREVERB_MODEL)
        for vid in fixture_ids(args.only):
            vocal = args.out_dir / f"{vid}_vocal16k.wav"
            mix = args.out_dir / f"{vid}_mix16k.wav"
            if vocal.exists() and mix.exists():
                print(f"{vid}: cached", flush=True)
                continue
            try:
                src = download(vid, work)
                to_16k_mono(src, mix)
                vocals = pick(kim.separate(str(src)), "vocals", work)
                dry = pick(der.separate(str(vocals)), "noreverb", work)
                to_16k_mono(dry, vocal)
                print(f"{vid}: ok", flush=True)
            except Exception as exc:  # reported per fixture and in the exit code
                failed.append(vid)
                print(f"{vid}: FAILED {exc!r}", flush=True)
            for f in work.iterdir():
                f.unlink()
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(f"done; failed={failed}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
