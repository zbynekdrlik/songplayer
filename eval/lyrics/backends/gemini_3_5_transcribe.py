#!/usr/bin/env python3
"""gemini_3_5_transcribe.py — Google Gemini 3.5 Transcribe, dedicated word-level ASR.

Dedicated speech-to-text (NOT the one-call audio-LLM family of
`gemini36_flash.py` / `gemini31_pro.py`): word-level timestamps come from
the model's own transcription-mode output (`timestamp_granularities:
["word"]`), same family of guarantee as `assemblyai_universal_3_pro.py` and
`soniox_v5.py` — not LLM-emitted `[mm:ss.mmm]` strings inside free text.

Verified against the LIVE API on 2026-09-12 from the win-resolume box; trust
this over the vendor docs (see `.claude/rules/lyrics-eval-backends.md`).

Three-step API, base `https://generativelanguage.googleapis.com`:
  1. POST /upload/v1beta/files      (raw bytes; returns file resource, state
                                      PROCESSING|ACTIVE)
  2. GET  /v1beta/{file.name}       poll until state != PROCESSING
  3. POST /v1beta/interactions      ({model, input: [{type: audio, uri,
                                      mime_type}], generation_config:
                                      {transcription_config: {...}}})
  (+ DELETE /v1beta/{file.name} — best-effort cleanup so uploads don't
   accumulate)
Auth: `x-goog-api-key: <GEMINI_API_KEY>` header on every call (not a Bearer
token, not a query param).

Response shape: top-level `id, status ("completed"), usage, created,
updated, service_tier, steps, object, model`. Words live at
`steps[*].content[*].annotations[*]` with
`{"type": "word_info", "text": ..., "start_offset": "5.200s",
"end_offset": "9s"}` — offsets are STRINGS with a trailing `s`, sometimes
with no decimal point (`"9s"`). There is no per-word confidence field, so
every emitted word carries `confidence: None` and `raw_confidence` is also
`None` (never fabricated).

Line-grouping: same silence-gap heuristic as `assemblyai_universal_3_pro.py`
/ `soniox_v5.py` (LINE_GAP_MS=400) so results are directly comparable.

Limits encoded here (per the verified API, not a doc guess):
  - No custom vocabulary — incompatible with word-level timestamps.
  - `language_codes` defaults to `["en-US"]`; override via env
    `G35T_LANGUAGE_CODES` (comma-separated), or set it to the literal
    string `auto` for language auto-detection (empty list on the wire).

Usage:
    python eval/lyrics/backends/gemini_3_5_transcribe.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads GEMINI_API_KEY from the environment. Never printed, never placed on
argv.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import time
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("lyrics_eval.gemini_3_5_transcribe")

BACKEND_ID = "gemini-3-5-transcribe"
BACKEND_REVISION = 1
MODEL_SLUG = "gemini-3.5-transcribe"

API_ROOT = "https://generativelanguage.googleapis.com"
FILES_UPLOAD_ENDPOINT = f"{API_ROOT}/upload/v1beta/files"
FILES_API_ROOT = f"{API_ROOT}/v1beta"
INTERACTIONS_ENDPOINT = f"{API_ROOT}/v1beta/interactions"

AUDIO_MIME_TYPE = "audio/wav"

DEFAULT_LANGUAGE_CODES = ["en-US"]

# Silence-gap threshold for "new line" — matches assemblyai_universal_3_pro.py
# / soniox_v5.py rather than inventing a new threshold, per this project's
# established convention (lrclib line cadence / wall tolerance).
LINE_GAP_MS = 400

# File-processing poll cadence + overall timeout.
FILE_POLL_INTERVAL_S = 2.0
FILE_POLL_TIMEOUT_S = 60.0

# Retry policy for transient upload/transcribe failures (429 rate-limit,
# 5xx server errors): 4 attempts total, exponential backoff starting at 2s.
RETRY_MAX_ATTEMPTS = 4
RETRY_BACKOFF_BASE_S = 2.0


def auth_headers(api_key: str) -> dict[str, str]:
    """Gemini uses the x-goog-api-key header (not Bearer, not a query param)."""
    return {"x-goog-api-key": api_key}


def _is_retryable_status(status_code: int) -> bool:
    return status_code == 429 or status_code >= 500


def request_with_retry(
    method: str,
    url: str,
    *,
    headers: dict[str, str],
    max_attempts: int = RETRY_MAX_ATTEMPTS,
    **kwargs: Any,
) -> requests.Response:
    """Issue an HTTP request, retrying on 429/5xx with exponential backoff
    (2, 4, 8, 16s). Any other 4xx raises immediately with the response body
    (the caller's headers never contain the API key in a loggable form —
    this function never logs `headers`)."""
    attempt = 0
    while True:
        attempt += 1
        r = requests.request(method, url, headers=headers, **kwargs)
        if r.status_code < 400:
            return r
        if _is_retryable_status(r.status_code) and attempt < max_attempts:
            backoff = RETRY_BACKOFF_BASE_S * (2 ** (attempt - 1))
            logger.warning(
                "gemini_3_5_transcribe %s %s failed: status=%d attempt=%d/%d — "
                "retrying in %.0fs",
                method,
                url,
                r.status_code,
                attempt,
                max_attempts,
                backoff,
            )
            time.sleep(backoff)
            continue
        if _is_retryable_status(r.status_code):
            logger.error(
                "gemini_3_5_transcribe %s %s exhausted retries: status=%d body=%s",
                method,
                url,
                r.status_code,
                r.text[:400],
            )
        else:
            logger.error(
                "gemini_3_5_transcribe %s %s failed: status=%d body=%s",
                method,
                url,
                r.status_code,
                r.text[:400],
            )
        raise RuntimeError(
            f"gemini_3_5_transcribe {method} {url} {r.status_code}: {r.text[:400]}"
        )


def upload_audio(wav_path: Path, api_key: str) -> dict[str, Any]:
    """POST the raw WAV bytes to the Files API and return the file resource
    dict (`name`, `uri`, `mimeType`, `state`, ...)."""
    size = wav_path.stat().st_size
    logger.info(
        "gemini_3_5_transcribe upload starting: path=%s bytes=%d", wav_path, size
    )
    with wav_path.open("rb") as fh:
        r = request_with_retry(
            "POST",
            FILES_UPLOAD_ENDPOINT,
            headers={
                **auth_headers(api_key),
                "X-Goog-Upload-Protocol": "raw",
                "X-Goog-Upload-Header-Content-Type": AUDIO_MIME_TYPE,
                "Content-Type": AUDIO_MIME_TYPE,
            },
            data=fh.read(),
            timeout=300,
        )
    payload = r.json() or {}
    file_info = payload.get("file") or {}
    name = file_info.get("name")
    if not name:
        raise RuntimeError(
            f"gemini_3_5_transcribe upload response missing file.name: {r.text[:400]}"
        )
    logger.info(
        "gemini_3_5_transcribe upload ok: name=%s state=%s",
        name,
        file_info.get("state"),
    )
    return file_info


def poll_file_ready(file_name: str, api_key: str) -> dict[str, Any]:
    """Poll GET /v1beta/{file.name} until state != PROCESSING."""
    deadline = time.monotonic() + FILE_POLL_TIMEOUT_S
    url = f"{FILES_API_ROOT}/{file_name}"
    poll_count = 0
    while True:
        r = request_with_retry("GET", url, headers=auth_headers(api_key), timeout=30)
        info = r.json() or {}
        state = info.get("state")
        poll_count += 1
        logger.debug(
            "gemini_3_5_transcribe file poll #%d: name=%s state=%s",
            poll_count,
            file_name,
            state,
        )
        if state != "PROCESSING":
            break
        if time.monotonic() > deadline:
            raise RuntimeError(
                f"gemini_3_5_transcribe file {file_name} still PROCESSING after "
                f"{FILE_POLL_TIMEOUT_S}s"
            )
        time.sleep(FILE_POLL_INTERVAL_S)
    if state == "FAILED":
        raise RuntimeError(f"gemini_3_5_transcribe file processing FAILED: {info}")
    logger.info(
        "gemini_3_5_transcribe file %s ready after %d poll(s): state=%s",
        file_name,
        poll_count,
        state,
    )
    return info


def resolve_language_codes() -> list[str]:
    """Read G35T_LANGUAGE_CODES from the env (comma-separated), default
    DEFAULT_LANGUAGE_CODES. The literal value "auto" means language
    auto-detection — an empty list on the wire."""
    raw = os.environ.get("G35T_LANGUAGE_CODES")
    if raw is None:
        return list(DEFAULT_LANGUAGE_CODES)
    if raw.strip().lower() == "auto":
        return []
    return [code.strip() for code in raw.split(",") if code.strip()]


def build_transcribe_body(
    file_uri: str, mime_type: str, language_codes: list[str]
) -> dict[str, Any]:
    return {
        "model": MODEL_SLUG,
        "input": [{"type": "audio", "uri": file_uri, "mime_type": mime_type}],
        "generation_config": {
            "transcription_config": {
                "language_codes": language_codes,
                "mode": {"type": "verbatim", "timestamp_granularities": ["word"]},
            }
        },
    }


def transcribe(
    file_uri: str, mime_type: str, language_codes: list[str], api_key: str
) -> dict[str, Any]:
    body = build_transcribe_body(file_uri, mime_type, language_codes)
    logger.info(
        "gemini_3_5_transcribe interactions call: model=%s language_codes=%s",
        MODEL_SLUG,
        language_codes,
    )
    r = request_with_retry(
        "POST",
        INTERACTIONS_ENDPOINT,
        headers={**auth_headers(api_key), "Content-Type": "application/json"},
        json=body,
        timeout=600,
    )
    result = r.json() or {}
    logger.info(
        "gemini_3_5_transcribe interactions ok: status=%s id=%s",
        result.get("status"),
        result.get("id"),
    )
    return result


def delete_file(file_name: str, api_key: str) -> None:
    """Best-effort delete of the uploaded file. Never fatal — a cleanup
    failure must not fail the whole eval run."""
    try:
        r = requests.delete(
            f"{FILES_API_ROOT}/{file_name}",
            headers=auth_headers(api_key),
            timeout=30,
        )
        logger.debug(
            "gemini_3_5_transcribe cleanup delete file: name=%s status=%d",
            file_name,
            r.status_code,
        )
    except requests.RequestException:
        logger.warning(
            "gemini_3_5_transcribe cleanup: failed to delete file name=%s (non-fatal)",
            file_name,
            exc_info=True,
        )


def parse_offset_to_ms(offset: str | None) -> int | None:
    """Parse a Gemini offset string like "5.200s" or "9s" into whole
    milliseconds. Returns None for a missing/malformed offset."""
    if not offset:
        return None
    s = offset.strip()
    if not s.endswith("s"):
        return None
    numeric = s[:-1]
    try:
        seconds = float(numeric)
    except ValueError:
        return None
    return round(seconds * 1000)


def collect_word_infos(response: dict[str, Any]) -> list[dict[str, Any]]:
    """Walk steps[*].content[*].annotations[*] in order and collect every
    `word_info` annotation. Order is preserved across steps and content
    blocks — the response's own ordering is the word order."""
    words: list[dict[str, Any]] = []
    for step in response.get("steps") or []:
        for content in step.get("content") or []:
            for annotation in content.get("annotations") or []:
                if annotation.get("type") != "word_info":
                    continue
                words.append(annotation)
    return words


def word_infos_to_words(word_infos: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Convert raw word_info annotations into eval-schema words:
    {text, start_ms, end_ms, confidence}. Gemini emits no per-word
    confidence, so confidence is always None. A word_info missing usable
    text/timing is skipped."""
    words: list[dict[str, Any]] = []
    for wi in word_infos:
        text = (wi.get("text") or "").strip()
        start_ms = parse_offset_to_ms(wi.get("start_offset"))
        end_ms = parse_offset_to_ms(wi.get("end_offset"))
        if not text or start_ms is None or end_ms is None:
            logger.warning("gemini_3_5_transcribe skipping malformed word_info: %r", wi)
            continue
        words.append(
            {
                "text": text,
                "start_ms": start_ms,
                "end_ms": end_ms,
                "confidence": None,
            }
        )
    return words


def group_words_into_lines(words: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Split a flat word stream into lyric lines on silence gaps >
    LINE_GAP_MS. Structurally identical to assemblyai_universal_3_pro.py /
    soniox_v5.py's grouping so results are directly comparable. A line that
    cannot be timed never occurs here (words with no timing are already
    dropped by word_infos_to_words), so start_ms/end_ms are only None when
    there are zero words overall — see estimate_duration_ms."""
    if not words:
        return []
    lines: list[dict[str, Any]] = []
    current_words: list[dict[str, Any]] = []
    prev_end: int | None = None
    for w in words:
        if (
            prev_end is not None
            and w["start_ms"] - prev_end > LINE_GAP_MS
            and current_words
        ):
            lines.append(_flush_line(current_words))
            current_words = []
        current_words.append(w)
        prev_end = w["end_ms"]
    if current_words:
        lines.append(_flush_line(current_words))
    return lines


def _flush_line(words: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "text": " ".join(w["text"] for w in words),
        "start_ms": words[0]["start_ms"],
        "end_ms": words[-1]["end_ms"],
        "text_sk": None,
        "words": words,
    }


def emit_result(
    *,
    out_path: Path,
    wav_path: str,
    duration_ms: int,
    lines: list[dict[str, Any]],
    raw_confidence: float | None,
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
    logging.basicConfig(
        level=os.environ.get("LYRICS_EVAL_LOG_LEVEL", "DEBUG"),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)

    logger.info(
        "gemini_3_5_transcribe starting: wav=%s out=%s model=%s",
        args.wav,
        args.out,
        MODEL_SLUG,
    )

    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        logger.error("GEMINI_API_KEY not set — aborting")
        print("GEMINI_API_KEY not set", flush=True)
        return 2

    language_codes = resolve_language_codes()

    t0 = time.time()
    upload_t0 = time.time()
    file_info = upload_audio(args.wav, api_key)
    file_name = file_info["name"]
    try:
        ready_info = poll_file_ready(file_name, api_key)
        upload_s = time.time() - upload_t0

        file_uri = ready_info.get("uri") or file_info.get("uri")
        mime_type = (
            ready_info.get("mimeType") or file_info.get("mimeType") or AUDIO_MIME_TYPE
        )
        if not file_uri:
            raise RuntimeError(
                f"gemini_3_5_transcribe file {file_name} has no uri: {ready_info}"
            )

        transcribe_t0 = time.time()
        response = transcribe(file_uri, mime_type, language_codes, api_key)
        transcribe_s = time.time() - transcribe_t0
    finally:
        delete_file(file_name, api_key)
    elapsed = time.time() - t0

    if response.get("status") != "completed":
        raise RuntimeError(
            f"gemini_3_5_transcribe interaction did not complete: "
            f"status={response.get('status')!r} id={response.get('id')!r}"
        )

    word_infos = collect_word_infos(response)
    words = word_infos_to_words(word_infos)
    lines = group_words_into_lines(words)

    logger.info(
        "gemini_3_5_transcribe done: words=%d lines=%d elapsed_s=%.1f",
        len(words),
        len(lines),
        elapsed,
    )

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=None,
        metadata={
            "model": MODEL_SLUG,
            "elapsed_s": round(elapsed, 1),
            "upload_s": round(upload_s, 1),
            "transcribe_s": round(transcribe_s, 1),
            "word_count": len(words),
            "line_count": len(lines),
            "line_gap_ms": LINE_GAP_MS,
            "language_codes": language_codes,
            "usage": response.get("usage"),
            "device": "api",
            "interaction_id": response.get("id"),
        },
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
