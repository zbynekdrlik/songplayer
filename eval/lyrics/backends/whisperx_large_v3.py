#!/usr/bin/env python3
"""whisperx_replicate.py — baseline backend caller for /lyrics-eval.

Calls victor-upmeet/whisperx on Replicate, mirroring the pinned version
hash from `crates/sp-server/src/lyrics/whisperx_replicate.rs::WHISPERX_VERSION`.
Reads REPLICATE_API_TOKEN from the environment.

Usage:
    python eval/lyrics/backends/whisperx_replicate.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json \\
        [--language en]

Output JSON matches the backend-call contract documented in
`docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` §7.
"""

from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
from typing import Any

import requests

# Mirror crates/sp-server/src/lyrics/whisperx_replicate.rs::WHISPERX_VERSION.
WHISPERX_VERSION = "84d2ad2d6194fe98a17d2b60bef1c7f910c46b2f6fd38996ca457afd9c8abfcb"
BACKEND_ID = "whisperx-large-v3"
BACKEND_REVISION = 1
REPLICATE_BASE = "https://api.replicate.com/v1"

# Polling cadence for prediction status. ~2s matches the existing Rust client.
PREDICT_POLL_INTERVAL_S = 2.0
# Maximum total wall-clock wait for a single prediction.
# Mirrors crates/sp-server/src/lyrics/whisperx_replicate.rs::PREDICTION_TIMEOUT (1800s).
PREDICT_TIMEOUT_S = 1800


def build_predict_input(audio_url: str, language: str) -> dict[str, Any]:
    return {
        "audio_file": audio_url,
        "language": language,
        "align_output": True,
        "diarization": False,
        "batch_size": 32,
    }


def upload_file(wav_path: Path, token: str) -> str:
    """Upload local WAV to Replicate's file API and return the public URL."""
    with wav_path.open("rb") as fh:
        r = requests.post(
            f"{REPLICATE_BASE}/files",
            headers={"Authorization": f"Token {token}"},
            files={"content": (wav_path.name, fh, "audio/wav")},
            timeout=300,
        )
    r.raise_for_status()
    payload = r.json()
    url = (payload.get("urls") or {}).get("get")
    if not url:
        raise RuntimeError(f"replicate /files response missing urls.get: {payload!r}")
    return url


def run_prediction(audio_url: str, language: str, token: str) -> dict[str, Any]:
    body = {
        "version": WHISPERX_VERSION,
        "input": build_predict_input(audio_url, language),
    }
    r = requests.post(
        f"{REPLICATE_BASE}/predictions",
        headers={
            "Authorization": f"Token {token}",
            "Content-Type": "application/json",
        },
        json=body,
        timeout=60,
    )
    r.raise_for_status()
    pred = r.json()
    poll_url = pred["urls"]["get"]
    deadline = time.time() + PREDICT_TIMEOUT_S
    while True:
        time.sleep(PREDICT_POLL_INTERVAL_S)
        if time.time() > deadline:
            raise RuntimeError(
                f"replicate prediction timed out after {PREDICT_TIMEOUT_S}s"
            )
        rr = requests.get(
            poll_url,
            headers={"Authorization": f"Token {token}"},
            timeout=60,
        )
        rr.raise_for_status()
        cur = rr.json()
        status = cur.get("status")
        if status == "succeeded":
            output = cur.get("output")
            if output is None:
                raise RuntimeError("replicate prediction succeeded but output is null")
            return output
        if status in {"failed", "canceled"}:
            raise RuntimeError(f"replicate prediction {status}: {cur.get('error')!r}")


def parse_output(output: dict[str, Any]) -> list[dict[str, Any]]:
    """Parse Replicate's WhisperX output into the eval line shape.

    Each emitted line: {text, start_ms, end_ms, words: list | None}.
    Word objects: {text, start_ms, end_ms, confidence}. Words with
    missing start/end are dropped from the per-line list; if a segment
    produces no usable word timings, `words` is None on the line.
    """
    segments = output.get("segments")
    if not isinstance(segments, list):
        raise ValueError("output missing segments[]")
    lines: list[dict[str, Any]] = []
    for seg in segments:
        text = (seg.get("text") or "").strip()
        if not text:
            continue
        start_ms = int(round(float(seg["start"]) * 1000))
        end_ms = int(round(float(seg["end"]) * 1000))
        raw_words = seg.get("words") or []
        word_objs: list[dict[str, Any]] = []
        for w in raw_words:
            ws = w.get("start")
            we = w.get("end")
            if ws is None or we is None:
                continue
            word_objs.append(
                {
                    "text": (w.get("word") or "").strip(),
                    "start_ms": int(round(float(ws) * 1000)),
                    "end_ms": int(round(float(we) * 1000)),
                    "confidence": float(w.get("score") or 0.9),
                }
            )
        lines.append(
            {
                "text": text,
                "start_ms": start_ms,
                "end_ms": end_ms,
                "words": word_objs if word_objs else None,
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
    p.add_argument("--language", default="en")
    args = p.parse_args(argv)

    token = os.environ.get("REPLICATE_API_TOKEN")
    if not token:
        print("REPLICATE_API_TOKEN not set", flush=True)
        return 2

    t0 = time.time()
    audio_url = upload_file(args.wav, token)
    output = run_prediction(audio_url, args.language, token)
    lines = parse_output(output)
    elapsed = time.time() - t0

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        # Replicate's WhisperX has no top-level confidence field; hardcoded to 0.9
        # matching the Rust client's AlignedTrack default (unwrap_or(0.9)).
        raw_confidence=0.9,
        metadata={
            "model": "victor-upmeet/whisperx",
            "model_version": WHISPERX_VERSION,
            "elapsed_s": round(elapsed, 1),
            "segment_count": len(lines),
        },
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
