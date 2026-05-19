#!/usr/bin/env python3
"""assemblyai_universal_3_pro.py — AssemblyAI Universal-3 Pro flagship ASR.

Dedicated speech-to-text (NOT an audio-LLM): the model returns word-level
timestamps from its acoustic alignment directly, so timing precision is
intrinsic rather than emitted as `[mm:ss.mmm]` strings by an LLM.

Three-step API:
  1. POST /v2/upload  (raw bytes; returns short-lived upload_url)
  2. POST /v2/transcript  ({audio_url, speech_models: ["universal-3-pro"], ...})
  3. GET /v2/transcript/{id}  poll until status == "completed" | "error"

Line-grouping: AssemblyAI returns `words[]` only (no sentence/line
boundaries native). We split into lines on silence gaps > LINE_GAP_MS.

Pricing as of 2026-05-19: $0.21/hr (~$0.014 per 4-minute song) with
185 free hours per account.

Usage:
    python eval/lyrics/backends/assemblyai_universal_3_pro.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads ASSEMBLYAI_API_KEY from the environment.
"""

from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
from typing import Any

import requests

BACKEND_ID = "assemblyai-universal-3-pro"
BACKEND_REVISION = 1
API_BASE = "https://api.assemblyai.com/v2"
SPEECH_MODEL = "universal-3-pro"

# Silence gap threshold for "new line" — typical lyric line break.
# Most sung lines have <500 ms gaps within them and >800 ms between them.
LINE_GAP_MS = 800

# Poll cadence + overall timeout.
POLL_INTERVAL_S = 2.0
POLL_TIMEOUT_S = 1800  # 30 min, same as the Replicate whisperx wrapper


def auth_headers(token: str) -> dict[str, str]:
    """AssemblyAI uses raw token (no Bearer prefix)."""
    return {"authorization": token}


def upload_audio(audio_path: Path, token: str) -> str:
    """Stream raw bytes to /v2/upload and return the upload URL."""
    with audio_path.open("rb") as fh:
        r = requests.post(
            f"{API_BASE}/upload",
            headers=auth_headers(token),
            data=fh.read(),
            timeout=300,
        )
    r.raise_for_status()
    url = (r.json() or {}).get("upload_url")
    if not url:
        raise RuntimeError(
            f"assemblyai upload response missing upload_url: {r.text[:300]}"
        )
    return url


def create_transcript(audio_url: str, token: str) -> str:
    body = {
        "audio_url": audio_url,
        # speech_models (plural, list) replaces the deprecated singular field.
        "speech_models": [SPEECH_MODEL],
        # Punctuation + casing on so line text reads naturally.
        "punctuate": True,
        "format_text": True,
        # Don't bother with diarization for solo singers.
        "speaker_labels": False,
        # Language detection on — multi-language fixture needs it.
        "language_detection": True,
    }
    r = requests.post(
        f"{API_BASE}/transcript",
        headers={**auth_headers(token), "Content-Type": "application/json"},
        json=body,
        timeout=60,
    )
    r.raise_for_status()
    payload = r.json() or {}
    tid = payload.get("id")
    if not tid:
        raise RuntimeError(f"assemblyai transcript create missing id: {r.text[:300]}")
    return tid


def poll_transcript(transcript_id: str, token: str) -> dict[str, Any]:
    deadline = time.monotonic() + POLL_TIMEOUT_S
    url = f"{API_BASE}/transcript/{transcript_id}"
    while True:
        if time.monotonic() > deadline:
            raise RuntimeError(f"assemblyai poll timed out after {POLL_TIMEOUT_S}s")
        time.sleep(POLL_INTERVAL_S)
        r = requests.get(url, headers=auth_headers(token), timeout=60)
        r.raise_for_status()
        cur = r.json() or {}
        status = cur.get("status")
        if status == "completed":
            return cur
        if status == "error":
            raise RuntimeError(f"assemblyai transcript error: {cur.get('error')!r}")


def group_words_into_lines(words: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Split a flat word stream into lyric lines on silence gaps.

    A new line starts when the gap between the previous word's end and the
    current word's start exceeds LINE_GAP_MS milliseconds.
    """
    if not words:
        return []
    lines: list[dict[str, Any]] = []
    current_words: list[dict[str, Any]] = []
    prev_end = None
    for w in words:
        wt = (w.get("text") or "").strip()
        ws = w.get("start")
        we = w.get("end")
        if not wt or ws is None or we is None:
            continue
        if prev_end is not None and ws - prev_end > LINE_GAP_MS and current_words:
            lines.append(_flush_line(current_words))
            current_words = []
        current_words.append(
            {
                "text": wt,
                "start_ms": int(ws),
                "end_ms": int(we),
                "confidence": float(w.get("confidence") or 0.9),
            }
        )
        prev_end = we
    if current_words:
        lines.append(_flush_line(current_words))
    return lines


def _flush_line(words: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "text": " ".join(w["text"] for w in words),
        "start_ms": words[0]["start_ms"],
        "end_ms": words[-1]["end_ms"],
        "words": [
            {
                "text": w["text"],
                "start_ms": w["start_ms"],
                "end_ms": w["end_ms"],
                "confidence": w["confidence"],
            }
            for w in words
        ],
    }


def emit_result(
    *,
    out_path: Path,
    wav_path: str,
    duration_ms: int,
    lines: list[dict[str, Any]],
    raw_confidence: float,
    metadata: dict[str, Any],
) -> None:
    payload = {
        "backend_id": BACKEND_ID,
        "backend_revision": BACKEND_REVISION,
        "wav_path": wav_path,
        "duration_ms": duration_ms,
        "lines": lines,
        "raw_confidence": raw_confidence,
        "metadata": metadata,
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


def estimate_duration_ms(lines: list[dict[str, Any]]) -> int:
    if not lines:
        return 0
    return max(int(line["end_ms"]) for line in lines)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)

    token = os.environ.get("ASSEMBLYAI_API_KEY")
    if not token:
        print("ASSEMBLYAI_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    audio_url = upload_audio(args.wav, token)
    tid = create_transcript(audio_url, token)
    transcript = poll_transcript(tid, token)
    elapsed = time.time() - t0

    words = transcript.get("words") or []
    lines = group_words_into_lines(words)

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=float(transcript.get("confidence") or 0.9),
        metadata={
            "model": SPEECH_MODEL,
            "transcript_id": tid,
            "elapsed_s": round(elapsed, 1),
            "word_count": len(words),
            "line_count": len(lines),
            "line_gap_ms": LINE_GAP_MS,
            "language_code": transcript.get("language_code"),
            "audio_duration_s": transcript.get("audio_duration"),
        },
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
