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
import re
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


def probe_duration_ms(audio: Path, ffmpeg: Path) -> int:
    """Probe audio duration using ffmpeg."""
    proc = subprocess.run(
        [str(ffmpeg), "-i", str(audio), "-f", "null", "-"],
        capture_output=True, text=True,
    )
    # Duration appears in combined output like: Duration: HH:MM:SS.ms
    combined = proc.stdout + proc.stderr
    match = re.search(r"Duration: (\d+):(\d+):(\d+\.\d+)", combined)
    if match:
        h, m, s = match.groups()
        total_s = int(h) * 3600 + int(m) * 60 + float(s)
        return int(total_s * 1000)
    raise ValueError(f"Could not parse duration from ffmpeg output.\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}")


def chunk_audio(vocal: Path, duration_ms: int, out_dir: Path, ffmpeg: Path) -> list[dict]:
    """Split vocal WAV into 60s chunks with 10s overlap. Returns list of chunk specs."""
    out_dir.mkdir(parents=True, exist_ok=True)
    chunks = []
    idx = 0
    start_ms = 0
    while start_ms < duration_ms:
        end_ms = min(start_ms + CHUNK_DURATION_S * 1000, duration_ms)
        name = f"chunk{idx:02d}_{start_ms}_{end_ms}.wav"
        path = out_dir / name
        subprocess.check_call([
            str(ffmpeg), "-y", "-loglevel", "error",
            "-ss", str(start_ms / 1000.0),
            "-t", str((end_ms - start_ms) / 1000.0),
            "-i", str(vocal),
            "-c:a", "pcm_s16le",
            str(path),
        ])
        chunks.append({"idx": idx, "start_ms": start_ms, "end_ms": end_ms, "path": path})
        idx += 1
        if end_ms >= duration_ms:
            break
        start_ms += CHUNK_STRIDE_S * 1000
    return chunks


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

    ffmpeg = DEFAULT_TOOLS / "ffmpeg.exe"
    duration_ms = probe_duration_ms(paths["audio"], ffmpeg)
    print(f"  duration:    {duration_ms} ms ({duration_ms/1000:.1f} s)")

    chunk_dir = cache / "gemini_chunks" / args.video_id
    chunks = chunk_audio(paths["vocal"], duration_ms, chunk_dir, ffmpeg)
    print(f"[chunk] produced {len(chunks)} chunks:")
    for c in chunks:
        print(f"  {c['idx']:>2}: {c['start_ms']/1000:>6.1f}s..{c['end_ms']/1000:>6.1f}s  {c['path'].name}")

    if args.dry_run:
        print("[dry-run] exiting before any API calls")
        return


if __name__ == "__main__":
    main()
