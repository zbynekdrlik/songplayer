#!/usr/bin/env python3
"""run.py — ctc-forced-aligner backend for the 2026-08-05 forced-aligner
shootout (MahmoudAshraf97/ctc-forced-aligner, commit
11855d1de76af2b490dd2e8e2db2661805ae90a0).

Unlike the audio-LLM backends in this harness (qwen35-omni, gemini36_flash,
...) this tool is NOT a transcriber. It receives the FIXED reference text
that qwen35-omni already produced for each fixture (`lines[].text` /
`text_sk`, pushed to `text-refs/<video_id>.json` by
`eval/lyrics/aligners/ctc_forced_aligner/push_text_refs.py`) plus the
isolated-vocal WAV, and returns per-WORD timestamps for that exact text via
CTC forced alignment (wav2vec2-CTC model
`MahmoudAshraf/mms-300m-1130-forced-aligner`). It never re-transcribes and
never reinterprets the words.

Verified against the real upstream source (not guessed from README prose)
by cloning the repo on win-resolume and reading
`ctc_forced_aligner/align.py`, `alignment_utils.py`, `text_utils.py`,
`__init__.py` directly:

  - The Python API is:
        model, tokenizer = load_alignment_model(device, model_path, attn_implementation, dtype)
        audio_waveform = load_audio(wav_path, model.dtype, model.device)
        emissions, stride = generate_emissions(model, audio_waveform, window_length, context_length, batch_size)
        tokens_starred, text_starred = preprocess_text(text, romanize, language, split_size, star_frequency)
        segments, scores, blank_token = get_alignments(emissions, tokens_starred, tokenizer)
        spans = get_spans(tokens_starred, segments, blank_token)
        results = postprocess_results(text_starred, spans, stride, scores, merge_threshold)
    `results` is a list of `{start, end, text, score}` dicts, `start`/`end`
    in SECONDS (float) — `postprocess_results` divides by 1000 internally
    (`stride` itself is in ms). ONE result per non-`<star>` entry, in the
    SAME order as `text.split()` (the CLI's own default `split_size="word"`)
    — this is what lets us slice the flat per-word result list back into
    per-reference-LINE chunks purely by counting words per line (see
    `_align_one_song` below).

  - `--star_frequency` (CLI default `"edges"`; the bare `preprocess_text()`
    function's own Python default is `"segment"` — the CLI overrides it)
    controls where `<star>` wildcard tokens are inserted into the target
    sequence before forced alignment. `"edges"` puts exactly two stars, one
    at the very start and one at the very end of the whole text. `"segment"`
    puts a star before EVERY segment — and since `split_size="word"` is also
    the default, a "segment" here is a WORD, so star_frequency="segment"
    means a `<star>` before every single word. Per the CLI help text: "Star
    token increases the accuracy of the alignment but also increases
    segment fragmentation." The `<star>` token gives the CTC decoder a
    wildcard it can freely map to any audio frame (music, ad-libs, breath,
    silence) instead of being forced to stretch/compress real words across
    those frames — task brief's own summary ("inserts a wildcard token so
    audio with no matching text doesn't get force-mapped onto real words")
    is directionally correct; "segment" is just the CLI's stronger, default
    setting of it (a wildcard opportunity between every word, not just at
    the two ends).
    This backend exposes it as `--star-frequency {none,segment}`:
    `none` (this backend's own default, i.e. "without --star_frequency
    segment" per the task brief) maps to the aligner's CLI default
    `"edges"`; `segment` maps to the aligner's `"segment"`.

  - Forced alignment is FORCED: `get_spans` walks the merged-repeat CTC path
    expecting an exact, in-order character match for every target token, so
    a successful call ALWAYS returns exactly one timestamp per input word —
    there is no per-word "not found" case the way an ASR system has. The
    only failure mode observed is a WHOLE-CALL exception (e.g. the C++
    `forced_align` implementation requires
    `len(log_probs) >= len(target_labels) + n_repeats`, which a heavily
    over-long/degenerate reference text — see the poisoned fixture,
    `Xvm4_fWkXe8` — can violate). `main()` below catches any such exception,
    marks EVERY line of that fixture UNTIMED (never guessed/interpolated),
    and still writes a valid output file with the failure recorded in
    `metadata.error` — matching the project's own untimed convention
    (`combine_lines_times.py`) instead of leaving no file at all.

  - `norm_config.py` (upstream) has no `eng`/`spa`-specific entries — every
    language not in {mon, khk, heb, tha, ara, arb, jav} falls back to the
    same shared `"*"` normalization config, and we never romanize (both
    English and Spanish are already Latin script, and the MMS-based
    alignment model natively supports both). So `LANGUAGE = "eng"` is used
    uniformly for all 22 fixtures, including the `multi_language` category —
    confirmed by spot-checking qwen35-omni's own `lines[].text` for 4
    multi_language fixtures (tCivrrU4SSM, cej4vn4sWtE, jUnyHptnsRo,
    hSMJa5tImRU): the reference TEXT is plain English throughout (qwen35-omni
    transcribes to English regardless of the sung language), so language
    selection has zero effect on this backend's behavior either way.

Usage:
    python run.py --wav <path/to/vocal16k.wav> \\
                   --text-json <path/to/text-refs/<video_id>.json> \\
                   --out <path/to/output.json> \\
                   --star-frequency {none,segment}

`--text-json` is the compact `{video_id, category, lines: [{text, text_sk}]}`
reference file (see `push_text_refs.py`). This script loads the alignment
model itself on every invocation (self-contained CLI, matching every other
backend's `--wav --out` convention in this harness) — for the real 22-fixture
x2-config batch, `batch_run.py` in this same directory imports
`load_model`/`align_one_song` from this file directly so the model is loaded
ONCE and reused across all fixtures in a config, instead of reloading it 44
times.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import time
import traceback
from pathlib import Path
from typing import Any

logger = logging.getLogger("lyrics_eval.ctc_forced_aligner")

MODEL_ID = "MahmoudAshraf/mms-300m-1130-forced-aligner"
LANGUAGE = (
    "eng"  # ISO 639-3 — see module docstring for why one code covers all 22 fixtures
)
SAMPLING_FREQ = 16000  # must match ctc_forced_aligner.alignment_utils.SAMPLING_FREQ

# CLI defaults from the real ctc_forced_aligner.align:cli() argparse, reused
# here verbatim rather than inventing our own tuning.
WINDOW_LENGTH_S = 30
CONTEXT_LENGTH_S = 2
BATCH_SIZE = 4
MERGE_THRESHOLD = 0.0

# ctc_forced_aligner.alignment_utils.load_audio shells out to a bare
# "ffmpeg" (relies on PATH — no way to pass a full path via its API). This
# project's own ffmpeg lives outside PATH by default, so we add its
# directory once per process. Override with FFMPEG_DIR if the box's layout
# ever changes.
DEFAULT_FFMPEG_DIR = r"C:\ProgramData\SongPlayer\cache\tools"


def ensure_ffmpeg_on_path() -> None:
    import shutil

    if shutil.which("ffmpeg"):
        return
    ffmpeg_dir = os.environ.get("FFMPEG_DIR", DEFAULT_FFMPEG_DIR)
    if os.path.exists(os.path.join(ffmpeg_dir, "ffmpeg.exe")) or os.path.exists(
        os.path.join(ffmpeg_dir, "ffmpeg")
    ):
        os.environ["PATH"] = ffmpeg_dir + os.pathsep + os.environ.get("PATH", "")
        logger.info("ffmpeg not on PATH — prepended %s", ffmpeg_dir)
    else:
        logger.warning(
            "ffmpeg not on PATH and not found at %s — load_audio will likely fail",
            ffmpeg_dir,
        )


def backend_id_for(star_frequency: str) -> str:
    """star_frequency is this backend's OWN vocabulary: "edges" or "segment"
    (already mapped from the CLI's --star-frequency {none,segment})."""
    return (
        "ctc-forced-aligner-star"
        if star_frequency == "segment"
        else "ctc-forced-aligner"
    )


def cli_star_frequency_to_aligner(value: str) -> str:
    """--star-frequency none|segment -> the real tool's star_frequency
    "edges"|"segment". "none" maps to the CLI's own default ("edges"), i.e.
    this backend's baseline run is "without --star_frequency segment"."""
    return "segment" if value == "segment" else "edges"


def load_model(device: str):
    """Load the alignment model+tokenizer once. Returns (model, tokenizer)."""
    import torch
    from ctc_forced_aligner import load_alignment_model

    dtype = torch.float16 if device == "cuda" else torch.float32
    logger.info(
        "loading alignment model: id=%s device=%s dtype=%s", MODEL_ID, device, dtype
    )
    model, tokenizer = load_alignment_model(device, MODEL_ID, None, dtype)
    return model, tokenizer


MODEL_LOAD_RETRIES = 10
MODEL_LOAD_RETRY_WAIT_S = 30


def load_model_with_retry(device: str):
    """win-resolume's single 8GB GPU is SHARED with a sibling aligner
    shootout run (LyricsAlignment-MTL) — observed live: its VRAM use swings
    from ~200MB free to ~7GB free as it loads/frees its own model between
    fixtures. A CUDA OOM here would otherwise kill the whole caller before a
    single output file is written (model load happens once, outside any
    per-song error handling). Retry with a fixed backoff instead of failing
    hard — the sibling's memory usage is transient, not a permanent
    conflict."""
    last_exc: Exception | None = None
    for attempt in range(1, MODEL_LOAD_RETRIES + 1):
        try:
            return load_model(device)
        except Exception as exc:  # noqa: BLE001 — covers torch.cuda.OutOfMemoryError (RuntimeError subclass)
            last_exc = exc
            logger.warning(
                "model load attempt %d/%d failed (%s: %s) — likely GPU VRAM contention with a sibling "
                "process; retrying in %ds",
                attempt,
                MODEL_LOAD_RETRIES,
                type(exc).__name__,
                exc,
                MODEL_LOAD_RETRY_WAIT_S,
            )
            try:
                import torch

                if torch.cuda.is_available():
                    torch.cuda.empty_cache()
            except Exception:
                logger.exception(
                    "torch.cuda.empty_cache() itself failed — continuing to retry anyway"
                )
            time.sleep(MODEL_LOAD_RETRY_WAIT_S)
    raise RuntimeError(
        f"model load failed after {MODEL_LOAD_RETRIES} attempts"
    ) from last_exc


def align_one_song(
    *,
    model,
    tokenizer,
    wav_path: Path,
    ref_lines: list[dict[str, Any]],
    star_frequency: str,
) -> tuple[list[dict[str, Any]], int, float]:
    """Run forced alignment for ONE song against its FIXED reference lines.

    Returns (lines_out, duration_ms, runtime_sec). Raises on a hard aligner
    failure (e.g. the target text is too long for the audio) — the caller
    decides the untimed-fallback behavior; this function never fabricates
    timestamps.
    """
    from ctc_forced_aligner import (
        generate_emissions,
        get_alignments,
        get_spans,
        load_audio,
        postprocess_results,
        preprocess_text,
    )

    t0 = time.time()

    line_texts = [(line.get("text") or "") for line in ref_lines]
    full_text = " ".join(t for t in line_texts if t)
    # Word counts per reference line, using the aligner's OWN tokenization
    # for split_size="word" (ctc_forced_aligner.text_utils.split_text just
    # does text.split()) — this is what lets us slice the flat per-word
    # result list back into per-line chunks with no separate re-alignment.
    line_word_counts = [len(t.split()) for t in line_texts]

    audio_waveform = load_audio(str(wav_path), model.dtype, model.device)
    duration_ms = int(round(audio_waveform.shape[0] / SAMPLING_FREQ * 1000))

    emissions, stride = generate_emissions(
        model, audio_waveform, WINDOW_LENGTH_S, CONTEXT_LENGTH_S, BATCH_SIZE
    )
    tokens_starred, text_starred = preprocess_text(
        full_text,
        romanize=False,
        language=LANGUAGE,
        split_size="word",
        star_frequency=star_frequency,
    )
    segments, scores, blank_token = get_alignments(emissions, tokens_starred, tokenizer)
    spans = get_spans(tokens_starred, segments, blank_token)
    word_results = postprocess_results(
        text_starred, spans, stride, scores, MERGE_THRESHOLD
    )

    runtime_sec = time.time() - t0

    lines_out: list[dict[str, Any]] = []
    idx = 0
    for line, n_words in zip(ref_lines, line_word_counts):
        text_sk = line.get("text_sk")
        if n_words == 0:
            lines_out.append(
                {
                    "text": line.get("text"),
                    "start_ms": None,
                    "end_ms": None,
                    "text_sk": text_sk,
                    "words": None,
                }
            )
            continue
        chunk = word_results[idx : idx + n_words]
        idx += n_words
        if not chunk:
            lines_out.append(
                {
                    "text": line.get("text"),
                    "start_ms": None,
                    "end_ms": None,
                    "text_sk": text_sk,
                    "words": None,
                }
            )
            continue
        words = [
            {
                "text": w["text"],
                "start_ms": int(round(w["start"] * 1000)),
                "end_ms": int(round(w["end"] * 1000)),
            }
            for w in chunk
        ]
        lines_out.append(
            {
                "text": line.get("text"),
                "start_ms": words[0]["start_ms"],
                "end_ms": words[-1]["end_ms"],
                "text_sk": text_sk,
                "words": words,
            }
        )

    if idx != len(word_results):
        logger.warning(
            "word count mismatch for wav=%s: consumed=%d produced=%d — "
            "reference/aligner word counts disagree, later lines may be misaligned",
            wav_path,
            idx,
            len(word_results),
        )

    return lines_out, duration_ms, runtime_sec


def all_untimed_lines(ref_lines: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "text": line.get("text"),
            "start_ms": None,
            "end_ms": None,
            "text_sk": line.get("text_sk"),
            "words": None,
        }
        for line in ref_lines
    ]


def build_payload(
    *,
    star_frequency: str,
    wav_path: Path,
    lines_out: list[dict[str, Any]],
    duration_ms: int,
    runtime_sec: float | None,
    model_load_sec: float | None,
    error: str | None,
) -> dict[str, Any]:
    return {
        "backend_id": backend_id_for(star_frequency),
        "backend_revision": 1,
        "wav_path": str(wav_path),
        "duration_ms": duration_ms,
        "lines": lines_out,
        "raw_confidence": None,
        "metadata": {
            "aligner": "ctc-forced-aligner",
            "star_frequency": star_frequency,
            "model_id": MODEL_ID,
            "runtime_sec": round(runtime_sec, 2) if runtime_sec is not None else None,
            "model_load_sec": round(model_load_sec, 2)
            if model_load_sec is not None
            else None,
            "error": error,
        },
    }


def wav_duration_ms_fallback(wav_path: Path) -> int:
    """Only used if a hard failure happens before/without a usable
    duration_ms from align_one_song. Deliberately reuses
    ctc_forced_aligner's OWN load_audio() (ffmpeg-based) rather than
    stdlib `wave` — this project's isolated-vocal WAVs are 32-bit float PCM
    (fmt tag 3), which Python's `wave` module cannot parse
    ("unknown format: 3"); ffmpeg decodes any PCM subtype fine."""
    import torch
    from ctc_forced_aligner import load_audio

    ensure_ffmpeg_on_path()
    waveform = load_audio(str(wav_path), torch.float32, "cpu")
    return int(round(waveform.shape[0] / SAMPLING_FREQ * 1000))


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
    )

    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument("--text-json", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--star-frequency", choices=["none", "segment"], default="none")
    p.add_argument(
        "--device", default=None, help="cuda or cpu; default: cuda if available"
    )
    args = p.parse_args(argv)

    import torch

    ensure_ffmpeg_on_path()
    device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")
    star_frequency = cli_star_frequency_to_aligner(args.star_frequency)

    ref = json.loads(args.text_json.read_text(encoding="utf-8"))
    ref_lines = ref.get("lines") or []
    logger.info(
        "run.py starting: wav=%s text_json=%s star_frequency=%s (cli=%s) n_lines=%d device=%s",
        args.wav,
        args.text_json,
        star_frequency,
        args.star_frequency,
        len(ref_lines),
        device,
    )

    t_load0 = time.time()
    model, tokenizer = load_model_with_retry(device)
    model_load_sec = time.time() - t_load0

    try:
        lines_out, duration_ms, runtime_sec = align_one_song(
            model=model,
            tokenizer=tokenizer,
            wav_path=args.wav,
            ref_lines=ref_lines,
            star_frequency=star_frequency,
        )
        error = None
    except Exception as exc:  # noqa: BLE001 — deliberately broad: any aligner
        # failure degrades to an honest all-untimed output rather than
        # crashing the batch or fabricating timestamps.
        logger.exception(
            "alignment failed for wav=%s — emitting all-untimed output", args.wav
        )
        lines_out = all_untimed_lines(ref_lines)
        runtime_sec = None
        error = f"{type(exc).__name__}: {exc}\n{traceback.format_exc()}"
        try:
            duration_ms = wav_duration_ms_fallback(args.wav)
        except Exception:
            logger.exception("could not even read WAV duration as a fallback")
            duration_ms = 0

    payload = build_payload(
        star_frequency=star_frequency,
        wav_path=args.wav,
        lines_out=lines_out,
        duration_ms=duration_ms,
        runtime_sec=runtime_sec,
        model_load_sec=model_load_sec,
        error=error,
    )

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    logger.info(
        "run.py done: out=%s n_lines=%d error=%s",
        args.out,
        len(lines_out),
        "yes" if error else "no",
    )
    return 0 if error is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
