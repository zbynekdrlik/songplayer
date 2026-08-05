#!/usr/bin/env python3
"""elevenlabs_fa.py — ElevenLabs Forced Alignment API backend.

A TRUE forced aligner: it receives FIXED reference text (an audio-LLM's
already-produced `lines[].text`, English as sung) plus the isolated-vocal
WAV, and returns per-word timestamps for that EXACT text — it does not
transcribe or reinterpret. Unlike the ASR backends in this harness
(`soniox_v5.py`, `aai_u35_translate.py`), there is no text-similarity
matching step needed between produced and reference text: the words we send
ARE the words that come back, in the same order, just timed.

Verified against the live docs 2026-08-05 (decisive quotes, exact URLs):

- **Endpoint + method** — `POST https://api.elevenlabs.io/v1/forced-alignment`
  (docs.elevenlabs.io/api-reference/forced-alignment/create).
- **Request shape** — multipart/form-data, two fields:
  - `file` (binary): "The file to align. All major audio formats are
    supported. The file size must be less than 1GB." (api-reference/
    forced-alignment/create)
  - `text` (string): "The text to align with the audio." (same page).
    The capabilities page adds: "Plain string only — do not wrap input
    text in JSON or any other structure." (docs.elevenlabs.io/overview/
    capabilities/forced-alignment)
- **Auth** — header `xi-api-key: <ELEVENLABS_API_KEY>` (api-reference page
  lists it as a header parameter; in practice it is required — a request
  without it returns 401, verified empirically 2026-08-05).
- **Limits** (docs.elevenlabs.io/overview/capabilities/forced-alignment):
  max audio duration "10 hours"; max text length "675,000 characters".
  Two DIFFERENT max-file-size figures appear across the two doc pages —
  the api-reference page's own field description says "less than 1GB"
  while the capabilities overview page separately states "3 GB" as a
  headline limit. Reported here verbatim rather than silently picking
  one; every fixture WAV in this eval is 9-33 MB, far under either
  figure, so the discrepancy does not affect this run.
- **Languages** — capabilities page: "29 languages" including "English
  (USA, UK, Australia, Canada)" and "Spanish (Spain, Mexico)" — both
  languages this catalog needs (`multi_language` fixtures code-switch
  EN/ES) are confirmed supported.
- **Pricing** — capabilities page: "the same rate as the Speech to Text
  API" (per-audio-second, not per-request; exact $/min not restated here
  since it is account-plan-dependent — see the account's own usage
  dashboard for the actual charge incurred by this run).
- **Diarization** — capabilities page: "Not supported; providing diarized
  text will produce unexpected results." Not relevant here (isolated
  solo-vocal stems, single-speaker reference text, matching this
  project's other backends' "no diarization needed" convention).
- **Response shape** — verified against the live 200 response (see
  `README.md` in this directory for the first real captured response):
  top-level `{characters: [...], words: [...], loss: <float>}`.
  - `characters[]`: `{text, start, end}` — one per character of the
    EXACT text sent, `start`/`end` in **seconds** (float).
  - `words[]`: `{text, start, end, loss}`. The docs describe this as "List
    of words with their timing information", which reads as one entry per
    whitespace-delimited word — **that is NOT what the live API returns.**
    Verified empirically on the first real call (276 tokens sent -> 551
    `words[]` entries back, exactly `2*276 - 1`): the array INTERLEAVES a
    separate whitespace-text entry (`{"text": " ", ...}`) between every
    pair of real words — the same convention `soniox_v5.py` already
    documented for a different vendor's token stream. `loss` = "The
    average alignment loss/confidence score for this word, calculated
    from its constituent characters" (LOWER is more confident — this is a
    loss, not a probability; correlated against timing accuracy in
    `confidence_correlation.py`). `filter_content_words()` strips the
    whitespace entries before this backend's word-index mapping runs.
  - `loss` (top-level, float): "The average alignment loss/confidence
    score for the entire transcript."

Reference text preparation (this module's own design, not from the docs):
each source line's `text` is whitespace-tokenized (same convention as
`combine_lines_times.tokenize_line_text` — punctuation stays attached to
its token) and every line's tokens are concatenated, single-space-joined,
into ONE transcript string sent as the `text` field — line boundaries are
NOT preserved in the request (the API has no line concept, only words).
The token->line mapping is kept locally so the returned flat `words[]`
array can be split back into per-line `start_ms`/`end_ms` — **line
start_ms = the first aligned word's start_ms, per the task brief** — using
plain index arithmetic (word i in the transcript came from whichever line
contributed word i when the transcript was built; no fuzzy text matching
needed, unlike `combine_lines_times.py`, because there is no ASR
transcription disagreement to reconcile — the aligner is timing the exact
text we sent). If the API ever returns a DIFFERENT word count than the
number of tokens we sent, this is treated as an unexpected-shape failure
(per script-failure-policy) — the positional mapping is invalid and must
not be silently used, so the whole fixture fails loudly with both token
and word counts logged rather than producing a mis-mapped result.

A line with zero words in it (empty `text`) is UNTIMED by construction —
no API call has anything to align for it.

Usage:
    python eval/lyrics/aligners_11l/elevenlabs_fa.py \\
        --wav /abs/path/vocal16k.wav \\
        --text-json /abs/path/qwen35-omni_<video_id>.json \\
        --out /abs/path/elevenlabs-fa_<video_id>.json

`--text-json` is any file shaped `{"lines": [{"text": ..., "text_sk": ...},
...]}` (the same backend-output shape every other backend in this harness
emits) — only `lines[].text` (and, if present, `lines[].text_sk`, carried
through untouched for the wall's Slovak display) are read; timestamps in
that file, if any, are never consulted.

Reads ELEVENLABS_API_KEY from the environment.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import wave
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("lyrics_eval.elevenlabs_fa")

BACKEND_ID = "elevenlabs-fa"
BACKEND_REVISION = 1
API_URL = "https://api.elevenlabs.io/v1/forced-alignment"

# Generous — audio is short (single songs, minutes not hours) but multipart
# upload of a 20-30MB WAV over a real network plus server-side alignment
# compute needs real headroom, not a tight timeout guessed from nothing.
REQUEST_TIMEOUT_S = 900


def tokenize_line_text(text: str) -> list[str]:
    """Whitespace-split a line's text into words, in order, punctuation left
    attached. Identical convention to combine_lines_times.tokenize_line_text
    — kept as a local copy (not imported) so this module has zero
    cross-import dependency on the offline-combine-experiment code, which is
    a different, unrelated tool this backend does not otherwise use."""
    return text.split()


def load_reference_lines(text_json_path: Path) -> list[dict[str, Any]]:
    data = json.loads(text_json_path.read_text(encoding="utf-8"))
    lines = data.get("lines")
    if not isinstance(lines, list) or not lines:
        raise RuntimeError(f"reference text-json {text_json_path} has no lines[] array")
    return lines


def build_transcript(
    ref_lines: list[dict[str, Any]],
) -> tuple[str, list[int], list[int]]:
    """Returns (transcript, word_to_line, words_per_line_count).

    `transcript` is every reference line's tokens, single-space-joined, in
    line order. `word_to_line[i]` is the source line index of the i-th
    whitespace token in `transcript`. `words_per_line_count[line_idx]` is
    how many tokens that line contributed (0 for an empty-text line)."""
    tokens: list[str] = []
    word_to_line: list[int] = []
    words_per_line = [0] * len(ref_lines)
    for line_idx, line in enumerate(ref_lines):
        text = (line.get("text") or "").strip()
        line_tokens = tokenize_line_text(text)
        words_per_line[line_idx] = len(line_tokens)
        for tok in line_tokens:
            tokens.append(tok)
            word_to_line.append(line_idx)
    transcript = " ".join(tokens)
    logger.info(
        "built transcript: n_lines=%d n_tokens=%d n_empty_lines=%d chars=%d",
        len(ref_lines),
        len(tokens),
        sum(1 for c in words_per_line if c == 0),
        len(transcript),
    )
    return transcript, word_to_line, words_per_line


def call_forced_alignment(
    wav_path: Path, transcript: str, api_key: str
) -> dict[str, Any]:
    size = wav_path.stat().st_size
    logger.info(
        "elevenlabs forced-alignment request starting: wav=%s bytes=%d "
        "transcript_chars=%d transcript_words=%d",
        wav_path,
        size,
        len(transcript),
        len(transcript.split()),
    )
    with wav_path.open("rb") as fh:
        r = requests.post(
            API_URL,
            headers={"xi-api-key": api_key},
            files={"file": (wav_path.name, fh, "audio/wav")},
            data={"text": transcript},
            timeout=REQUEST_TIMEOUT_S,
        )
    logger.info("elevenlabs forced-alignment HTTP status=%d", r.status_code)
    if r.status_code != 200:
        logger.error(
            "elevenlabs forced-alignment failed: status=%d body=%s",
            r.status_code,
            r.text[:3000],
        )
        raise RuntimeError(
            f"elevenlabs forced-alignment HTTP {r.status_code}: {r.text[:3000]}"
        )
    try:
        payload = r.json()
    except ValueError as exc:
        raise RuntimeError(
            f"elevenlabs forced-alignment returned non-JSON body: {r.text[:1000]}"
        ) from exc
    if not isinstance(payload, dict) or "words" not in payload:
        raise RuntimeError(
            f"elevenlabs forced-alignment response missing 'words': "
            f"keys={list(payload.keys()) if isinstance(payload, dict) else type(payload)}"
        )
    logger.info(
        "elevenlabs forced-alignment response: n_words_raw=%d top_level_loss=%s",
        len(payload["words"]),
        payload.get("loss"),
    )
    return payload


def filter_content_words(words: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Strips the interleaved whitespace-only entries the live API returns
    between every pair of real words (empirically 2*n_real_words - 1 total
    entries — see the module docstring). A "content" word is any entry
    whose text is non-empty after stripping whitespace; a pure-whitespace
    entry (including a lone space, tab, or newline) is dropped."""
    return [w for w in words if (w.get("text") or "").strip() != ""]


def reconstruct_lines(
    ref_lines: list[dict[str, Any]],
    word_to_line: list[int],
    content_words: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Splits the flat, whitespace-filtered `content_words` array back into
    per-line dicts using the word_to_line index built at request time — no
    fuzzy text matching, since the aligner returns exactly the words we
    sent, in order. Fails loudly (RuntimeError) if the word count does not
    match the token count, since the positional mapping would otherwise be
    silently wrong.

    Returns (lines_out, stats) where stats carries n_lines_untimed and
    n_words_missing_timing for logging/reporting."""
    if len(content_words) != len(word_to_line):
        raise RuntimeError(
            f"elevenlabs word count mismatch: sent {len(word_to_line)} tokens, "
            f"response has {len(content_words)} content words (after filtering "
            f"interleaved whitespace entries) — refusing to build a positional "
            f"mapping that may be misaligned"
        )

    # Group content_words by their originating line index.
    words_by_line: list[list[dict[str, Any]]] = [[] for _ in ref_lines]
    for line_idx, word in zip(word_to_line, content_words, strict=True):
        words_by_line[line_idx].append(word)

    lines_out: list[dict[str, Any]] = []
    n_lines_untimed = 0
    n_words_missing_timing = 0

    for line_idx, ref_line in enumerate(ref_lines):
        line_words = words_by_line[line_idx]
        timed_word_spans: list[tuple[float, float]] = []
        losses: list[float] = []
        words_ms: list[dict[str, Any]] = []
        for w in line_words:
            start = w.get("start")
            end = w.get("end")
            if start is None or end is None:
                # Defensive: exclude just this word from timing/line-span
                # computation rather than crashing the whole fixture — the
                # word-count check above already guarantees the POSITIONAL
                # mapping is sound; a single word missing its own start/end
                # inside an otherwise-valid response is a per-word gap, not
                # a shape failure.
                n_words_missing_timing += 1
                continue
            timed_word_spans.append((float(start), float(end)))
            word_loss = w.get("loss")
            if word_loss is not None:
                losses.append(float(word_loss))
            words_ms.append(
                {
                    "text": w.get("text"),
                    "start_ms": round(float(start) * 1000),
                    "end_ms": round(float(end) * 1000),
                    "loss": word_loss,
                }
            )

        if not timed_word_spans:
            n_lines_untimed += 1
            start_ms = None
            end_ms = None
            mean_word_loss = None
        else:
            start_ms = round(min(s for s, _e in timed_word_spans) * 1000)
            end_ms = round(max(e for _s, e in timed_word_spans) * 1000)
            mean_word_loss = sum(losses) / len(losses) if losses else None

        lines_out.append(
            {
                "text": ref_line.get("text"),
                "text_sk": ref_line.get("text_sk"),
                "start_ms": start_ms,
                "end_ms": end_ms,
                "mean_word_loss": mean_word_loss,
                "words": words_ms,
            }
        )

    stats = {
        "n_lines_untimed": n_lines_untimed,
        "n_words_missing_timing": n_words_missing_timing,
        "n_lines_total": len(ref_lines),
    }
    logger.info(
        "reconstructed lines: n_lines=%d n_untimed=%d n_words_missing_timing=%d",
        stats["n_lines_total"],
        stats["n_lines_untimed"],
        stats["n_words_missing_timing"],
    )
    return lines_out, stats


def estimate_duration_ms(wav_path: Path) -> int | None:
    """Reads the WAV header (via stdlib `wave`) to get duration without a
    full decode. Returns None (never fabricates a value) if the file is not
    a readable PCM WAV — logged, not fatal, since duration_ms is metadata,
    not something scoring depends on."""
    try:
        with wave.open(str(wav_path), "rb") as wf:
            frames = wf.getnframes()
            rate = wf.getframerate()
            if rate <= 0:
                return None
            return round(frames / rate * 1000)
    except (wave.Error, OSError, EOFError) as exc:
        logger.warning("could not read WAV duration for %s: %s", wav_path, exc)
        return None


def emit_result(
    *,
    wav_path: Path,
    duration_ms: int | None,
    lines_out: list[dict[str, Any]],
    top_level_loss: float | None,
    stats: dict[str, Any],
    out_path: Path,
) -> None:
    result = {
        "backend_id": BACKEND_ID,
        "backend_revision": BACKEND_REVISION,
        "wav_path": str(wav_path),
        "duration_ms": duration_ms,
        "lines": lines_out,
        # Deliberately None, never fabricated: `loss` is a LOSS (lower =
        # more confident), not a 0-1 probability — reporting it under the
        # harness's generic `raw_confidence` field would misleadingly imply
        # "higher is better". `top_level_loss` below carries the real value
        # under its own honestly-named key instead.
        "raw_confidence": None,
        "top_level_loss": top_level_loss,
        "metadata": {
            "n_lines_untimed": stats["n_lines_untimed"],
            "n_words_missing_timing": stats["n_words_missing_timing"],
        },
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    logger.info("wrote %s", out_path)


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(levelname)s %(name)s: %(message)s"
    )

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--text-json", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)

    api_key = os.environ.get("ELEVENLABS_API_KEY")
    if not api_key:
        raise RuntimeError(
            "ELEVENLABS_API_KEY not set in environment — refusing to call the "
            "API without it (would fail with an opaque 401 anyway)"
        )

    if not args.wav.exists():
        raise RuntimeError(f"wav file not found: {args.wav}")

    ref_lines = load_reference_lines(args.text_json)
    transcript, word_to_line, _words_per_line = build_transcript(ref_lines)
    if not transcript.strip():
        raise RuntimeError(
            f"reference text-json {args.text_json} produced an empty transcript "
            f"(every line's text was blank) — nothing to align"
        )

    payload = call_forced_alignment(args.wav, transcript, api_key)
    content_words = filter_content_words(payload["words"])
    lines_out, stats = reconstruct_lines(ref_lines, word_to_line, content_words)
    duration_ms = estimate_duration_ms(args.wav)

    emit_result(
        wav_path=args.wav,
        duration_ms=duration_ms,
        lines_out=lines_out,
        top_level_loss=payload.get("loss"),
        stats=stats,
        out_path=args.out,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
