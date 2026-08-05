#!/usr/bin/env python3
"""run.py — LyricsAlignment-MTL (Huang/Benetos/Ewert, ICASSP 2022), MTL+BDR
variant, per-fixture forced-alignment runner.

This is the CANONICAL copy of the script that actually runs the alignment.
The real computation happens on the win-resolume Windows box (GPU, its own
isolated venv, the vendored upstream repo) — this exact file is pushed there
unmodified via the win-resolume MCP FileWrite tool and executed with
`python.exe run.py --wav ... --text-json ... --out ...`. It is committed
here so a reader does not have to SSH anywhere to see what ran. See
`README.md` in this directory for the full install story, every trap hit,
and how the MTL+BDR checkpoint choice was confirmed.

Upstream repo:  https://github.com/jhuang448/LyricsAlignment-MTL  (MIT)
Paper:          Huang, Benetos, Ewert, "Improving Lyrics Alignment through
                 Joint Pitch Detection," ICASSP 2022.

What this does, end to end:
  1. Reads a COMPACT reference-text JSON for one fixture
     (`{"video_id": ..., "category": ..., "lines": [{"text": ..., "text_sk": ...}, ...]}`)
     — the FIXED transcript an audio-LLM already produced. We never alter
     `text`/`text_sk`; only their TIMING is what this aligner contributes.
  2. Writes that `lines[].text` out as a plain "one raw line per line" .txt
     file — the exact input shape `wrapper.preprocess_from_file()` expects
     (`lyrics_file`, no `word_file` — see README "Input format" section).
  3. Calls the upstream `wrapper.preprocess_from_file()` + `wrapper.align()`
     (method="MTL_BDR") UNMODIFIED — the acoustic model (MTL) + boundary
     model (BDR) load from `./checkpoints/checkpoint_MTL` +
     `./checkpoints/checkpoint_BDR` exactly as `eval_bdr.py`/the README's
     own MTL+BDR inference example does.
  4. The upstream code returns WORD-level alignment
     (`word_align`: one [start_frame, end_frame] pair per word, in the
     SAME order as its own internally-lowercased/punctuation-stripped
     `words` list). We regroup those word timings back onto OUR original
     lines using `build_line_word_map()` below, which independently
     replicates the exact same character filter the upstream
     `wrapper.preprocess_lyrics()` applies (see README "Word-to-line
     remapping" for why this is safe and how it's verified at runtime via
     an equality assertion against the upstream library's own output).
  5. Emits this project's standard per-fixture backend JSON shape (see
     `eval/lyrics/backends/soniox_v5.py` for the established convention).

Three DOCUMENTED, MINOR compatibility/performance additions on top of the
untouched upstream algorithm (all explained at length in README.md):
  - `numpy.Inf` (used by `utils.alignment_bdr()`'s DP table init) is
    restored as an alias for `numpy.inf` — numpy>=2.0 removed the
    capitalized alias this 2022-era code relies on. Exactly the "renamed
    numpy alias" shim example this project's task brief anticipated.
  - `torch.load` is wrapped to pass `weights_only=False` — torch>=2.6
    flipped that default, and the vendored checkpoints predate the
    weights-only pickle allowlist.
  - `utils.g2p` (the g2p_en grapheme-to-phoneme call) is memoized. This is
    a pure cache of a deterministic function — it changes wall-clock time
    only, never the alignment output — and is what makes the "poisoned"
    395-line fixture (which repeats a handful of phrases hundreds of
    times) tractable; see README "The O(n^2) g2p trap".

Usage (on the Windows box, inside the mtl_aligner_venv):
    python.exe run.py --wav C:\\...\\<video_id>_vocal16k.wav \\
                       --text-json C:\\...\\text-refs\\<video_id>.json \\
                       --out C:\\...\\out\\aligners\\lyrics-alignment-mtl_<video_id>.json \\
                       --repo-dir C:\\ProgramData\\SongPlayer\\cache\\tools\\LyricsAlignment-MTL
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

logger = logging.getLogger("lyrics_eval.lyrics_alignment_mtl")

BACKEND_ID = "lyrics-alignment-mtl"
BACKEND_REVISION = 1
METHOD = "MTL_BDR"  # wrapper.align()'s method string for the MTL+BDR variant
CHECKPOINT_DESC = (
    "checkpoint_MTL (acoustic model, MTL variant) + checkpoint_BDR "
    "(boundary-detection refinement) — the 'MTL+BDR' configuration named in "
    "the upstream README's inference example (eval_bdr.py), loaded via "
    "wrapper.align(method='MTL_BDR')"
)

# The exact character set wrapper.py's preprocess_lyrics() keeps when
# lowercasing+filtering raw lyric lines (a-z, apostrophe, space — everything
# else, including digits and punctuation other than ', is stripped). This is
# copied VERBATIM from LyricsAlignment-MTL/wrapper.py::preprocess_lyrics —
# not reinterpreted — so our independent replication in
# build_line_word_map() below produces byte-identical output to the
# upstream function; see README "Word-to-line remapping".
FILTER_CHARS = set("abcdefghijklmnopqrstuvwxyz' ")

# seconds per model output frame — copied VERBATIM from wrapper.py / model.py
# (256 = maxpool-reduced CNN stride product, 22050 = the model's fixed
# training sample rate, 3 = the extra maxpool-along-time factor).
RESOLUTION_SEC_PER_FRAME = 256 / 22050 * 3


def filter_word(word: str) -> str:
    """Same per-character filter as upstream preprocess_lyrics(), applied to
    a single already-whitespace-split word instead of a whole line. Filtering
    a word in isolation vs. filtering the enclosing line then re-splitting on
    whitespace produces IDENTICAL results, because the filter only ever
    REMOVES characters — it never merges two words separated by real
    whitespace (whitespace itself is a kept character) and never invents a
    new internal space. See README for the full argument + the runtime
    assertion that confirms it empirically for every fixture."""
    return "".join(c for c in word.lower() if c in FILTER_CHARS)


def build_line_word_map(
    lines_text: list[str],
) -> tuple[list[str], list[tuple[int, str]], list[int]]:
    """Independently replicates the word list that
    LyricsAlignment-MTL/wrapper.py::preprocess_lyrics() derives when called
    WITHOUT a word_file (our usage — see README): each original line is
    whitespace-split, each word is filtered via filter_word(), and any word
    that filters down to the empty string (observed exactly once across the
    22 fixtures: a bare "20") is silently dropped — matching upstream's own
    behaviour of filtering a whole line then collapsing whitespace.

    Returns:
      filtered_words: the words list, in order — MUST equal (asserted at
        call time in main()) the upstream `words` return value.
      origin: parallel list of (original_line_idx, original_word_text) for
        each surviving entry in filtered_words.
      line_word_counts: one entry per input line — how many of its words
        survived filtering (0 for a line that is fully untimeable).
    """
    filtered_words: list[str] = []
    origin: list[tuple[int, str]] = []
    line_word_counts: list[int] = []
    for line_idx, text in enumerate(lines_text):
        count = 0
        for w in (text or "").split():
            fw = filter_word(w)
            if fw:
                filtered_words.append(fw)
                origin.append((line_idx, w))
                count += 1
        line_word_counts.append(count)
    return filtered_words, origin, line_word_counts


def get_wav_duration_ms(wav_path: Path) -> int:
    """Real WAV duration from the file header — never guessed from line
    timestamps, per this eval harness's convention. Uses `soundfile`
    (already a hard dependency of the upstream repo itself) rather than the
    stdlib `wave` module: this project's fixture WAVs are IEEE-float PCM
    (format tag 3, from the dereverb/vocal-isolation pipeline), which
    `wave.Error: unknown format: 3` rejects outright — `wave` only
    understands integer PCM."""
    import soundfile as sf

    info = sf.info(str(wav_path))
    return int(round(info.frames / info.samplerate * 1000))


def install_compat_shims(repo_dir: str):
    """Import the upstream modules with the two documented, minor
    compatibility/performance additions applied. Returns the `wrapper`
    module. See the module docstring + README for why each shim exists —
    neither touches the alignment algorithm itself."""
    if repo_dir not in sys.path:
        sys.path.insert(0, repo_dir)
    # wrapper.align() loads checkpoints via relative paths
    # ("./checkpoints/checkpoint_...") — see wrapper.py::align().
    os.chdir(repo_dir)

    import numpy as np

    if not hasattr(np, "Inf"):
        np.Inf = np.inf  # type: ignore[attr-defined]
        logger.info("compat shim installed: numpy.Inf restored (numpy>=2.0 removed it)")

    import torch

    _orig_torch_load = torch.load

    def _patched_torch_load(*args: Any, **kwargs: Any):
        kwargs.setdefault("weights_only", False)
        return _orig_torch_load(*args, **kwargs)

    torch.load = _patched_torch_load  # type: ignore[assignment]
    logger.info("compat shim installed: torch.load(weights_only=False)")

    import utils as mtl_utils  # upstream module, NOT this project's eval/lyrics utils

    _g2p_cache: dict[str, Any] = {}
    _orig_g2p = mtl_utils.g2p

    def _cached_g2p(text: str):
        if text not in _g2p_cache:
            _g2p_cache[text] = _orig_g2p(text)
        return _g2p_cache[text]

    mtl_utils.g2p = _cached_g2p  # type: ignore[assignment]
    logger.info("perf shim installed: utils.g2p memoized (deterministic fn)")

    import wrapper  # noqa: F401  (upstream module)

    return wrapper


def align_fixture(
    *,
    wav_path: Path,
    lines_text: list[str],
    lines_text_sk: list[str | None],
    repo_dir: str,
    cuda: bool,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Runs the upstream MTL+BDR aligner on one fixture and regroups its
    word-level output back onto our original lines. Returns (lines, timing)
    where `timing` carries preprocess_sec/align_sec for metadata.runtime_sec."""
    wrapper = install_compat_shims(repo_dir)
    import torch  # already loaded by install_compat_shims; re-bind the name locally

    filtered_words, origin, line_word_counts = build_line_word_map(lines_text)

    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".raw.txt", delete=False, encoding="utf-8"
    ) as fh:
        for text in lines_text:
            fh.write((text or "") + "\n")
        lyrics_txt_path = fh.name

    try:
        t0 = time.time()
        audio, words, lyrics_p, idx_word_p, idx_line_p = wrapper.preprocess_from_file(
            str(wav_path), lyrics_txt_path, word_file=None
        )
        preprocess_sec = time.time() - t0
        logger.info(
            "preprocess done: n_words=%d preprocess_sec=%.2f", len(words), preprocess_sec
        )

        if words != filtered_words:
            raise RuntimeError(
                "word-filtering mismatch: our build_line_word_map() replication "
                f"diverged from upstream wrapper.preprocess_lyrics() output "
                f"(ours={len(filtered_words)} words, upstream={len(words)} words) "
                "— see README 'Word-to-line remapping' before trusting this output"
            )

        t1 = time.time()
        device_used = "cpu"
        cuda_oom_retried = False
        try:
            if cuda:
                word_align, words_out = wrapper.align(
                    audio, words, lyrics_p, idx_word_p, idx_line_p, method=METHOD, cuda=True
                )
                device_used = "cuda"
            else:
                word_align, words_out = wrapper.align(
                    audio, words, lyrics_p, idx_word_p, idx_line_p, method=METHOD, cuda=False
                )
        except torch.cuda.OutOfMemoryError:
            # This box's GPU is SHARED with a sibling agent's concurrent
            # inference workload (task brief: "a sibling agent is ALSO
            # doing GPU work on this same box in parallel"). A larger
            # fixture's mel-spectrogram + CNN/LSTM activations can push
            # combined usage over the 8GB card during a contention window.
            # Rather than fail the whole fixture (or worse, sit blocking
            # the batch waiting for the sibling to free memory — the task
            # brief explicitly says "don't wait around for it"), fall back
            # to CPU for THIS fixture only: identical algorithm, identical
            # checkpoint, just a different execution device. Logged and
            # surfaced in metadata.device / metadata.cuda_oom_retried so
            # this is never silently hidden.
            logger.warning(
                "CUDA OOM on this fixture (likely shared-GPU contention "
                "with a sibling process) — retrying on CPU",
                exc_info=True,
            )
            torch.cuda.empty_cache()
            word_align, words_out = wrapper.align(
                audio, words, lyrics_p, idx_word_p, idx_line_p, method=METHOD, cuda=False
            )
            device_used = "cpu"
            cuda_oom_retried = True
        align_sec = time.time() - t1
        logger.info(
            "alignment done: align_sec=%.2f device=%s cuda_oom_retried=%s",
            align_sec,
            device_used,
            cuda_oom_retried,
        )
    finally:
        try:
            os.unlink(lyrics_txt_path)
        except OSError:
            logger.warning(
                "failed to remove temp lyrics file %s (non-fatal)",
                lyrics_txt_path,
                exc_info=True,
            )

    if len(word_align) != len(filtered_words):
        raise RuntimeError(
            f"word_align length ({len(word_align)}) != filtered word count "
            f"({len(filtered_words)}) — upstream returned a different number "
            "of aligned words than words submitted"
        )

    word_ms: list[tuple[int, int]] = [
        (
            int(round(st_frame * RESOLUTION_SEC_PER_FRAME * 1000)),
            int(round(ed_frame * RESOLUTION_SEC_PER_FRAME * 1000)),
        )
        for st_frame, ed_frame in word_align
    ]

    out_lines: list[dict[str, Any]] = []
    cursor = 0
    for line_idx, text in enumerate(lines_text):
        n = line_word_counts[line_idx]
        text_sk = lines_text_sk[line_idx] if line_idx < len(lines_text_sk) else None
        if n == 0:
            # The aligner has nothing to time this line with at all — every
            # word in it filtered to empty (never observed in the 22
            # fixtures, but handled honestly rather than assumed away).
            out_lines.append(
                {
                    "text": text,
                    "start_ms": None,
                    "end_ms": None,
                    "text_sk": text_sk,
                    "words": None,
                }
            )
            continue

        orig_words_in_line = [w for (_li, w) in origin[cursor : cursor + n]]
        timings = word_ms[cursor : cursor + n]
        cursor += n

        words_field = [
            {"text": ow, "start_ms": st, "end_ms": ed}
            for ow, (st, ed) in zip(orig_words_in_line, timings)
        ]
        out_lines.append(
            {
                "text": text,
                "start_ms": timings[0][0],
                "end_ms": timings[-1][1],
                "text_sk": text_sk,
                "words": words_field,
            }
        )

    timing = {
        "preprocess_sec": round(preprocess_sec, 3),
        "align_sec": round(align_sec, 3),
        "device": device_used,
        "cuda_oom_retried": cuda_oom_retried,
    }
    return out_lines, timing


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(
        level=os.environ.get("LYRICS_EVAL_LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wav", type=Path, required=True)
    p.add_argument(
        "--text-json",
        type=Path,
        required=True,
        help="compact reference JSON: {video_id, category, lines: [{text, text_sk}]}",
    )
    p.add_argument("--out", type=Path, required=True)
    p.add_argument(
        "--repo-dir",
        type=str,
        default=r"C:\ProgramData\SongPlayer\cache\tools\LyricsAlignment-MTL",
        help="path to the cloned jhuang448/LyricsAlignment-MTL repo (Windows box only)",
    )
    p.add_argument(
        "--no-cuda",
        action="store_true",
        help="force CPU even if a CUDA device is available",
    )
    args = p.parse_args(argv)

    ref = json.loads(args.text_json.read_text(encoding="utf-8"))
    ref_lines = ref.get("lines") or []
    lines_text = [line.get("text") or "" for line in ref_lines]
    lines_text_sk = [line.get("text_sk") for line in ref_lines]

    logger.info(
        "lyrics-alignment-mtl starting: video_id=%s n_lines=%d wav=%s",
        ref.get("video_id"),
        len(lines_text),
        args.wav,
    )

    out_lines, timing = align_fixture(
        wav_path=args.wav,
        lines_text=lines_text,
        lines_text_sk=lines_text_sk,
        repo_dir=args.repo_dir,
        cuda=not args.no_cuda,
    )

    duration_ms = get_wav_duration_ms(args.wav)
    n_untimed = sum(1 for line in out_lines if line["start_ms"] is None)

    payload = {
        "backend_id": BACKEND_ID,
        "backend_revision": BACKEND_REVISION,
        "wav_path": str(args.wav),
        "duration_ms": duration_ms,
        "lines": out_lines,
        "raw_confidence": None,
        "metadata": {
            "aligner": "lyrics-alignment-mtl",
            "checkpoint": CHECKPOINT_DESC,
            "granularity": "word",
            "runtime_sec": timing["align_sec"],
            "preprocess_sec": timing["preprocess_sec"],
            "method": METHOD,
            "device": timing["device"],
            "cuda_oom_retried": timing["cuda_oom_retried"],
        },
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")

    logger.info(
        "lyrics-alignment-mtl done: video_id=%s n_lines=%d n_untimed=%d "
        "align_sec=%.2f out=%s",
        ref.get("video_id"),
        len(out_lines),
        n_untimed,
        timing["align_sec"],
        args.out,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
