#!/usr/bin/env python3
"""gemini_3_1_flash_lite.py — Google Gemini 3.1 Flash Lite via OpenRouter.

Audio-LLM transcription: receives the dereverbed vocal WAV + a user
instruction to transcribe with per-line start/stop timing, returns plain
text in `[mm:ss.mmm --> mm:ss.mmm] line` format. The wrapper parses
that back into the standard eval backend JSON shape.

OpenRouter pricing as of 2026-05-19: audio at $0.0000005/token —
~$0.006 per ~4 min song under typical token counts. Fast (~13 s
end-to-end on 14 MiB WAV).

Usage:
    python eval/lyrics/backends/gemini_3_1_flash_lite.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads OPENROUTER_API_KEY from the environment.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import time
from pathlib import Path
from typing import Any

import requests

BACKEND_ID = "gemini-3-1-flash-lite"
BACKEND_REVISION = 1
MODEL_SLUG = "google/gemini-3.1-flash-lite-20260507"
OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"

INSTRUCTION = (
    "Transcribe this song's lyrics. For each line of lyrics, emit one row "
    "in the exact format:\n"
    "[mm:ss.mmm --> mm:ss.mmm] line text\n"
    "Use timestamps relative to the start of the audio. Do not invent lyrics "
    "you cannot hear. Do not repeat the same line more than twice in a row "
    "unless the singer actually does so. Output only the timestamped lines, "
    "no preamble, no commentary."
)

LINE_RE = re.compile(
    r"^\[(\d{1,2}):(\d{2})\.(\d{3})\s*-->\s*(\d{1,2}):(\d{2})\.(\d{3})\]\s*(.+)$"
)


def encode_audio_base64(wav_path: Path) -> str:
    return base64.standard_b64encode(wav_path.read_bytes()).decode("ascii")


def build_request(audio_b64: str) -> dict[str, Any]:
    return {
        "model": MODEL_SLUG,
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_audio",
                        "input_audio": {"data": audio_b64, "format": "wav"},
                    },
                    {"type": "text", "text": INSTRUCTION},
                ],
            }
        ],
        # Long-song transcripts may run 200-1500 lines; 8192 leaves headroom.
        "max_tokens": 8192,
    }


def call_openrouter(body: dict[str, Any], token: str) -> dict[str, Any]:
    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
        "HTTP-Referer": "https://github.com/zbynekdrlik/songplayer",
        "X-Title": "songplayer-lyrics-eval",
    }
    r = requests.post(OPENROUTER_URL, headers=headers, json=body, timeout=600)
    if r.status_code != 200:
        raise RuntimeError(f"openrouter {r.status_code}: {r.text[:400]}")
    return r.json()


def _ts_to_ms(mm: str, ss: str, ms: str) -> int:
    return int(mm) * 60 * 1000 + int(ss) * 1000 + int(ms)


def parse_output(text: str) -> list[dict[str, Any]]:
    """Parse `[mm:ss.mmm --> mm:ss.mmm] text` rows into eval lines."""
    lines: list[dict[str, Any]] = []
    for raw in text.splitlines():
        m = LINE_RE.match(raw.strip())
        if not m:
            continue
        start_ms = _ts_to_ms(m.group(1), m.group(2), m.group(3))
        end_ms = _ts_to_ms(m.group(4), m.group(5), m.group(6))
        body = m.group(7).strip()
        if not body:
            continue
        lines.append(
            {
                "text": body,
                "start_ms": start_ms,
                "end_ms": end_ms,
                "words": None,
            }
        )
    return lines


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

    token = os.environ.get("OPENROUTER_API_KEY")
    if not token:
        print("OPENROUTER_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    audio_b64 = encode_audio_base64(args.wav)
    body = build_request(audio_b64)
    resp = call_openrouter(body, token)
    elapsed = time.time() - t0

    msg = resp.get("choices", [{}])[0].get("message", {})
    raw_text = msg.get("content") or ""
    lines = parse_output(raw_text)

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=0.9,
        metadata={
            "model": MODEL_SLUG,
            "elapsed_s": round(elapsed, 1),
            "segment_count": len(lines),
            "raw_text_len": len(raw_text),
            "usage": resp.get("usage"),
        },
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
