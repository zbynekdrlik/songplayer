#!/usr/bin/env python3
"""gemini36_flash.py — Google Gemini 3.6 Flash, direct API, one-call spike.

North-star spike backend: ONE Gemini call over the isolated-vocal WAV that
BOTH transcribes the sung lyrics (EN/ES) AND translates each line to Slovak
in a single structured-JSON response — see
`eval/lyrics/prompts/one_call_karaoke.md` for the shared prompt this and
`gemini31_pro.py` both load at runtime.

Unlike `gemini_3_1_flash_lite.py` (routed through OpenRouter), this backend
calls `generativelanguage.googleapis.com` directly, mirroring the endpoint
shape and `x-goog-api-key` auth header used in production
(`crates/sp-server/src/metadata/gemini.rs::GeminiProvider::endpoint`).

Audio transport: inline base64 for WAVs <= 20 MB (INLINE_LIMIT_BYTES);
larger files go through the Gemini Files API resumable-upload protocol and
are referenced by URI once the uploaded file reaches state ACTIVE.

Structured output: `generationConfig.responseMimeType=application/json` +
`responseSchema` (RESPONSE_SCHEMA below) is the first line of defense
against malformed JSON; `parse_json_response()` still applies robust
extraction (direct parse -> markdown-fence strip -> first-JSON-object regex)
as a second line of defense, since even schema-constrained models
occasionally wrap output in commentary in practice.

`finishReason == "RECITATION"` (Gemini's copyright-block signal — a real,
previously-observed risk per CLAUDE.md's translator-refusal note) is
detected explicitly and raised as a clear, visible error rather than being
allowed to silently degrade into an empty/malformed result.

Usage:
    python eval/lyrics/backends/gemini36_flash.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads GEMINI_API_KEY from the environment.
"""

from __future__ import annotations

import argparse
import base64
import json
import logging
import os
import re
import time
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("lyrics_eval.gemini36_flash")

BACKEND_ID = "gemini36-flash"
BACKEND_REVISION = 1
MODEL_SLUG = "gemini-3.6-flash"

API_ROOT = "https://generativelanguage.googleapis.com"
GENERATE_ENDPOINT = f"{API_ROOT}/v1beta/models/{MODEL_SLUG}:generateContent"
FILES_UPLOAD_ENDPOINT = f"{API_ROOT}/upload/v1beta/files"
FILES_API_ROOT = f"{API_ROOT}/v1beta"

# Gemini's documented inline-audio limit; larger files must go through the
# Files API instead of being embedded as base64 in the request body.
INLINE_LIMIT_BYTES = 20 * 1024 * 1024

AUDIO_MIME_TYPE = "audio/wav"

PROMPT_PATH = Path(__file__).resolve().parents[1] / "prompts" / "one_call_karaoke.md"
PROMPT_MARKER_RE = re.compile(
    r"<!-- PROMPT-START -->(.*?)<!-- PROMPT-END -->", re.DOTALL
)

JSON_FENCE_RE = re.compile(r"```(?:json)?\s*(.*?)```", re.DOTALL)
JSON_OBJECT_RE = re.compile(r"\{.*\}", re.DOTALL)

# Structured-output schema for generationConfig.responseSchema. Mirrors the
# JSON shape documented in the shared prompt.
RESPONSE_SCHEMA: dict[str, Any] = {
    "type": "OBJECT",
    "properties": {
        "lines": {
            "type": "ARRAY",
            "items": {
                "type": "OBJECT",
                "properties": {
                    "start_ms": {"type": "INTEGER"},
                    "end_ms": {"type": "INTEGER"},
                    "text": {"type": "STRING"},
                    "text_sk": {"type": "STRING"},
                    "words": {
                        "type": "ARRAY",
                        "items": {
                            "type": "OBJECT",
                            "properties": {
                                "start_ms": {"type": "INTEGER"},
                                "end_ms": {"type": "INTEGER"},
                                "w": {"type": "STRING"},
                            },
                            "required": ["start_ms", "end_ms", "w"],
                        },
                    },
                },
                "required": ["start_ms", "end_ms", "text", "text_sk"],
            },
        }
    },
    "required": ["lines"],
}


def load_prompt() -> str:
    """Load the shared one-call prompt body from prompts/one_call_karaoke.md.

    Only the text between the `<!-- PROMPT-START -->` / `<!-- PROMPT-END -->`
    markers is returned — the surrounding doc commentary in the markdown
    file is never sent to the API.
    """
    raw = PROMPT_PATH.read_text(encoding="utf-8")
    m = PROMPT_MARKER_RE.search(raw)
    if not m:
        raise RuntimeError(
            f"{PROMPT_PATH} is missing <!-- PROMPT-START -->/<!-- PROMPT-END --> markers"
        )
    prompt = m.group(1).strip()
    logger.debug("loaded prompt from %s (%d chars)", PROMPT_PATH, len(prompt))
    return prompt


def poll_file_active(
    file_name: str, api_key: str, timeout_s: float = 120.0
) -> dict[str, Any]:
    """Poll a Gemini Files API resource until state == ACTIVE.

    A just-uploaded file starts in PROCESSING and cannot yet be referenced
    by generateContent. Raises on FAILED state or on poll timeout — never
    silently proceeds with a non-ACTIVE file.
    """
    deadline = time.time() + timeout_s
    url = f"{FILES_API_ROOT}/{file_name}"
    poll_count = 0
    while True:
        poll_count += 1
        r = requests.get(url, headers={"x-goog-api-key": api_key}, timeout=30)
        if r.status_code != 200:
            logger.error(
                "gemini files get failed: file=%s status=%d body=%s",
                file_name,
                r.status_code,
                r.text[:400],
            )
            raise RuntimeError(f"gemini files get {r.status_code}: {r.text[:400]}")
        info = r.json() or {}
        state = info.get("state")
        logger.debug(
            "gemini files poll #%d: file=%s state=%s", poll_count, file_name, state
        )
        if state == "ACTIVE":
            logger.info(
                "gemini file %s reached ACTIVE after %d poll(s)", file_name, poll_count
            )
            return info
        if state == "FAILED":
            logger.error(
                "gemini file %s processing FAILED: %s", file_name, info.get("error")
            )
            raise RuntimeError(f"gemini file processing failed: {info.get('error')!r}")
        if time.time() > deadline:
            logger.error(
                "gemini file %s poll timed out after %.1fs (last state=%s)",
                file_name,
                timeout_s,
                state,
            )
            raise RuntimeError(
                f"gemini file {file_name} did not reach ACTIVE within {timeout_s}s "
                f"(last state={state!r})"
            )
        time.sleep(2.0)


def upload_via_files_api(wav_path: Path, api_key: str) -> tuple[str, str]:
    """Upload audio >20MB via the Gemini Files API resumable protocol.

    Returns (file_uri, mime_type) once the uploaded file is ACTIVE.
    """
    size = wav_path.stat().st_size
    logger.info(
        "gemini files upload starting: path=%s bytes=%d (> inline limit %d)",
        wav_path,
        size,
        INLINE_LIMIT_BYTES,
    )
    start_resp = requests.post(
        FILES_UPLOAD_ENDPOINT,
        headers={
            "x-goog-api-key": api_key,
            "X-Goog-Upload-Protocol": "resumable",
            "X-Goog-Upload-Command": "start",
            "X-Goog-Upload-Header-Content-Length": str(size),
            "X-Goog-Upload-Header-Content-Type": AUDIO_MIME_TYPE,
            "Content-Type": "application/json",
        },
        json={"file": {"display_name": wav_path.name}},
        timeout=60,
    )
    if start_resp.status_code != 200:
        raise RuntimeError(
            f"gemini files upload-start {start_resp.status_code}: {start_resp.text[:400]}"
        )
    upload_url = start_resp.headers.get("X-Goog-Upload-URL")
    if not upload_url:
        raise RuntimeError(
            "gemini files upload-start response missing X-Goog-Upload-URL header"
        )
    logger.debug("gemini files upload-start ok, upload_url obtained")

    with wav_path.open("rb") as fh:
        upload_resp = requests.post(
            upload_url,
            headers={
                "X-Goog-Upload-Offset": "0",
                "X-Goog-Upload-Command": "upload, finalize",
                "Content-Length": str(size),
            },
            data=fh,
            timeout=300,
        )
    if upload_resp.status_code != 200:
        raise RuntimeError(
            f"gemini files upload-bytes {upload_resp.status_code}: "
            f"{upload_resp.text[:400]}"
        )
    file_info = (upload_resp.json() or {}).get("file") or {}
    file_uri = file_info.get("uri")
    file_name = file_info.get("name")
    if not file_uri or not file_name:
        raise RuntimeError(
            f"gemini files upload response missing file.uri/file.name: "
            f"{upload_resp.text[:400]}"
        )
    logger.info(
        "gemini files upload-bytes ok: file_name=%s file_uri=%s", file_name, file_uri
    )

    active_info = poll_file_active(file_name, api_key)
    return (
        active_info.get("uri", file_uri),
        active_info.get("mimeType", AUDIO_MIME_TYPE),
    )


def build_audio_part(
    wav_path: Path, api_key: str
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Build the `contents[].parts[]` audio entry, choosing inline vs Files API.

    Returns (part, upload_metadata) — upload_metadata is folded into the
    result's `metadata` block so the transport choice is visible in output.
    """
    size = wav_path.stat().st_size
    if size <= INLINE_LIMIT_BYTES:
        logger.info(
            "gemini audio transport=inline: path=%s bytes=%d (limit=%d)",
            wav_path,
            size,
            INLINE_LIMIT_BYTES,
        )
        b64 = base64.standard_b64encode(wav_path.read_bytes()).decode("ascii")
        part = {"inline_data": {"mime_type": AUDIO_MIME_TYPE, "data": b64}}
        return part, {"audio_transport": "inline", "audio_bytes": size}
    logger.info(
        "gemini audio transport=files_api: path=%s bytes=%d (limit=%d)",
        wav_path,
        size,
        INLINE_LIMIT_BYTES,
    )
    file_uri, mime_type = upload_via_files_api(wav_path, api_key)
    part = {"file_data": {"mime_type": mime_type, "file_uri": file_uri}}
    return part, {"audio_transport": "files_api", "audio_bytes": size}


def build_request(audio_part: dict[str, Any]) -> dict[str, Any]:
    return {
        "system_instruction": {"parts": [{"text": load_prompt()}]},
        "contents": [
            {
                "role": "user",
                "parts": [
                    audio_part,
                    {
                        "text": "Process the song audio above now. Follow the "
                        "system instructions exactly and output only the JSON "
                        "object described there."
                    },
                ],
            }
        ],
        "generationConfig": {
            "temperature": 0.2,
            "candidateCount": 1,
            "responseMimeType": "application/json",
            "responseSchema": RESPONSE_SCHEMA,
            # Cap reasoning budget. Per CLAUDE.md's v11 pipeline note, an
            # unranged thinking budget on dense-chorus audio previously
            # caused hallucinated-duplicate loops and timeouts on this
            # project's production Gemini alignment provider. 4096 leaves
            # headroom for a full-song single-call transcribe+translate
            # without runaway thinking tokens.
            "thinkingConfig": {"thinkingBudget": 4096},
        },
    }


def call_gemini(body: dict[str, Any], api_key: str) -> dict[str, Any]:
    logger.info(
        "gemini generateContent request: model=%s endpoint=%s",
        MODEL_SLUG,
        GENERATE_ENDPOINT,
    )
    t0 = time.time()
    r = requests.post(
        GENERATE_ENDPOINT,
        headers={"x-goog-api-key": api_key, "Content-Type": "application/json"},
        json=body,
        # Full-song single-call transcribe+translate can run long; generous
        # timeout matches the other backends' long-poll budgets.
        timeout=900,
    )
    latency_s = time.time() - t0
    if r.status_code != 200:
        logger.error(
            "gemini generateContent failed: status=%d latency_s=%.1f body=%s",
            r.status_code,
            latency_s,
            r.text[:400],
        )
        raise RuntimeError(f"gemini generateContent {r.status_code}: {r.text[:400]}")
    logger.info(
        "gemini generateContent ok: status=%d latency_s=%.1f", r.status_code, latency_s
    )
    return r.json()


def _extract_candidate_text(candidate: dict[str, Any]) -> str:
    parts = (candidate.get("content") or {}).get("parts") or []
    texts = [p.get("text") for p in parts if p.get("text")]
    if not texts:
        raise RuntimeError(
            f"gemini candidate has no text parts: {json.dumps(candidate)[:400]}"
        )
    return "".join(texts)


def parse_json_response(text: str) -> dict[str, Any]:
    """Parse the model's reply into a dict, tolerating fences/preamble.

    Tries, in order: (1) the raw trimmed text, (2) the contents of a
    markdown ```json fence if present, (3) the first `{...}` object found
    by regex. Raises with the raw text (truncated) if none parse — never
    returns a guessed/empty shape.
    """
    trimmed = text.strip()
    attempts = [trimmed]
    fence = JSON_FENCE_RE.search(trimmed)
    if fence:
        attempts.append(fence.group(1).strip())
    obj = JSON_OBJECT_RE.search(trimmed)
    if obj:
        attempts.append(obj.group(0))
    for candidate in attempts:
        try:
            return json.loads(candidate)
        except json.JSONDecodeError:
            continue
    raise RuntimeError(
        f"gemini output was not valid JSON after fence/object extraction: "
        f"{trimmed[:400]!r}"
    )


def payload_to_lines(payload: dict[str, Any]) -> list[dict[str, Any]]:
    """Convert the parsed `{lines: [...]}` payload into the eval line shape.

    Each emitted line: {text, start_ms, end_ms, text_sk, words}. `text_sk`
    is an extension of the documented eval backend-output shape
    (`{backend_id, backend_revision, wav_path, duration_ms, lines,
    raw_confidence, metadata}` per eval/lyrics/README.md) carrying this
    spike's Slovak translation; the harness's report/judgment schemas do
    not (yet) score translation quality, but the field is additive and does
    not break existing consumers of `lines[].text/start_ms/end_ms/words`.
    """
    raw_lines = payload.get("lines")
    if not isinstance(raw_lines, list):
        raise RuntimeError(
            f"gemini JSON payload missing 'lines' array: {json.dumps(payload)[:400]}"
        )
    lines: list[dict[str, Any]] = []
    for raw in raw_lines:
        text = (raw.get("text") or "").strip()
        if not text:
            continue
        if "start_ms" not in raw or "end_ms" not in raw:
            raise RuntimeError(f"gemini line missing start_ms/end_ms: {raw!r}")
        start_ms = int(raw["start_ms"])
        end_ms = int(raw["end_ms"])
        text_sk = (raw.get("text_sk") or "").strip() or None

        words: list[dict[str, Any]] | None = None
        raw_words = raw.get("words") or []
        if raw_words:
            words = []
            for w in raw_words:
                ws = w.get("start_ms")
                we = w.get("end_ms")
                wt = (w.get("w") or "").strip()
                if ws is None or we is None or not wt:
                    continue
                words.append(
                    {
                        "text": wt,
                        "start_ms": int(ws),
                        "end_ms": int(we),
                        "confidence": 0.9,
                    }
                )
            if not words:
                words = None

        lines.append(
            {
                "text": text,
                "start_ms": start_ms,
                "end_ms": end_ms,
                "text_sk": text_sk,
                "words": words,
            }
        )
    lines_without_sk = sum(1 for line in lines if line["text_sk"] is None)
    lines_with_words = sum(1 for line in lines if line["words"] is not None)
    logger.info(
        "gemini payload parsed: raw_lines=%d kept_lines=%d missing_text_sk=%d "
        "lines_with_words=%d",
        len(raw_lines),
        len(lines),
        lines_without_sk,
        lines_with_words,
    )
    if lines_without_sk:
        logger.warning(
            "gemini output missing text_sk on %d/%d lines — translation coverage gap",
            lines_without_sk,
            len(lines),
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
    logging.basicConfig(
        level=os.environ.get("LYRICS_EVAL_LOG_LEVEL", "DEBUG"),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)

    logger.info(
        "gemini36_flash starting: wav=%s out=%s model=%s",
        args.wav,
        args.out,
        MODEL_SLUG,
    )

    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        logger.error("GEMINI_API_KEY not set — aborting")
        print("GEMINI_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    audio_part, upload_meta = build_audio_part(args.wav, api_key)
    body = build_request(audio_part)
    resp = call_gemini(body, api_key)
    elapsed = time.time() - t0
    logger.info("gemini36_flash call complete: elapsed_s=%.1f", elapsed)

    candidates = resp.get("candidates") or []
    if not candidates:
        logger.error("gemini response has no candidates: %s", json.dumps(resp)[:400])
        raise RuntimeError(
            f"gemini response has no candidates: {json.dumps(resp)[:400]}"
        )
    candidate = candidates[0]

    # Known copyright-block risk (CLAUDE.md's Claude-refusal mitigation note
    # documents the same class of risk for the translator). Must be visible
    # in the failure, never silently swallowed into an empty result.
    finish_reason = candidate.get("finishReason")
    logger.debug("gemini candidate finishReason=%s", finish_reason)
    if finish_reason == "RECITATION":
        logger.error(
            "gemini RECITATION block detected: model=%s wav=%s", MODEL_SLUG, args.wav
        )
        raise RuntimeError(
            "gemini blocked output: finishReason=RECITATION (copyright-block) — "
            f"the model refused to transcribe/translate this audio; model={MODEL_SLUG}"
        )

    text = _extract_candidate_text(candidate)
    logger.debug("gemini candidate text length=%d chars", len(text))
    payload = parse_json_response(text)
    lines = payload_to_lines(payload)

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=0.9,
        metadata={
            "model": MODEL_SLUG,
            "elapsed_s": round(elapsed, 1),
            "line_count": len(lines),
            "finish_reason": finish_reason,
            "usage": resp.get("usageMetadata"),
            **upload_meta,
        },
    )
    logger.info(
        "gemini36_flash done: lines=%d out=%s elapsed_s=%.1f",
        len(lines),
        args.out,
        elapsed,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
