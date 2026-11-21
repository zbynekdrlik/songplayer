#!/usr/bin/env python3
"""heart_asr.py — HeartTranscriptor-oss on the fixture set (#349).

``HeartMuLa/HeartTranscriptor-oss`` (Apache-2.0, 2026-01-14) is a Whisper
fine-tune for lyrics, trained on SEPARATED vocals. heartlib runs it as a
plain transformers ``AutomaticSpeechRecognitionPipeline`` (30 s chunks,
batch 16, fp16; ``examples/run_lyrics_transcription.py``: 2 beams, 256 new
tokens, task ``transcribe``, language auto). Its card documents no
timestamps, but its ``generation_config.json`` ships Whisper
``alignment_heads``, so the pipeline's own ``return_timestamps="word"``
(cross-attention DTW) gives word times. That is the only way it can serve
as the no-text tier or as the gate's transcript, so that is what is
measured; nothing else is changed from heartlib's call.

Words are grouped into lines at a 400 ms silence gap, exactly as
``ctc_lyrics.py`` and the 2026-09-12 g35t adapter do.

Output: ``<out-dir>/heart-asr_<video_id>.json`` in the aligner-raw shape
``score_aligner.py`` reads; a fixture with no input writes an error row.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

from eval.lyrics.review_2026_11.ctc_lyrics import MANIFEST, POISONED, group_lines

REPO_ID = "HeartMuLa/HeartTranscriptor-oss"
BACKEND_ID = "heart-asr"
SR = 16000


def chunks_to_words(chunks: list[dict]) -> list[dict]:
    """Pipeline word chunks -> timed words. A chunk without a start or end
    (Whisper's DTW can leave the last word open) is dropped, never guessed."""
    words = []
    for c in chunks:
        text = (c.get("text") or "").strip()
        start, end = c.get("timestamp") or (None, None)
        if not text or start is None or end is None:
            continue
        words.append(
            {
                "text": text,
                "start_ms": int(round(start * 1000)),
                "end_ms": int(round(end * 1000)),
            }
        )
    words.sort(key=lambda w: w["start_ms"])
    return words


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wav-dir", type=Path, required=True)
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--only", nargs="*", default=[])
    # heartlib uses 16, but 16 and even 4 OOM an 8 GB card on their own
    # (word-timestamp DTW keeps every cross-attention); the batch only groups
    # independent 30 s chunks, so 1 changes memory, not the transcript.
    ap.add_argument("--batch-size", type=int, default=1)
    args = ap.parse_args(argv)

    import soundfile as sf
    import torch
    from transformers import (
        AutomaticSpeechRecognitionPipeline,
        WhisperForConditionalGeneration,
        WhisperProcessor,
    )

    device = "cuda" if torch.cuda.is_available() else "cpu"
    dtype = torch.float16 if device == "cuda" else torch.float32
    model = WhisperForConditionalGeneration.from_pretrained(REPO_ID, dtype=dtype)
    processor = WhisperProcessor.from_pretrained(REPO_ID)
    pipe = AutomaticSpeechRecognitionPipeline(
        model=model,
        tokenizer=processor.tokenizer,
        feature_extractor=processor.feature_extractor,
        device=device,
        dtype=dtype,
        chunk_length_s=30,
        batch_size=args.batch_size,
    )
    args.out_dir.mkdir(parents=True, exist_ok=True)
    failed = []
    for fx in json.loads(MANIFEST.read_text())["fixtures"]:
        vid = fx["video_id"]
        if vid == POISONED or (args.only and vid not in args.only):
            continue
        wav = args.wav_dir / f"{vid}_vocal16k.wav"
        out_path = args.out_dir / f"{BACKEND_ID}_{vid}.json"
        if not wav.exists():
            row = {
                "backend_id": BACKEND_ID,
                "error": f"no input {wav.name}",
                "lines": [],
            }
            out_path.write_text(json.dumps(row))
            print(f"{vid}: no input", flush=True)
            continue
        audio, sr = sf.read(str(wav), dtype="float32")
        if sr != SR:
            raise RuntimeError(f"{wav} is {sr} Hz, expected {SR}")
        t0 = time.monotonic()
        try:
            result = pipe(
                {"raw": audio, "sampling_rate": SR},
                return_timestamps="word",
                generate_kwargs={
                    "task": "transcribe",
                    "num_beams": 2,
                    "max_new_tokens": 256,
                },
            )
        except Exception as exc:  # an error row, never a guessed timing
            failed.append(vid)
            row = {"backend_id": BACKEND_ID, "error": repr(exc), "lines": []}
            out_path.write_text(json.dumps(row))
            print(f"{vid}: FAILED {exc!r}", flush=True)
            continue
        runtime = time.monotonic() - t0
        lines = group_lines(chunks_to_words(result.get("chunks") or []))
        out = {
            "backend_id": BACKEND_ID,
            "wav_path": str(wav),
            "duration_ms": int(len(audio) * 1000 / SR),
            "lines": lines,
            "metadata": {
                "model": REPO_ID,
                "device": device,
                "batch_size": args.batch_size,
                "runtime_sec": round(runtime, 3),
            },
        }
        out_path.write_text(json.dumps(out))
        print(f"{vid}: {len(lines)} lines, {runtime:.1f}s", flush=True)
    print(f"done; failed={failed}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
