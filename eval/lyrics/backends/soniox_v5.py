#!/usr/bin/env python3
"""soniox_v5.py — Soniox stt-async-v5, one-call transcript + word timing + SK translation.

North-star spike backend: a single Soniox async transcription job that
requests same-call one-way translation to Slovak
(`translation: {type: "one_way", target_language: "sk"}`) alongside the
transcript. Unlike the Gemini one-call backends (an audio-LLM emitting
structured JSON) this is a dedicated ASR model — word-level timestamps come
from Soniox's own acoustic alignment (`start_ms`/`end_ms` per TOKEN, not
LLM-emitted `[mm:ss.mmm]` strings), same family of guarantee as
`assemblyai_universal_3_pro.py`.

Verified against the live Soniox docs + the official REST example
(`soniox/soniox_examples` GitHub repo, `speech_to_text/python/soniox_async.py`)
2026-08-05:

Four-step REST API, base `https://api.soniox.com`:
  1. POST /v1/files                                (multipart; solo when using a local file)
  2. POST /v1/transcriptions                        ({model, audio source, translation, ...})
  3. GET  /v1/transcriptions/{id}                    poll until status == "completed" | "error"
  4. GET  /v1/transcriptions/{id}/transcript          -> {"tokens": [...]}
  (+ DELETE /v1/transcriptions/{id}, DELETE /v1/files/{id} — best-effort cleanup)
Auth: `Authorization: Bearer <SONIOX_API_KEY>` (Bearer prefix REQUIRED — unlike
AssemblyAI's raw-token convention).

Tokens are a flat, ordered stream and can be SUB-WORD pieces (docs' own
example: "Beautiful" -> "Beau"/"ti"/"ful", contiguous start_ms/end_ms, no
dedicated boundary flag). Soniox's own token stream also contains literal
whitespace as token text (confirmed via the reference SDK's `render_tokens`
helper walking a flat token list and via the docs' "subwords, words, or
spaces" phrasing) — `merge_tokens_to_words()` below treats a token with a
LEADING SPACE (or a token that is pure whitespace) as a word-boundary
signal and falls back to "empty current buffer" as the other boundary
signal, so it does not depend on Soniox using only one of those two
conventions.

**Translation alignment — verified, NOT assumed:** per
`docs/translation/stt-translation` + `docs/translation/stt-translation/async-translation`,
translated tokens carry NO `start_ms`/`end_ms` ("Translated tokens do not
include timestamps. They are generated after their spoken tokens and follow
the same sequence.") and translation happens at SEGMENT granularity, not
per-utterance and not necessarily per our silage-gap-derived line
("Transcription and translation chunks follow each other, but tokens are
not 1-to-1 mapped and may not align."). This backend therefore:
  1. Splits the flat token stream into alternating RUNS by
     `translation_status` (`"translation"` vs everything else = source).
  2. Line-groups each SOURCE run independently via the SAME silence-gap
     heuristic as `assemblyai_universal_3_pro.py` (LINE_GAP_MS=400,
     matching this project's established wall-tolerance convention rather
     than inventing a new threshold).
  3. Only assigns a TRANSLATION run's joined text to `text_sk` when the
     immediately preceding SOURCE run produced EXACTLY ONE line — an
     unambiguous 1:1 mapping. When a source run spans multiple lines, the
     segment translation cannot be honestly split across them: `text_sk`
     stays `None` on every line in that run, and the full segment text is
     preserved in `metadata["unaligned_translation_segments"]` instead of
     being silently dropped or fabricated onto one arbitrary line.
This is a REAL fidelity question the smoke test settles empirically: if
Soniox chunks translation at roughly line/phrase granularity, most source
runs will be single-line and text_sk coverage will be high; if Soniox
translates in a few large segments (or one segment per language switch —
i.e. close to whole-transcript), coverage will be low and that is reported
honestly, not papered over.

Usage:
    python eval/lyrics/backends/soniox_v5.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads SONIOX_API_KEY from the environment.
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

logger = logging.getLogger("lyrics_eval.soniox_v5")

BACKEND_ID = "soniox-v5"
BACKEND_REVISION = 1
API_BASE = "https://api.soniox.com"
MODEL_SLUG = "stt-async-v5"
TARGET_LANGUAGE = "sk"

# Silence-gap threshold for "new line" — matches assemblyai_universal_3_pro.py
# r2 (LINE_GAP_MS=400) rather than inventing a new threshold, per this
# project's established convention (lrclib line cadence / wall tolerance).
LINE_GAP_MS = 400

# Poll cadence + overall timeout — matches the other async backends in this
# harness (aai_u35_translate.py, assemblyai_universal_3_pro.py).
POLL_INTERVAL_S = 2.0
POLL_TIMEOUT_S = 1800  # 30 min


def auth_headers(token: str) -> dict[str, str]:
    """Soniox uses a Bearer-prefixed token (unlike AssemblyAI's raw token)."""
    return {"Authorization": f"Bearer {token}"}


def upload_audio(audio_path: Path, token: str) -> str:
    """POST /v1/files (multipart) and return the file id."""
    size = audio_path.stat().st_size
    logger.info("soniox upload starting: path=%s bytes=%d", audio_path, size)
    with audio_path.open("rb") as fh:
        r = requests.post(
            f"{API_BASE}/v1/files",
            headers=auth_headers(token),
            files={"file": (audio_path.name, fh, "audio/wav")},
            timeout=300,
        )
    if r.status_code != 200:
        logger.error(
            "soniox upload failed: status=%d body=%s", r.status_code, r.text[:300]
        )
    r.raise_for_status()
    file_id = (r.json() or {}).get("id")
    if not file_id:
        raise RuntimeError(f"soniox upload response missing id: {r.text[:300]}")
    logger.info("soniox upload ok: file_id=%s", file_id)
    return file_id


def create_transcription(file_id: str, token: str) -> str:
    body: dict[str, Any] = {
        "model": MODEL_SLUG,
        "file_id": file_id,
        # Language hints significantly improve accuracy; songs in this
        # catalog are EN or ES worship music (some multi_language fixtures
        # code-switch within one song).
        "language_hints": ["en", "es"],
        "enable_language_identification": True,
        # No diarization: isolated solo-vocal stems, not a conversation —
        # matches aai_u35_translate.py's "solo singer" convention. Not
        # needed for translation alignment either (that's driven purely by
        # translation_status, not speaker).
        "enable_speaker_diarization": False,
        # Same-call one-way translation into Slovak. `context` (vocabulary
        # biasing) is deliberately OMITTED — this eval tests the clean
        # one-call path per the task brief, not a tuned/biased variant.
        "translation": {
            "type": "one_way",
            "target_language": TARGET_LANGUAGE,
        },
    }
    logger.info(
        "soniox create_transcription: model=%s target_language=%s",
        MODEL_SLUG,
        TARGET_LANGUAGE,
    )
    r = requests.post(
        f"{API_BASE}/v1/transcriptions",
        headers={**auth_headers(token), "Content-Type": "application/json"},
        json=body,
        timeout=60,
    )
    if r.status_code != 200:
        logger.error(
            "soniox create_transcription failed: status=%d body=%s",
            r.status_code,
            r.text[:300],
        )
    r.raise_for_status()
    payload = r.json() or {}
    tid = payload.get("id")
    if not tid:
        raise RuntimeError(f"soniox transcription create missing id: {r.text[:300]}")
    logger.info("soniox transcription created: id=%s", tid)
    return tid


def poll_transcription(transcription_id: str, token: str) -> None:
    """Poll GET /v1/transcriptions/{id} until status is completed|error."""
    deadline = time.monotonic() + POLL_TIMEOUT_S
    url = f"{API_BASE}/v1/transcriptions/{transcription_id}"
    poll_count = 0
    while True:
        if time.monotonic() > deadline:
            logger.error(
                "soniox poll timed out: id=%s after %ds (%d polls)",
                transcription_id,
                POLL_TIMEOUT_S,
                poll_count,
            )
            raise RuntimeError(f"soniox poll timed out after {POLL_TIMEOUT_S}s")
        time.sleep(POLL_INTERVAL_S)
        poll_count += 1
        r = requests.get(url, headers=auth_headers(token), timeout=60)
        if r.status_code != 200:
            logger.error(
                "soniox poll failed: status=%d body=%s", r.status_code, r.text[:300]
            )
        r.raise_for_status()
        cur = r.json() or {}
        status = cur.get("status")
        logger.debug(
            "soniox poll #%d: id=%s status=%s", poll_count, transcription_id, status
        )
        if status == "completed":
            logger.info(
                "soniox transcription completed: id=%s after %d poll(s)",
                transcription_id,
                poll_count,
            )
            return
        if status == "error":
            logger.error(
                "soniox transcription error: id=%s error_message=%s",
                transcription_id,
                cur.get("error_message"),
            )
            raise RuntimeError(
                f"soniox transcription error: {cur.get('error_message')!r}"
            )


def fetch_transcript(transcription_id: str, token: str) -> list[dict[str, Any]]:
    r = requests.get(
        f"{API_BASE}/v1/transcriptions/{transcription_id}/transcript",
        headers=auth_headers(token),
        timeout=60,
    )
    if r.status_code != 200:
        logger.error(
            "soniox fetch_transcript failed: status=%d body=%s",
            r.status_code,
            r.text[:300],
        )
    r.raise_for_status()
    payload = r.json() or {}
    tokens = payload.get("tokens")
    if not isinstance(tokens, list):
        raise RuntimeError(
            f"soniox transcript response missing tokens[] array: {r.text[:400]}"
        )
    logger.info("soniox transcript fetched: token_count=%d", len(tokens))
    return tokens


def cleanup(transcription_id: str, file_id: str, token: str) -> None:
    """Best-effort delete of the transcription + uploaded file. Never fatal —
    a cleanup failure must not fail the whole eval run."""
    try:
        r = requests.delete(
            f"{API_BASE}/v1/transcriptions/{transcription_id}",
            headers=auth_headers(token),
            timeout=30,
        )
        logger.debug(
            "soniox cleanup delete transcription: id=%s status=%d",
            transcription_id,
            r.status_code,
        )
    except requests.RequestException:
        logger.warning(
            "soniox cleanup: failed to delete transcription id=%s (non-fatal)",
            transcription_id,
            exc_info=True,
        )
    try:
        r = requests.delete(
            f"{API_BASE}/v1/files/{file_id}", headers=auth_headers(token), timeout=30
        )
        logger.debug(
            "soniox cleanup delete file: id=%s status=%d", file_id, r.status_code
        )
    except requests.RequestException:
        logger.warning(
            "soniox cleanup: failed to delete file id=%s (non-fatal)",
            file_id,
            exc_info=True,
        )


def is_translation_token(token: dict[str, Any]) -> bool:
    return token.get("translation_status") == "translation"


def split_into_runs(
    tokens: list[dict[str, Any]],
) -> list[tuple[str, list[dict[str, Any]]]]:
    """Split the flat token stream into alternating (kind, tokens) runs,
    kind in {"source", "translation"}. A run is a maximal contiguous
    sequence of tokens of the same kind. Any translation_status other than
    the literal string "translation" (None, "original", "none", or an
    unrecognized value) is treated as source — logged if unrecognized so a
    future API change is visible rather than silently mis-bucketed."""
    seen_statuses: set[str] = set()
    runs: list[tuple[str, list[dict[str, Any]]]] = []
    current_kind: str | None = None
    current_tokens: list[dict[str, Any]] = []
    for tok in tokens:
        status = tok.get("translation_status")
        if status is not None:
            seen_statuses.add(str(status))
        if status not in (None, "translation", "original", "none"):
            logger.warning(
                "soniox unrecognized translation_status=%r on token %r — treating as source",
                status,
                tok,
            )
        kind = "translation" if is_translation_token(tok) else "source"
        if kind != current_kind:
            if current_tokens:
                runs.append((current_kind, current_tokens))
            current_kind = kind
            current_tokens = []
        current_tokens.append(tok)
    if current_tokens:
        runs.append((current_kind, current_tokens))
    logger.info(
        "soniox token stream split into %d run(s); translation_status values seen=%s",
        len(runs),
        sorted(seen_statuses),
    )
    return runs


def merge_tokens_to_words(tokens: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Merge a run of (possibly sub-word) SOURCE tokens into whole words.

    A token with a LEADING SPACE in its `text` (or a token that is pure
    whitespace) signals a word boundary; a token with no leading space
    continues the current word. This is deliberately tolerant of Soniox
    emitting either dedicated whitespace tokens or leading-space-prefixed
    word-start tokens (both conventions are documented/observed for
    BPE-style STT tokenizers; Soniox's own docs describe the stream as
    "subwords, words, or spaces" without committing to one shape) — an
    empty current-word buffer ALSO forces the next non-empty token to start
    a fresh word, so a dedicated whitespace-token separator is handled
    identically to a leading-space-on-the-next-token separator.

    Returns eval-schema words: {text, start_ms, end_ms, confidence}.
    """
    words: list[dict[str, Any]] = []
    buf_text = ""
    buf_start: int | None = None
    buf_end: int | None = None
    buf_confidences: list[float] = []

    def flush() -> None:
        nonlocal buf_text, buf_start, buf_end, buf_confidences
        if buf_text and buf_start is not None and buf_end is not None:
            words.append(
                {
                    "text": buf_text,
                    "start_ms": buf_start,
                    "end_ms": buf_end,
                    "confidence": (
                        sum(buf_confidences) / len(buf_confidences)
                        if buf_confidences
                        else 0.9
                    ),
                }
            )
        buf_text = ""
        buf_start = None
        buf_end = None
        buf_confidences = []

    for tok in tokens:
        raw = tok.get("text") or ""
        ts = tok.get("start_ms")
        te = tok.get("end_ms")
        if ts is None or te is None:
            logger.warning("soniox source token missing start_ms/end_ms: %r", tok)
            continue
        has_leading_ws = len(raw) > 0 and raw[0].isspace()
        content = raw.strip()
        if not content:
            # Pure whitespace token — separator only, no content to add.
            flush()
            continue
        starts_new_word = has_leading_ws or not buf_text
        if starts_new_word:
            flush()
            buf_text = content
            buf_start = int(ts)
        else:
            buf_text += content
        buf_end = int(te)
        conf = tok.get("confidence")
        if conf is not None:
            buf_confidences.append(float(conf))
    flush()
    return words


def group_words_into_lines(words: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Split a flat word stream into lyric lines on silence gaps > LINE_GAP_MS.
    Structurally identical to assemblyai_universal_3_pro.py's grouping so the
    two backends' line-segmentation behavior is directly comparable."""
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


def join_translation_text(tokens: list[dict[str, Any]]) -> str:
    """Join a translation run's token texts into one string, stripping the
    single leading separator (if any) so the segment doesn't start with a
    stray space."""
    return "".join(tok.get("text") or "" for tok in tokens).strip()


def build_lines(
    tokens: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Turn the flat token stream into eval lines + a translation-alignment
    report for metadata. Returns (lines, translation_report)."""
    runs = split_into_runs(tokens)

    all_lines: list[dict[str, Any]] = []
    unaligned_segments: list[dict[str, Any]] = []
    n_translation_runs = 0
    n_aligned = 0
    n_unaligned_multi_line = 0
    n_orphan_translation = 0
    pending_source_line_indices: list[int] | None = None

    for kind, run_tokens in runs:
        if kind == "source":
            words = merge_tokens_to_words(run_tokens)
            new_lines = group_words_into_lines(words)
            start_idx = len(all_lines)
            all_lines.extend(new_lines)
            pending_source_line_indices = list(range(start_idx, len(all_lines)))
        else:  # translation
            n_translation_runs += 1
            text_sk = join_translation_text(run_tokens)
            if not pending_source_line_indices:
                logger.warning(
                    "soniox translation run with no preceding source line "
                    "(text_sk=%r) — cannot align, dropping",
                    text_sk[:80],
                )
                n_orphan_translation += 1
                unaligned_segments.append(
                    {"reason": "no_preceding_source_line", "text_sk": text_sk}
                )
            elif len(pending_source_line_indices) == 1:
                idx = pending_source_line_indices[0]
                all_lines[idx]["text_sk"] = text_sk
                n_aligned += 1
            else:
                logger.warning(
                    "soniox translation segment spans %d lines (indices=%s) — "
                    "cannot honestly split text_sk across lines, leaving None",
                    len(pending_source_line_indices),
                    pending_source_line_indices,
                )
                n_unaligned_multi_line += 1
                unaligned_segments.append(
                    {
                        "reason": "multi_line_segment",
                        "line_indices": pending_source_line_indices,
                        "text_sk": text_sk,
                    }
                )
            pending_source_line_indices = None

    report = {
        "n_runs": len(runs),
        "n_translation_runs": n_translation_runs,
        "n_lines_aligned_1to1": n_aligned,
        "n_segments_unaligned_multi_line": n_unaligned_multi_line,
        "n_orphan_translation_segments": n_orphan_translation,
        "unaligned_translation_segments": unaligned_segments,
    }
    logger.info(
        "soniox translation alignment: runs=%d translation_runs=%d aligned_1to1=%d "
        "unaligned_multi_line=%d orphan=%d",
        len(runs),
        n_translation_runs,
        n_aligned,
        n_unaligned_multi_line,
        n_orphan_translation,
    )
    return all_lines, report


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
        "soniox_v5 starting: wav=%s out=%s model=%s target_language=%s",
        args.wav,
        args.out,
        MODEL_SLUG,
        TARGET_LANGUAGE,
    )

    token = os.environ.get("SONIOX_API_KEY")
    if not token:
        logger.error("SONIOX_API_KEY not set — aborting")
        print("SONIOX_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    file_id = upload_audio(args.wav, token)
    tid = create_transcription(file_id, token)
    try:
        poll_transcription(tid, token)
        tokens = fetch_transcript(tid, token)
    finally:
        cleanup(tid, file_id, token)
    elapsed = time.time() - t0
    logger.info("soniox_v5 call complete: elapsed_s=%.1f", elapsed)

    if not tokens:
        raise RuntimeError(
            f"soniox transcript returned an empty tokens[] array (transcription_id={tid})"
        )

    lines, translation_report = build_lines(tokens)

    source_tokens = [t for t in tokens if not is_translation_token(t)]
    confidences = [
        float(t["confidence"]) for t in source_tokens if t.get("confidence") is not None
    ]
    raw_confidence = sum(confidences) / len(confidences) if confidences else 0.9

    languages_seen = sorted({t.get("language") for t in tokens if t.get("language")})

    emit_result(
        out_path=args.out,
        wav_path=str(args.wav),
        duration_ms=estimate_duration_ms(lines),
        lines=lines,
        raw_confidence=raw_confidence,
        metadata={
            "model": MODEL_SLUG,
            "transcription_id": tid,
            "elapsed_s": round(elapsed, 1),
            "token_count": len(tokens),
            "source_token_count": len(source_tokens),
            "line_count": len(lines),
            "target_language": TARGET_LANGUAGE,
            "languages_seen": languages_seen,
            "line_gap_ms": LINE_GAP_MS,
            "translation_alignment": translation_report,
        },
    )
    logger.info(
        "soniox_v5 done: lines=%d out=%s elapsed_s=%.1f",
        len(lines),
        args.out,
        elapsed,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
