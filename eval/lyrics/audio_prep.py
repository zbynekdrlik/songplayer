#!/usr/bin/env python3
"""audio_prep.py — produce the cached 16 kHz mono vocal WAV for a video_id.

Eval helper. On first call for a given video_id, runs:

  1. yt-dlp to fetch the audio
  2. scripts/lyrics_worker.py preprocess-vocals (Mel-Roformer + anvuew
     dereverb, identical to production preprocess_vocals)

Subsequent calls return the cached path without re-running. Cache lives
on the host (win-resolume) at C:\\ProgramData\\SongPlayer\\eval-cache by
default. The cached WAV is the exact same shape /lyrics-eval backends
expect (16 kHz mono float32).

Usage:
    python eval/lyrics/audio_prep.py --video-id BW_vUblj_RA
    python eval/lyrics/audio_prep.py --video-id BW_vUblj_RA --force
    python eval/lyrics/audio_prep.py --video-id BW_vUblj_RA \\
        --cache-dir /custom/eval-cache --models-dir /custom/models
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

DEFAULT_CACHE_DIR = Path(r"C:\ProgramData\SongPlayer\eval-cache")
DEFAULT_MODELS_DIR = Path(r"C:\ProgramData\SongPlayer\cache\models")
REPO_ROOT = Path(__file__).resolve().parents[2]
PROD_HELPER = REPO_ROOT / "scripts" / "lyrics_worker.py"


def expected_cache_path(cache_dir: Path, video_id: str) -> Path:
    return cache_dir / f"{video_id}_vocal16k.wav"


def download_audio_with_ytdlp(video_id: str, work_dir: Path) -> Path:
    """yt-dlp the audio (bestaudio, extracted to WAV) and return the file path."""
    out_template = str(work_dir / f"{video_id}.%(ext)s")
    cmd = [
        "yt-dlp",
        "-q",
        "-f",
        "bestaudio",
        "-x",
        "--audio-format",
        "wav",
        "-o",
        out_template,
        f"https://www.youtube.com/watch?v={video_id}",
    ]
    subprocess.run(cmd, check=True)
    out = work_dir / f"{video_id}.wav"
    if not out.exists():
        raise RuntimeError(f"yt-dlp produced no output at {out}")
    return out


def run_preprocess_vocals(
    audio_path: Path, output_path: Path, models_dir: Path
) -> None:
    """Shell out to scripts/lyrics_worker.py preprocess-vocals.

    Matches production exactly. Output is 16 kHz mono float32 WAV.
    """
    if not PROD_HELPER.exists():
        raise RuntimeError(
            f"production helper not found at {PROD_HELPER} — eval needs prod tooling"
        )
    cmd = [
        sys.executable,
        str(PROD_HELPER),
        "preprocess-vocals",
        "--audio",
        str(audio_path),
        "--output",
        str(output_path),
        "--models-dir",
        str(models_dir),
    ]
    proc = subprocess.run(cmd, check=False, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise RuntimeError(
            f"preprocess-vocals failed (rc={proc.returncode}); see stderr above"
        )
    # Helper prints {"output": "..."} on success — parse so failures are obvious.
    try:
        last_line = proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else ""
        if last_line:
            parsed = json.loads(last_line)
            if parsed.get("output") and Path(parsed["output"]) != output_path:
                sys.stderr.write(
                    f"WARN: helper reported output={parsed['output']!r} "
                    f"but expected {output_path!r}\n"
                )
    except (json.JSONDecodeError, IndexError):
        # Stdout was not JSON. Not fatal as long as the output file exists.
        pass
    if not output_path.exists():
        raise RuntimeError(
            f"preprocess-vocals returned 0 but {output_path} does not exist"
        )


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--video-id", required=True)
    p.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE_DIR)
    p.add_argument("--models-dir", type=Path, default=DEFAULT_MODELS_DIR)
    p.add_argument("--force", action="store_true")
    args = p.parse_args(argv)

    cache_dir: Path = args.cache_dir
    cache_dir.mkdir(parents=True, exist_ok=True)
    target = expected_cache_path(cache_dir, args.video_id)

    if target.exists() and not args.force:
        print(str(target))
        return 0

    with tempfile.TemporaryDirectory(prefix="lyrics_eval_") as tmp_str:
        work = Path(tmp_str)
        audio = download_audio_with_ytdlp(args.video_id, work)
        run_preprocess_vocals(audio, target, args.models_dir)

    print(str(target))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
