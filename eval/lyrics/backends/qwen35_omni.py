#!/usr/bin/env python3
"""qwen35_omni.py — Alibaba DashScope Qwen3.5-Omni-Plus, one-call spike.

North-star spike backend: ONE Qwen3.5-Omni-Plus call over the isolated-vocal
WAV that BOTH transcribes the sung lyrics (EN/ES) AND translates each line
to Slovak in a single structured-JSON response — see
`eval/lyrics/prompts/one_call_karaoke.md` for the shared prompt this and
`gemini36_flash.py` both load at runtime.

Qwen3.5-Omni-Plus is the only north-star candidate with published SINGING
benchmarks (its sibling ASR stack reports MIR-1K vocal-only WER 4.56%); it
is an instructable omni chat model (text+image+audio+video in), so the
whole job goes in one prompted call — same shape as the Gemini backend.

API SHAPE — verified EMPIRICALLY on 2026-08-05 against the live DashScope
API with the production `dashscope_api_key` (a workspace-scoped `sk-ws-`
key), because the written docs across alibabacloud.com/help/en/model-studio
and help.aliyun.com/zh/model-studio disagree with each other and, in one
place, with reality:

  * Endpoint: the CLASSIC international gateway
    `https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`
    (OpenAI-compatible). Several doc pages claim a workspace-scoped
    `sk-ws-` key requires the newer per-workspace host
    `https://{WorkspaceId}.<region>.maas.aliyuncs.com/compatible-mode/v1`
    with the WorkspaceId embedded in the hostname (obtained only from the
    console, which this harness has no access to) — empirically FALSE for
    this key: the classic `dashscope-intl.aliyuncs.com` host resolves the
    workspace from the Bearer token itself and returned HTTP 200 on every
    call. The `{WorkspaceId}.<region>.maas.aliyuncs.com` hosts are for a
    different deployment tier (dedicated/custom throughput), not required
    for shared-catalog access to qwen3.5-omni-plus.
  * Model id: `qwen3.5-omni-plus`, confirmed live (also live-verified:
    `qwen-omni-turbo`, `qwen3-omni-flash`; `dashscope-us.aliyuncs.com`
    returned `invalid_api_key` for this key — the account is int'l, not
    US-region).
  * Auth: `Authorization: Bearer <DASHSCOPE_API_KEY>` — no
    `X-DashScope-WorkSpace` header, no workspace query param. Docs search
    turned up no such header for the compatible-mode path; none was needed.
  * Audio input: OpenAI-compatible `input_audio` content part —
    `{"type": "input_audio", "input_audio": {"data": "data:;base64,<B64>",
    "format": "wav"}}` (note: empty MIME before the `;base64,` — matches
    the zh docs' literal example, and matches what actually worked).
  * Streaming: help.aliyun.com/zh states plainly "stream 必须设置为
    True，否则会报错" (stream must be set to True or it errors). Empirically
    this account/model ALSO accepted `stream: false` for both a text-only
    prompt and a real audio+text prompt (20 s clip) — but that was only
    verified on short completions. This backend uses `stream: true`
    (documented-required, and proven end-to-end against the real 14 MB /
    236 s fixture below) rather than trust the shorter, undocumented
    success path for the long structured-JSON completions this job
    actually produces.
  * `modalities: ["text"]` (never `["text","audio"]`) — requests text-only
    output so DashScope does not synthesize and bill for a spoken-audio
    reply; confirmed accepted.
  * Size cap: the zh docs claim the base64 string "must be < 10MB"
    (编码后的 Base64 字符串大小必须小于 10MB). Empirically FALSE at the low
    end — a 14.4 MB raw WAV (19.2 MB base64, the `5JW87KKDTcU` fixture,
    236 s) returned a real, correct transcription (`"Nothing excites us
    like Jesus..."`, matching the actual lyrics). But a real cap exists
    higher up: a 42.8 MB raw WAV (57.1 MB base64, `hSMJa5tImRU`) caused the
    TLS connection itself to drop (`SSLEOFError` on write) — the server
    aborts the connection outright on an oversized body rather than
    returning a clean 4xx. `INLINE_LIMIT_BYTES` below is therefore set
    well under the proven-working 14.4 MB point (see the constant for the
    exact value); any fixture over that limit is transcoded to a small
    mono MP3 via ffmpeg before base64-encoding, since DashScope's own docs
    list MP3 among the supported input formats (AMR/WAV/3GP/3GPP/AAC/MP3)
    and this project's fixtures run up to 40.9 MB raw.

Docs consulted (2026-08-05, English + Chinese editions per task brief):
  - https://www.alibabacloud.com/help/en/model-studio/qwen-omni
  - https://help.aliyun.com/zh/model-studio/qwen-omni
  - https://www.alibabacloud.com/help/en/model-studio/get-api-key
  - https://www.alibabacloud.com/help/en/model-studio/models
  - https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope
  - https://www.alibabacloud.com/help/en/model-studio/model-calling-in-sub-workspace

Usage:
    python eval/lyrics/backends/qwen35_omni.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads DASHSCOPE_API_KEY from the environment.
"""

from __future__ import annotations

import argparse
import base64
import json
import logging
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("lyrics_eval.qwen35_omni")

BACKEND_ID = "qwen35-omni"
BACKEND_REVISION = 1
MODEL_SLUG = "qwen3.5-omni-plus"

API_BASE = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
CHAT_ENDPOINT = f"{API_BASE}/chat/completions"

# Empirically: 14.4 MB raw (19.2 MB base64) worked end-to-end (real,
# correct transcript). 42.8 MB raw (57.1 MB base64) dropped the TLS
# connection outright. Set well under the proven-good point so every
# fixture that fits gets the lossless inline WAV path; anything over is
# transcoded to a small mono MP3 (see transcode_to_mp3()).
INLINE_LIMIT_BYTES = 14 * 1024 * 1024
TRANSCODE_BITRATE = "96k"

PROMPT_PATH = Path(__file__).resolve().parents[1] / "prompts" / "one_call_karaoke.md"
PROMPT_MARKER_RE = re.compile(
    r"<!-- PROMPT-START -->(.*?)<!-- PROMPT-END -->", re.DOTALL
)

JSON_FENCE_RE = re.compile(r"```(?:json)?\s*(.*?)```", re.DOTALL)
JSON_OBJECT_RE = re.compile(r"\{.*\}", re.DOTALL)


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


def resolve_ffmpeg() -> str:
    """Resolve the ffmpeg binary, matching audio_prep.py's convention:
    SONGPLAYER_TOOLS_DIR env var (default the production tools dir on
    Windows), falling back to PATH."""
    tools_dir = os.environ.get(
        "SONGPLAYER_TOOLS_DIR", r"C:\ProgramData\SongPlayer\cache\tools"
    )
    candidate = Path(tools_dir) / (
        "ffmpeg.exe" if sys.platform == "win32" else "ffmpeg"
    )
    if candidate.exists():
        return str(candidate)
    found = shutil.which("ffmpeg")
    if found:
        return found
    raise RuntimeError(
        f"ffmpeg not found at {candidate} or on PATH — cannot transcode oversized audio"
    )


def transcode_to_mp3(wav_path: Path) -> Path:
    """Transcode an oversized WAV to a small mono MP3 via ffmpeg so it fits
    under INLINE_LIMIT_BYTES. Returns the path to a temp .mp3 file (caller
    is responsible for cleanup)."""
    ffmpeg = resolve_ffmpeg()
    tmp_fd, tmp_path_str = tempfile.mkstemp(suffix=".mp3", prefix="qwen35omni_")
    os.close(tmp_fd)
    tmp_path = Path(tmp_path_str)
    cmd = [
        ffmpeg,
        "-y",
        "-i",
        str(wav_path),
        "-ac",
        "1",
        "-b:a",
        TRANSCODE_BITRATE,
        "-f",
        "mp3",
        str(tmp_path),
    ]
    logger.info("transcoding oversized audio to mp3: %s -> %s", wav_path, tmp_path)
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if proc.returncode != 0:
        raise RuntimeError(
            f"ffmpeg transcode failed (rc={proc.returncode}): {proc.stderr[-800:]}"
        )
    out_size = tmp_path.stat().st_size
    logger.info(
        "transcode complete: %s bytes=%d (from %s bytes=%d)",
        tmp_path,
        out_size,
        wav_path,
        wav_path.stat().st_size,
    )
    return tmp_path


def build_audio_part(
    wav_path: Path,
) -> tuple[dict[str, Any], dict[str, Any], Path | None]:
    """Build the `input_audio` content part, transcoding to mp3 first if the
    raw WAV is over INLINE_LIMIT_BYTES.

    Returns (part, upload_metadata, temp_file_to_cleanup_or_None).
    """
    raw_size = wav_path.stat().st_size
    audio_format = "wav"
    send_path = wav_path
    temp_file: Path | None = None
    if raw_size > INLINE_LIMIT_BYTES:
        logger.info(
            "qwen audio transport=transcoded_mp3: raw_bytes=%d (limit=%d)",
            raw_size,
            INLINE_LIMIT_BYTES,
        )
        temp_file = transcode_to_mp3(wav_path)
        send_path = temp_file
        audio_format = "mp3"
    else:
        logger.info(
            "qwen audio transport=inline_wav: raw_bytes=%d (limit=%d)",
            raw_size,
            INLINE_LIMIT_BYTES,
        )

    send_bytes = send_path.read_bytes()
    b64 = base64.standard_b64encode(send_bytes).decode("ascii")
    part = {
        "type": "input_audio",
        "input_audio": {"data": f"data:;base64,{b64}", "format": audio_format},
    }
    metadata = {
        "audio_transport": "inline_wav" if temp_file is None else "transcoded_mp3",
        "audio_raw_bytes": raw_size,
        "audio_sent_bytes": len(send_bytes),
        "audio_sent_b64_chars": len(b64),
        "audio_format_sent": audio_format,
    }
    return part, metadata, temp_file


# Backend-local schema nudge (per the /lyrics-eval "free hands on prompts,
# not schemas" rule — the SHARED prompt file is never edited here). Smoke
# test on 2026-08-05 with only the shared prompt's own schema instructions
# showed qwen3.5-omni-plus ignores the requested `{"lines": [...]}` shape
# and instead free-associates its own metadata schema — verbatim observed
# reply: `{"artist": "Planetshakers", "song_title": "Like a Fire", "album":
# ..., "lyrics": [{"start_time": "0:05.180", "end_time": "0:07.410", "text":
# ...}]}` (song metadata + MM:SS.mmm time STRINGS, no text_sk, no top-level
# "lines" key at all). This nudge restates the exact required shape with a
# concrete example and explicitly bans the observed wrong shape.
SCHEMA_NUDGE = (
    "Process the song audio above now. Follow the system instructions "
    "exactly for the transcription, phrasing, and Slovak translation rules. "
    'Output EXACTLY ONE JSON object with a SINGLE top-level key: "lines" '
    '(an array). Do NOT include any other top-level keys — no "artist", '
    '"song_title", "album", "genre", "duration_seconds", or any '
    'song-metadata fields. Each element of "lines" must have exactly these '
    'keys: "start_ms" (a plain INTEGER number of milliseconds — NEVER a '
    'time string like "0:05.180"), "end_ms" (same, integer milliseconds), '
    '"text" (original-language transcript), "text_sk" (Slovak translation), '
    'and optionally "words". Example of the required shape: '
    '{"lines": [{"start_ms": 5180, "end_ms": 7410, "text": "Nothing excites '
    'us like Jesus", "text_sk": "Nič nás nenadchne tak ako Ježiš"}]}. '
    "Output ONLY this JSON object — no markdown fences, no extra top-level "
    "keys, no commentary before or after it."
)


def build_request(audio_part: dict[str, Any]) -> dict[str, Any]:
    return {
        "model": MODEL_SLUG,
        "messages": [
            {"role": "system", "content": load_prompt()},
            {
                "role": "user",
                "content": [
                    audio_part,
                    {"type": "text", "text": SCHEMA_NUDGE},
                ],
            },
        ],
        # Text-only output — never pay for/receive synthesized speech back.
        "modalities": ["text"],
        # Documented as required for qwen-omni models; also the path
        # proven end-to-end against the real 14MB/236s fixture (see module
        # docstring). Response is SSE ("data: {...}\n\n" chunks).
        "stream": True,
    }


def call_qwen(
    body: dict[str, Any], api_key: str
) -> tuple[str, str | None, dict[str, Any] | None]:
    """POST the chat/completions request and accumulate the SSE stream.

    Returns (accumulated_text, finish_reason, usage). Raises RuntimeError
    with the real HTTP status + body on any non-200 response — never
    silently proceeds with a partial/empty accumulation.
    """
    logger.info(
        "qwen chat/completions request: model=%s endpoint=%s", MODEL_SLUG, CHAT_ENDPOINT
    )
    t0 = time.time()
    r = requests.post(
        CHAT_ENDPOINT,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        json=body,
        # Full-song single-call transcribe+translate can run long; generous
        # timeout matches the other backends' long-poll budgets.
        timeout=900,
        stream=True,
    )
    if r.status_code != 200:
        body_text = r.text[:800]
        logger.error(
            "qwen chat/completions failed: status=%d body=%s", r.status_code, body_text
        )
        raise RuntimeError(f"qwen chat/completions {r.status_code}: {body_text}")

    accumulated = ""
    finish_reason: str | None = None
    usage: dict[str, Any] | None = None
    chunk_count = 0
    for raw_line in r.iter_lines(decode_unicode=True):
        if not raw_line:
            continue
        if not raw_line.startswith("data:"):
            continue
        payload_str = raw_line[len("data:") :].strip()
        if payload_str == "[DONE]":
            break
        try:
            payload = json.loads(payload_str)
        except json.JSONDecodeError:
            logger.warning(
                "qwen stream chunk was not valid JSON: %r", payload_str[:200]
            )
            continue
        chunk_count += 1
        choices = payload.get("choices") or []
        if choices:
            choice = choices[0]
            delta = choice.get("delta") or {}
            content = delta.get("content")
            if content:
                accumulated += content
            fr = choice.get("finish_reason")
            if fr:
                finish_reason = fr
        if payload.get("usage"):
            usage = payload["usage"]

    latency_s = time.time() - t0
    logger.info(
        "qwen chat/completions ok: chunks=%d latency_s=%.1f accumulated_chars=%d "
        "finish_reason=%s",
        chunk_count,
        latency_s,
        len(accumulated),
        finish_reason,
    )
    return accumulated, finish_reason, usage


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
        f"qwen output was not valid JSON after fence/object extraction: "
        f"{trimmed[:400]!r}"
    )


def payload_to_lines(payload: dict[str, Any]) -> list[dict[str, Any]]:
    """Convert the parsed `{lines: [...]}` payload into the eval line shape.

    Each emitted line: {text, start_ms, end_ms, text_sk, words}.
    """
    raw_lines = payload.get("lines")
    if not isinstance(raw_lines, list):
        raise RuntimeError(
            f"qwen JSON payload missing 'lines' array: {json.dumps(payload)[:400]}"
        )
    lines: list[dict[str, Any]] = []
    for raw in raw_lines:
        text = (raw.get("text") or "").strip()
        if not text:
            continue
        if "start_ms" not in raw or "end_ms" not in raw:
            raise RuntimeError(f"qwen line missing start_ms/end_ms: {raw!r}")
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
        "qwen payload parsed: raw_lines=%d kept_lines=%d missing_text_sk=%d "
        "lines_with_words=%d",
        len(raw_lines),
        len(lines),
        lines_without_sk,
        lines_with_words,
    )
    if lines_without_sk:
        logger.warning(
            "qwen output missing text_sk on %d/%d lines — translation coverage gap",
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
        "qwen35_omni starting: wav=%s out=%s model=%s", args.wav, args.out, MODEL_SLUG
    )

    api_key = os.environ.get("DASHSCOPE_API_KEY")
    if not api_key:
        logger.error("DASHSCOPE_API_KEY not set — aborting")
        print("DASHSCOPE_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    audio_part, upload_meta, temp_file = build_audio_part(args.wav)
    try:
        body = build_request(audio_part)
        text, finish_reason, usage = call_qwen(body, api_key)
    finally:
        if temp_file is not None:
            try:
                temp_file.unlink(missing_ok=True)
            except OSError:
                logger.warning(
                    "failed to clean up temp file %s", temp_file, exc_info=True
                )
    elapsed = time.time() - t0
    logger.info("qwen35_omni call complete: elapsed_s=%.1f", elapsed)

    if not text.strip():
        logger.error(
            "qwen response had no accumulated text: finish_reason=%s usage=%s",
            finish_reason,
            usage,
        )
        raise RuntimeError(
            f"qwen returned no text content (finish_reason={finish_reason!r})"
        )

    # finish_reason == "length" means the completion was truncated by the
    # model's max-output-tokens cap — never silently accept a partial JSON
    # line list as if it were the whole song.
    if finish_reason == "length":
        logger.error(
            "qwen output TRUNCATED (finish_reason=length): accumulated_chars=%d",
            len(text),
        )
        raise RuntimeError(
            "qwen output was truncated mid-generation (finish_reason=length) — "
            f"accumulated {len(text)} chars, JSON is likely incomplete"
        )

    logger.debug("qwen accumulated text length=%d chars", len(text))
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
            "usage": usage,
            **upload_meta,
        },
    )
    logger.info(
        "qwen35_omni done: lines=%d out=%s elapsed_s=%.1f",
        len(lines),
        args.out,
        elapsed,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
