#!/usr/bin/env python3
"""Phase 0 prototype: generate _lyrics.json for a song via Gemini chunked transcription.

This script is experimental and throwaway. It reuses the existing Demucs-dereverbed
vocal WAV (or generates one via lyrics_worker.py preprocess-vocals) and calls Gemini 3
Pro via CLIProxyAPI's OpenAI-compatible endpoint to transcribe 60s chunks with 10s
overlap, then merges and writes a LyricsTrack-shaped JSON into the cache dir.
"""
import argparse
import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.request
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


def load_description_reference(description_path: Path) -> str:
    """Load clean description lyrics as a newline-joined reference block."""
    if not description_path.exists():
        return "(no description lyrics available for this song)"
    data = json.loads(description_path.read_text(encoding="utf-8"))
    lines = data.get("lines") or []
    return "\n".join(lines) if lines else "(description lyrics file empty)"


PROMPT_TEMPLATE = """You are a precise sung-lyrics transcription assistant. Your only output format is timed lines in this exact schema, one per line, nothing else:
(MM:SS.x --> MM:SS.x) text

Transcribe the sung vocals in the attached audio.

Rules:

1. Timestamps are LOCAL to this audio chunk, starting at 00:00. Do NOT offset.

2. COVERAGE — Output a timed line for EVERY sung phrase. Do NOT skip or collapse repeated choruses or refrains. If a phrase is sung 5 times, output 5 separate lines. Do not summarize.

3. SHORT LINES — Break long phrases into short, separately timed lines.
   - Break at every comma, semicolon, or breath pause.
   - Example: "To know Your heart, oh it's the goal of my life, it's the aim of my life" MUST be 3 separate lines:
     (07:23.0 --> 07:25.5) To know Your heart
     (07:26.0 --> 07:30.0) Oh it's the goal of my life
     (07:31.0 --> 07:34.0) It's the aim of my life
   - Aim for <= 8 words per output line where the phrasing allows.

4. PRECISION — Line start_time = the exact moment the first syllable BEGINS being sung (not the breath before, not a preceding beat). Line end_time = the last syllable finishes, before the next silence.

5. SILENCE — If the chunk has no vocals (instrumental only, or pre-roll silence), output exactly: # no vocals

6. OUTPUT FORMAT — Output ONLY timed lines. No intro text, no commentary, no markdown fences, no summary at the end.

7. DO NOT HALLUCINATE — Only transcribe what you actually hear. If you hear a word not matching the reference lyrics below, still write what you hear. If the reference has a line that doesn't appear in this audio chunk, do NOT include it.

Reference lyrics for this song (extracted from YouTube description — may be out of order, missing chorus repeats, or contain extra phrases not in this chunk):
{reference}

This chunk covers audio from {chunk_start_s:.1f}s to {chunk_end_s:.1f}s of the full song ({full_duration_s:.1f}s total). The chunk may start or end mid-phrase.
"""


def build_prompt(reference: str, chunk_start_ms: int, chunk_end_ms: int, full_duration_ms: int) -> str:
    return PROMPT_TEMPLATE.format(
        reference=reference,
        chunk_start_s=chunk_start_ms / 1000.0,
        chunk_end_s=chunk_end_ms / 1000.0,
        full_duration_s=full_duration_ms / 1000.0,
    )


def call_gemini(proxy_url: str, model: str, prompt: str, audio_path: Path, timeout_s: int = 60) -> str:
    """Call Gemini via CLIProxy /v1beta/models/{model}:generateContent. Returns raw text body."""
    audio_b64 = base64.b64encode(audio_path.read_bytes()).decode("ascii")
    body = {
        "contents": [{
            "parts": [
                {"text": prompt},
                {"inline_data": {"mime_type": "audio/wav", "data": audio_b64}},
            ]
        }],
        "generationConfig": {"temperature": 0.0},
    }
    url = f"{proxy_url}/v1beta/models/{model}:generateContent"
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read().decode("utf-8")
    doc = json.loads(raw)
    try:
        return doc["candidates"][0]["content"]["parts"][0]["text"]
    except (KeyError, IndexError) as e:
        raise RuntimeError(f"unexpected Gemini response shape: {raw[:500]}") from e


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--video-id", required=True, help="YouTube video id")
    ap.add_argument("--cache-dir", default=str(DEFAULT_CACHE))
    ap.add_argument("--proxy-url", default=DEFAULT_PROXY)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--dry-run", action="store_true", help="skip API calls, print plan")
    ap.add_argument("--one-chunk-test", type=int, default=None,
                    help="call Gemini on only this chunk index and print raw output")
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

    if args.one_chunk_test is not None:
        reference = load_description_reference(paths["description"])
        c = chunks[args.one_chunk_test]
        prompt = build_prompt(reference, c["start_ms"], c["end_ms"], duration_ms)
        print(f"[one-chunk-test] chunk {c['idx']} prompt length={len(prompt)} chars")
        out = call_gemini(args.proxy_url, args.model, prompt, c["path"])
        print(f"\n=== RAW GEMINI RESPONSE (chunk {c['idx']}) ===")
        print(out)
        print("=== END ===")
        return

    if args.dry_run:
        print("[dry-run] exiting before any API calls")
        return


if __name__ == "__main__":
    main()
