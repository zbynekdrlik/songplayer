#!/usr/bin/env python3
"""Phase 0 prototype: generate _lyrics.json for a song via Gemini chunked transcription.

This script is experimental and throwaway. It reuses the existing Demucs-dereverbed
vocal WAV (or generates one via lyrics_worker.py preprocess-vocals) and calls Gemini 3
Pro via CLIProxyAPI's OpenAI-compatible endpoint to transcribe 60s chunks with 10s
overlap, then merges and writes a LyricsTrack-shaped JSON into the cache dir.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

CHUNK_DURATION_S = 60
CHUNK_OVERLAP_S = 10
CHUNK_STRIDE_S = CHUNK_DURATION_S - CHUNK_OVERLAP_S  # 50

DEFAULT_CACHE = Path(r"C:\ProgramData\SongPlayer\cache")
DEFAULT_TOOLS = DEFAULT_CACHE / "tools"
DEFAULT_PROXY = "http://127.0.0.1:18787"
DEFAULT_MODEL = "gemini-3-pro-preview"


def resolve_paths(cache_dir: Path, youtube_id: str) -> dict:
    """Locate the song's normalized audio + vocal WAV + description lyrics."""
    audio_glob = list(cache_dir.glob(f"*_{youtube_id}_normalized_audio.flac"))
    if not audio_glob:
        raise FileNotFoundError(f"no normalized audio for {youtube_id} in {cache_dir}")
    audio = audio_glob[0]
    vocal = cache_dir / f"{youtube_id}_vocals_dereverbed.wav"
    description = cache_dir / f"{youtube_id}_description_lyrics.json"
    return {"audio": audio, "vocal": vocal, "description": description}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--video-id", required=True, help="YouTube video id")
    ap.add_argument("--cache-dir", default=str(DEFAULT_CACHE))
    ap.add_argument("--proxy-url", default=DEFAULT_PROXY)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--dry-run", action="store_true", help="skip API calls, print plan")
    args = ap.parse_args()

    cache = Path(args.cache_dir)
    paths = resolve_paths(cache, args.video_id)
    print(f"[gemini_lyrics] video_id={args.video_id}")
    print(f"  audio:       {paths['audio']}")
    print(f"  vocal:       {paths['vocal']}  (exists={paths['vocal'].exists()})")
    print(f"  description: {paths['description']}  (exists={paths['description'].exists()})")
    print(f"  proxy:       {args.proxy_url}")
    print(f"  model:       {args.model}")
    if args.dry_run:
        print("[dry-run] exiting before any API calls")
        return


if __name__ == "__main__":
    main()
