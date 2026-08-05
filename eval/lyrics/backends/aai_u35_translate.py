#!/usr/bin/env python3
"""aai_u35_translate.py — AssemblyAI Universal-3.5 Pro + same-call SK translation.

North-star spike backend: a single AssemblyAI transcription job that both
transcribes with the flagship `universal-3.5-pro` speech model AND requests
Slovak translation via AssemblyAI's Speech Understanding feature in the same
`/v2/transcript` request (`translation.formal=true`,
`translation.match_original_utterance=true` so the translated text carries
utterance-level timing instead of arriving untimed).

Reuses the three-step async transcription protocol (upload -> create ->
poll) and endpoint/auth conventions from both this project's existing
`assemblyai_universal_3_pro.py` eval backend and the production Rust client
at `crates/sp-server/src/lyrics/asr_path/aai_backend.rs` (raw `authorization`
header, no `Bearer` prefix; `speech_models` plural list field; 2 s poll
interval / 1800 s poll timeout matching `aai_backend.rs`'s
`POLL_INTERVAL`/`POLL_TIMEOUT_S`) — written as idiomatic standalone Python
per this harness's convention rather than a port of the Rust types.

**Documented limitation**: AssemblyAI's translation feature is
utterance-scoped, not word-scoped — `text_sk` on each emitted line carries
only the ORIGINAL utterance's `start_ms`/`end_ms` (no independent
translated-word timing exists to align against). This is a real fidelity
gap vs the Gemini one-call backends (which emit `text_sk` on the same
line-level granularity as the source but from a model that reasons about
timing per line, not per fixed utterance boundary) and should factor into
any promotion decision.

Line-grouping: unlike `assemblyai_universal_3_pro.py` (which derives lines
from a flat `words[]` stream via a silence-gap heuristic because it does
not request translation), this backend's request-shape assumes AAI returns
native `utterances[]` once translation is enabled (translation must anchor
to a segment boundary) — see `utterances_to_lines()`. If a future AAI
response omits `utterances` (translation feature not yet live / disabled
account-side), this backend FAILS LOUDLY rather than silently guessing
line boundaries from an unrelated heuristic.

Usage:
    python eval/lyrics/backends/aai_u35_translate.py \\
        --wav /abs/path/vocal16k.wav \\
        --out /tmp/result.json

Reads ASSEMBLYAI_API_KEY from the environment.
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

logger = logging.getLogger("lyrics_eval.aai_u35_translate")

BACKEND_ID = "aai-u35-translate"
BACKEND_REVISION = 1
API_BASE = "https://api.assemblyai.com/v2"
SPEECH_MODEL = "universal-3.5-pro"
TARGET_LANGUAGE = "sk"

# Poll cadence + overall timeout — mirrors assemblyai_universal_3_pro.py and
# crates/sp-server/src/lyrics/asr_path/aai_backend.rs::POLL_INTERVAL/POLL_TIMEOUT_S.
POLL_INTERVAL_S = 2.0
POLL_TIMEOUT_S = 1800  # 30 min


def auth_headers(token: str) -> dict[str, str]:
    """AssemblyAI uses raw token (no Bearer prefix)."""
    return {"authorization": token}


def upload_audio(audio_path: Path, token: str) -> str:
    """Stream raw bytes to /v2/upload and return the upload URL."""
    size = audio_path.stat().st_size
    logger.info("aai upload starting: path=%s bytes=%d", audio_path, size)
    with audio_path.open("rb") as fh:
        r = requests.post(
            f"{API_BASE}/upload",
            headers=auth_headers(token),
            data=fh.read(),
            timeout=300,
        )
    if r.status_code != 200:
        logger.error(
            "aai upload failed: status=%d body=%s", r.status_code, r.text[:300]
        )
    r.raise_for_status()
    url = (r.json() or {}).get("upload_url")
    if not url:
        raise RuntimeError(
            f"assemblyai upload response missing upload_url: {r.text[:300]}"
        )
    logger.info("aai upload ok: upload_url obtained")
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
        # Speech Understanding same-call translation to Slovak.
        # `match_original_utterance=True` keeps translated text aligned 1:1
        # with the ORIGINAL-language utterance boundaries (so the translated
        # text can borrow that utterance's start/end) rather than AAI
        # re-segmenting the translation independently.
        "translation": {
            "target_languages": [TARGET_LANGUAGE],
            "formal": True,
            "match_original_utterance": True,
        },
    }
    logger.info(
        "aai create_transcript: model=%s target_language=%s",
        SPEECH_MODEL,
        TARGET_LANGUAGE,
    )
    r = requests.post(
        f"{API_BASE}/transcript",
        headers={**auth_headers(token), "Content-Type": "application/json"},
        json=body,
        timeout=60,
    )
    if r.status_code != 200:
        logger.error(
            "aai create_transcript failed: status=%d body=%s",
            r.status_code,
            r.text[:300],
        )
    r.raise_for_status()
    payload = r.json() or {}
    tid = payload.get("id")
    if not tid:
        raise RuntimeError(f"assemblyai transcript create missing id: {r.text[:300]}")
    logger.info("aai transcript created: id=%s", tid)
    return tid


def poll_transcript(transcript_id: str, token: str) -> dict[str, Any]:
    deadline = time.monotonic() + POLL_TIMEOUT_S
    url = f"{API_BASE}/transcript/{transcript_id}"
    poll_count = 0
    while True:
        if time.monotonic() > deadline:
            logger.error(
                "aai poll timed out: id=%s after %ds (%d polls)",
                transcript_id,
                POLL_TIMEOUT_S,
                poll_count,
            )
            raise RuntimeError(f"assemblyai poll timed out after {POLL_TIMEOUT_S}s")
        time.sleep(POLL_INTERVAL_S)
        poll_count += 1
        r = requests.get(url, headers=auth_headers(token), timeout=60)
        r.raise_for_status()
        cur = r.json() or {}
        status = cur.get("status")
        logger.debug("aai poll #%d: id=%s status=%s", poll_count, transcript_id, status)
        if status == "completed":
            logger.info(
                "aai transcript completed: id=%s after %d poll(s)",
                transcript_id,
                poll_count,
            )
            return cur
        if status == "error":
            logger.error(
                "aai transcript error: id=%s error=%s", transcript_id, cur.get("error")
            )
            raise RuntimeError(f"assemblyai transcript error: {cur.get('error')!r}")


def _assign_words_to_range(
    words: list[dict[str, Any]], start_ms: int, end_ms: int
) -> list[dict[str, Any]]:
    """Filter the flat word stream to words whose start falls within [start_ms, end_ms]."""
    assigned: list[dict[str, Any]] = []
    for w in words:
        wt = (w.get("text") or "").strip()
        ws = w.get("start")
        we = w.get("end")
        if not wt or ws is None or we is None:
            continue
        if start_ms <= ws <= end_ms:
            assigned.append(
                {
                    "text": wt,
                    "start_ms": int(ws),
                    "end_ms": int(we),
                    "confidence": float(w.get("confidence") or 0.9),
                }
            )
    return assigned


def utterances_to_lines(
    utterances: list[dict[str, Any]],
    translated_by_index: dict[int, str],
    words: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Map AAI `utterances[]` + per-index translated text into eval lines.

    Each emitted line: {text, start_ms, end_ms, text_sk, words}. `words` is
    the flat word stream sliced to this utterance's time range (original
    language only — the translated text has NO independent word timing, see
    module docstring's "Documented limitation").
    """
    lines: list[dict[str, Any]] = []
    missing_translation = 0
    for idx, utt in enumerate(utterances):
        text = (utt.get("text") or "").strip()
        if not text:
            continue
        start = utt.get("start")
        end = utt.get("end")
        if start is None or end is None:
            raise RuntimeError(f"aai utterance missing start/end: {utt!r}")
        start_ms = int(start)
        end_ms = int(end)

        text_sk = translated_by_index.get(idx)
        if text_sk is None:
            missing_translation += 1
        else:
            text_sk = text_sk.strip() or None

        utt_words = utt.get("words")
        if isinstance(utt_words, list) and utt_words:
            line_words = _assign_words_to_range(utt_words, start_ms, end_ms)
        else:
            line_words = _assign_words_to_range(words, start_ms, end_ms)

        lines.append(
            {
                "text": text,
                "start_ms": start_ms,
                "end_ms": end_ms,
                # Documented limitation: text_sk carries only the ORIGINAL
                # utterance's start_ms/end_ms — AAI's translation has no
                # independent word-level (or finer-than-utterance) timing.
                "text_sk": text_sk,
                "words": line_words if line_words else None,
            }
        )

    logger.info(
        "aai utterances mapped: utterances=%d kept_lines=%d missing_translation=%d",
        len(utterances),
        len(lines),
        missing_translation,
    )
    if missing_translation:
        logger.warning(
            "aai translation missing on %d/%d utterances — sk coverage gap",
            missing_translation,
            len(utterances),
        )
    return lines


def extract_translated_by_index(transcript: dict[str, Any]) -> dict[int, str]:
    """Pull the per-utterance Slovak translation, keyed by utterance index.

    Expected response shape (Speech Understanding translation, keyed by
    target language code): `transcript["translation"]["sk"]["utterances"]`,
    a list positionally aligned with `transcript["utterances"]` because
    `match_original_utterance=True` was requested. Raises if the
    `translation` block is entirely absent — a silently-empty text_sk on
    every line would hide a real API/config failure (the whole point of
    this backend is capturing text_sk), so this is a loud failure, not a
    silent fallback, per script-failure-policy.
    """
    translation_block = transcript.get("translation")
    if not translation_block:
        raise RuntimeError(
            "assemblyai response missing 'translation' block entirely — "
            "translation.target_languages=['sk'] was requested but the API "
            "did not return it; check account Speech Understanding access"
        )
    lang_block = translation_block.get(TARGET_LANGUAGE)
    if not lang_block:
        raise RuntimeError(
            f"assemblyai translation block missing target language {TARGET_LANGUAGE!r}: "
            f"{json.dumps(translation_block)[:300]}"
        )
    translated_utterances = lang_block.get("utterances")
    if not isinstance(translated_utterances, list):
        raise RuntimeError(
            f"assemblyai translation.{TARGET_LANGUAGE} missing 'utterances' list: "
            f"{json.dumps(lang_block)[:300]}"
        )
    by_index: dict[int, str] = {}
    for idx, item in enumerate(translated_utterances):
        text = (item.get("text") or "").strip() if isinstance(item, dict) else ""
        if text:
            by_index[idx] = text
    logger.debug(
        "aai translation extracted: translated_utterances=%d non_empty=%d",
        len(translated_utterances),
        len(by_index),
    )
    return by_index


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
        "aai_u35_translate starting: wav=%s out=%s model=%s target_language=%s",
        args.wav,
        args.out,
        SPEECH_MODEL,
        TARGET_LANGUAGE,
    )

    token = os.environ.get("ASSEMBLYAI_API_KEY")
    if not token:
        logger.error("ASSEMBLYAI_API_KEY not set — aborting")
        print("ASSEMBLYAI_API_KEY not set", flush=True)
        return 2

    t0 = time.time()
    audio_url = upload_audio(args.wav, token)
    tid = create_transcript(audio_url, token)
    transcript = poll_transcript(tid, token)
    elapsed = time.time() - t0
    logger.info("aai_u35_translate call complete: elapsed_s=%.1f", elapsed)

    utterances = transcript.get("utterances")
    if not isinstance(utterances, list) or not utterances:
        logger.error(
            "aai response missing utterances[] — cannot map translation to lines "
            "(transcript_id=%s)",
            tid,
        )
        raise RuntimeError(
            "assemblyai response has no 'utterances' — this backend requires "
            "utterance-level segmentation to anchor the same-call SK translation; "
            f"transcript_id={tid}"
        )
    words = transcript.get("words") or []
    translated_by_index = extract_translated_by_index(transcript)
    lines = utterances_to_lines(utterances, translated_by_index, words)

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
            "utterance_count": len(utterances),
            "line_count": len(lines),
            "target_language": TARGET_LANGUAGE,
            "language_code": transcript.get("language_code"),
            "audio_duration_s": transcript.get("audio_duration"),
        },
    )
    logger.info(
        "aai_u35_translate done: lines=%d out=%s elapsed_s=%.1f",
        len(lines),
        args.out,
        elapsed,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
