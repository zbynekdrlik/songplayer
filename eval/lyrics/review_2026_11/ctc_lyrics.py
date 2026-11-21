#!/usr/bin/env python3
"""ctc_lyrics.py — open-weight lyrics CTC models on the fixture set (#349).

Two 2026 open-weight lyrics CTC models (both ``Wav2Vec2BertForCTC``, 16 kHz,
20 ms frames, the same 921-token character vocabulary):

  - ``w2v-bert-lyrics`` = mrfakename/w2v-bert-2.0-music-lyrics-ctc (MIT,
    2026-09-23), trained on full mixes;
  - ``sylla`` = radiant-lyrics/sylla-600m-v1 (GPL-3.0, 2026-10-16), its
    fine-tune for lead-vocal syllable timing.

Two modes, one inference pass (logits) per song:

  - ``align``: forced alignment of the SAME reference text the 2026-08-05
    aligner shootout gave every aligner (the ``lines[].text`` carried by
    ``reports/2026-08-05-aligner-raw/lyrics-alignment-mtl_<id>.json``),
    CTC Viterbi via ``torchaudio.functional.forced_align``. Line start = the
    first aligned word's start, end = the last word's end; a line with no
    alignable character is UNTIMED (``start_ms`` null), never guessed. Sylla's
    own "Lockstep" aligner is not published, so this is the plain Viterbi
    the base model's card documents.
  - ``asr``: the no-text tier — greedy CTC decoding into words with their
    frame times, grouped into lines at a 400 ms silence gap (the same
    grouping the 2026-09-12 g35t adapter and the AAI/Soniox adapters used).

Long audio follows the model card ("trained on windows of up to 30 s"):
each read is at most 30 s — a 26 s centre kept plus 2 s of context on each
side — and the kept logits are concatenated.

Output: ``<out-dir>/<backend_id>_<video_id>.json`` in the aligner-raw shape
``score_aligner.py`` reads. A fixture with no input writes an error row.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
import unicodedata
from pathlib import Path
from typing import Any

MODELS = {
    "w2v-bert-lyrics": "mrfakename/w2v-bert-2.0-music-lyrics-ctc",
    "sylla": "radiant-lyrics/sylla-600m-v1",
}
FRAME_MS = 20
READ_S = 30.0  # the longest window the model was trained on
CONTEXT_S = 2.0
WINDOW_S = READ_S - 2 * CONTEXT_S  # the kept centre of each read
BLANK_ID = 0
SR = 16000
LINE_GAP_MS = 400
REPO = Path(__file__).resolve().parents[3]
REF_DIR = REPO / "eval/lyrics/reports/2026-08-05-aligner-raw"
MANIFEST = REPO / "eval/lyrics/manifest.json"
POISONED = "Xvm4_fWkXe8"


def normalize_word(word: str) -> str:
    """The model's own output alphabet: lowercase NFKC, punctuation removed,
    apostrophes kept inside a word (model card, 'Output format')."""
    w = unicodedata.normalize("NFKC", word).lower().replace("’", "'")
    w = re.sub(r"[^\w']", "", w)
    return w.strip("'").replace("_", "")


def window_plan(n_samples: int) -> list[tuple[int, int, int, int]]:
    """(read_start, read_end, keep_start, keep_end) sample bounds per window:
    keep spans tile [0, n) exactly; each read adds CONTEXT_S each side."""
    step = int(WINDOW_S * SR)
    ctx = int(CONTEXT_S * SR)
    plan = []
    keep_start = 0
    while keep_start < n_samples:
        keep_end = min(keep_start + step, n_samples)
        plan.append(
            (
                max(0, keep_start - ctx),
                min(n_samples, keep_end + ctx),
                keep_start,
                keep_end,
            )
        )
        keep_start = keep_end
    return plan


def frames_for(samples: int) -> int:
    return samples // (SR * FRAME_MS // 1000)


def kept_slice(
    n_frames: int, r0: int, k0: int, k1: int, n_samples: int
) -> tuple[int, int]:
    """The frame slice [a, b) of one window's logits that covers its kept
    samples [k0, k1). An interior window with fewer frames than that would
    shift every later frame by its shortfall, so it raises; only the last
    window may come up short (its tail is cut where the audio ends)."""
    a = frames_for(k0 - r0)
    want = a + frames_for(k1 - k0)
    if k1 < n_samples and n_frames < want:
        raise RuntimeError(f"window {k0}-{k1}: {n_frames} frames < {want}")
    return a, min(n_frames, want)


def logits_for(model, processor, audio, device) -> Any:
    import torch

    parts = []
    for r0, r1, k0, k1 in window_plan(len(audio)):
        inputs = processor(audio[r0:r1], sampling_rate=SR, return_tensors="pt")
        inputs = {k: v.to(device) for k, v in inputs.items()}
        with torch.inference_mode():
            lp = torch.log_softmax(model(**inputs).logits[0].float(), dim=-1).cpu()
        a, b = kept_slice(lp.shape[0], r0, k0, k1, len(audio))
        parts.append(lp[a:b])
    return torch.cat(parts, dim=0)


def build_targets(
    vocab: dict[str, int], ref_lines: list[str]
) -> tuple[list[int], list[tuple[int, int] | None], list[list[str]]]:
    """The CTC target sequence for the whole song: every reference word's
    characters, ``|`` between words. ``owner[i]`` is the (line, word) of
    target token i (None for a separator); words with no character the
    model knows are left out (their line may end up untimed)."""
    sep = vocab["|"]
    targets: list[int] = [sep]
    owner: list[tuple[int, int] | None] = [None]
    words_per_line: list[list[str]] = []
    for li, text in enumerate(ref_lines):
        kept = []
        for raw in text.split():
            w = normalize_word(raw)
            ids = [vocab[c] for c in w if c in vocab]
            if not ids:
                continue
            wi = len(kept)
            kept.append(raw)
            for t in ids:
                targets.append(t)
                owner.append((li, wi))
            targets.append(sep)
            owner.append(None)
        words_per_line.append(kept)
    return targets, owner, words_per_line


def word_spans(
    path: list[int], owner: list[tuple[int, int] | None]
) -> dict[tuple[int, int], list[int]]:
    """Walk a CTC forced-alignment frame path: a new target token starts at
    every non-blank frame whose token differs from the previous frame's, or
    follows a blank (a doubled letter always has a blank between its two
    copies). Returns (line, word) -> [first frame, last frame]."""
    spans: dict[tuple[int, int], list[int]] = {}
    ti = -1
    prev = BLANK_ID
    for frame, tok in enumerate(path):
        if tok != BLANK_ID and (tok != prev or prev == BLANK_ID):
            ti += 1
        if tok != BLANK_ID and owner[ti] is not None:
            span = spans.setdefault(owner[ti], [frame, frame])
            span[1] = frame
        prev = tok
    return spans


def lines_from_spans(
    ref_lines: list[str],
    words_per_line: list[list[str]],
    spans: dict[tuple[int, int], list[int]],
) -> list[dict]:
    """Produced lines in reference order; a line with no aligned word is
    UNTIMED (null start/end), never guessed."""
    lines = []
    for li, text in enumerate(ref_lines):
        words = []
        for wi, raw in enumerate(words_per_line[li]):
            span = spans.get((li, wi))
            if span is None:
                continue
            words.append(
                {
                    "text": raw,
                    "start_ms": span[0] * FRAME_MS,
                    "end_ms": (span[1] + 1) * FRAME_MS,
                }
            )
        if words:
            lines.append(
                {
                    "text": text,
                    "start_ms": words[0]["start_ms"],
                    "end_ms": words[-1]["end_ms"],
                    "words": words,
                }
            )
        else:
            lines.append(
                {"text": text, "start_ms": None, "end_ms": None, "words": None}
            )
    return lines


def align_lines(lp, vocab: dict[str, int], ref_lines: list[str]) -> list[dict]:
    """CTC Viterbi of every reference word, in song order, against the whole
    song's log-probs. Returns produced lines (untimed lines kept, null start)."""
    import torch
    import torchaudio.functional as taf

    targets, owner, words_per_line = build_targets(vocab, ref_lines)
    spans: dict[tuple[int, int], list[int]] = {}
    if len(targets) > 1:
        path, _ = taf.forced_align(
            lp.unsqueeze(0),
            torch.tensor([targets], dtype=torch.int32),
            blank=BLANK_ID,
        )
        spans = word_spans(path[0].tolist(), owner)
    return lines_from_spans(ref_lines, words_per_line, spans)


def greedy_words(best: list[int], id_to_tok: dict[int, str]) -> list[dict]:
    """Greedy CTC decode of the per-frame best token ids into words with
    frame times (``|`` = word separator). A word's end moves only with a
    letter actually appended, never with a ``<…>`` special token."""
    words: list[dict] = []
    cur, start, end, prev = "", None, None, BLANK_ID
    for frame, tok in enumerate(best):
        if tok != BLANK_ID:
            ch = id_to_tok.get(tok, "")
            if ch == "|":
                if tok != prev and cur:
                    words.append({"text": cur, "start_ms": start, "end_ms": end})
                    cur, start = "", None
            elif ch and not ch.startswith("<"):
                if tok != prev:
                    if start is None:
                        start = frame * FRAME_MS
                    cur += ch
                end = (frame + 1) * FRAME_MS
        prev = tok
    if cur:
        words.append({"text": cur, "start_ms": start, "end_ms": end})
    return words


def group_lines(words: list[dict]) -> list[dict]:
    lines: list[dict] = []
    for w in words:
        if lines and w["start_ms"] - lines[-1]["end_ms"] < LINE_GAP_MS:
            line = lines[-1]
            line["words"].append(w)
            line["end_ms"] = w["end_ms"]
            line["text"] += " " + w["text"]
        else:
            lines.append(
                {
                    "text": w["text"],
                    "start_ms": w["start_ms"],
                    "end_ms": w["end_ms"],
                    "words": [w],
                }
            )
    return lines


def reference_lines(video_id: str) -> list[str] | None:
    path = REF_DIR / f"lyrics-alignment-mtl_{video_id}.json"
    if not path.exists():
        return None
    return [line["text"] for line in json.loads(path.read_text())["lines"]]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", choices=sorted(MODELS), required=True)
    ap.add_argument("--wav-dir", type=Path, required=True)
    ap.add_argument("--input", choices=["vocal", "mix"], default="vocal")
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--only", nargs="*", default=[])
    args = ap.parse_args(argv)

    import soundfile as sf
    import torch
    from transformers import Wav2Vec2BertForCTC, Wav2Vec2BertProcessor

    device = "cuda" if torch.cuda.is_available() else "cpu"
    repo = MODELS[args.model]
    processor = Wav2Vec2BertProcessor.from_pretrained(repo)
    model = Wav2Vec2BertForCTC.from_pretrained(repo).eval().to(device)
    vocab = processor.tokenizer.get_vocab()
    if vocab.get("<blank>") != BLANK_ID:
        raise RuntimeError(f"{repo}: <blank> is {vocab.get('<blank>')}, not {BLANK_ID}")
    id_to_tok = {v: k for k, v in vocab.items()}
    suffix = "" if args.input == "vocal" else "-mix"
    align_id = f"{args.model}-align{suffix}"
    asr_id = f"{args.model}-asr{suffix}"
    args.out_dir.mkdir(parents=True, exist_ok=True)

    fixtures = json.loads(MANIFEST.read_text())["fixtures"]
    failed = []
    for fx in fixtures:
        vid = fx["video_id"]
        if vid == POISONED or (args.only and vid not in args.only):
            continue
        wav = args.wav_dir / f"{vid}_{args.input}16k.wav"
        refs = reference_lines(vid)
        meta = {"model": repo, "input": args.input, "device": device}
        if not wav.exists():
            for bid in (align_id, asr_id):
                row = {"backend_id": bid, "error": f"no input {wav.name}", "lines": []}
                (args.out_dir / f"{bid}_{vid}.json").write_text(json.dumps(row))
            failed.append(vid)
            print(f"{vid}: FAILED no input", flush=True)
            continue
        audio, sr = sf.read(str(wav), dtype="float32")
        if sr != SR:
            raise RuntimeError(f"{wav} is {sr} Hz, expected {SR}")
        t0 = time.monotonic()
        lp = logits_for(model, processor, audio, device)
        infer_s = time.monotonic() - t0
        duration_ms = int(len(audio) * 1000 / SR)
        asr_lines = group_lines(greedy_words(lp.argmax(dim=-1).tolist(), id_to_tok))
        out = {
            "backend_id": asr_id,
            "wav_path": str(wav),
            "duration_ms": duration_ms,
            "lines": asr_lines,
            "metadata": {**meta, "runtime_sec": round(infer_s, 3)},
        }
        (args.out_dir / f"{asr_id}_{vid}.json").write_text(json.dumps(out))
        if refs is None:
            row = {"backend_id": align_id, "error": "no reference text", "lines": []}
            (args.out_dir / f"{align_id}_{vid}.json").write_text(json.dumps(row))
        else:
            t1 = time.monotonic()
            lines = align_lines(lp, vocab, refs)
            out = {
                "backend_id": align_id,
                "wav_path": str(wav),
                "duration_ms": duration_ms,
                "lines": lines,
                "metadata": {
                    **meta,
                    "runtime_sec": round(infer_s + time.monotonic() - t1, 3),
                },
            }
            (args.out_dir / f"{align_id}_{vid}.json").write_text(json.dumps(out))
        print(f"{vid}: asr {len(asr_lines)} lines, {infer_s:.1f}s", flush=True)
    print(f"done; failed={failed}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
