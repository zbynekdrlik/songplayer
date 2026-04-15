#!/usr/bin/env python3
"""
lyrics_worker.py — narrow Python entry points for the lyrics pipeline.

Commands:
  preprocess-vocals  Mel-Roformer + anvuew dereverb + 16 kHz mono float32 WAV
  align-chunks       Chunked Qwen3-ForcedAligner alignment (loads model once,
                     loops over all chunks from a JSON request file)
  preload            Warm Mel-Roformer + anvuew + Qwen3-ForcedAligner at boot
  isolate-vocals     Diagnostic: Mel-Roformer only, 16 kHz mono float32 WAV
"""

import argparse
import gc
import json
import os
import shutil
import sys
import tempfile


MEL_ROFORMER_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"
DEREVERB_MODEL = "dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"


def _pick_vocal_stem(out_files, fallback_dir):
    """Return the absolute path of the Vocals stem among `out_files`."""
    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    vocal = [p for p in out_files if "Vocals" in p or "vocals" in p]
    if vocal:
        return _abs(vocal[0])
    non_inst = [
        p for p in out_files if "Instrumental" not in p and "instrumental" not in p
    ]
    if len(non_inst) == 1:
        return _abs(non_inst[0])
    raise RuntimeError(
        f"audio-separator did not produce an identifiable Vocals stem (got: {out_files})"
    )


def _pick_dereverbed_stem(out_files, fallback_dir):
    """Return the absolute path of the anvuew *(noreverb)* stem.

    Match on the parenthesized token *(noreverb)* in the filename — the
    substring 'dry' false-matched real filenames on earlier runs. If no
    explicit noreverb tag is present, fall back to the single file that
    does not contain '(reverb)'.
    """
    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    noreverb = [p for p in out_files if "(noreverb)" in p.lower()]
    if noreverb:
        return _abs(noreverb[0])
    non_reverb = [p for p in out_files if "(reverb)" not in p.lower()]
    if len(non_reverb) == 1:
        return _abs(non_reverb[0])
    raise RuntimeError(
        f"anvuew dereverb did not produce an identifiable (noreverb) stem (got: {out_files})"
    )


def _free_vram(sep):
    """Drop separator state so the next model can load without OOM."""
    import torch
    if hasattr(sep, "model_instance"):
        sep.model_instance = None
    del sep
    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()


def cmd_preprocess_vocals(args):
    """Mel-Roformer isolate → anvuew dereverb → 16 kHz mono float32 WAV.

    Writes a FLOAT WAV to --output. Exits 0 on success.
    """
    import numpy as np
    import librosa
    import soundfile as sf
    from audio_separator.separator import Separator

    stem_dir = tempfile.mkdtemp(prefix="sp_stems_")
    try:
        # Step 1: Mel-Roformer vocal isolation.
        sep = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep.load_model(MEL_ROFORMER_MODEL)
        out_files = sep.separate(args.audio)
        vocal_path = _pick_vocal_stem(out_files, stem_dir)
        _free_vram(sep)

        # Step 2: anvuew mel-band roformer dereverb on the isolated vocal.
        sep2 = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep2.load_model(DEREVERB_MODEL)
        out_files2 = sep2.separate(vocal_path)
        dry_path = _pick_dereverbed_stem(out_files2, stem_dir)
        _free_vram(sep2)

        # Step 3: resample to exactly 16 kHz mono float32, peak-clamp.
        audio, _ = librosa.load(dry_path, sr=16000, mono=True)
        peak = float(np.max(np.abs(audio))) if audio.size else 0.0
        if peak > 1.0:
            audio = audio / peak
        sf.write(args.output, audio, 16000, subtype="FLOAT")
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)

    print(json.dumps({"output": args.output}))


def cmd_align_chunks(args):
    """Chunked Qwen3-ForcedAligner: loads the model ONCE, loops over all chunks.

    --chunks is a path to JSON with shape:
      {"chunks": [{"chunk_idx": 0, "word_offset": 0,
                   "start_ms": 500, "end_ms": 3500,
                   "text": "hey there friend", "word_count": 3}, ...]}

    The `word_offset` field is metadata — Python ignores it and only the
    Rust assembly phase uses it to slot sub-chunk output back into the
    right position within a split-line's full word sequence.

    Writes JSON to --output with shape:
      {"chunks": [{"chunk_idx": 0, "words": [
          {"text": "hey", "start_ms": 1000, "end_ms": 1200}, ...
      ]}, ...]}

    Word timestamps are absolute (start_ms of chunk + aligner offset).
    """
    import numpy as np
    import soundfile as sf
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.chunks, "r", encoding="utf-8") as f:
        request = json.load(f)
    chunks_in = request["chunks"]

    audio, sr = sf.read(args.audio, dtype="float32")
    if sr != 16000:
        raise RuntimeError(f"expected 16 kHz audio, got {sr}")
    if audio.ndim != 1:
        audio = np.mean(audio, axis=1).astype("float32")

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"
    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )

    results = []
    total_samples = audio.shape[0]
    for c in chunks_in:
        start_s = int(round(c["start_ms"] * 16000 / 1000))
        end_s = int(round(c["end_ms"] * 16000 / 1000))
        start_s = max(0, start_s)
        end_s = min(total_samples, end_s)
        if end_s <= start_s:
            results.append({"chunk_idx": c["chunk_idx"], "words": []})
            continue
        slice_ = audio[start_s:end_s]
        fd, wav_path = tempfile.mkstemp(suffix="_chunk.wav")
        os.close(fd)
        try:
            sf.write(wav_path, slice_, 16000, subtype="FLOAT")
            aligned = model.align(
                audio=wav_path,
                text=c["text"],
                language="English",
            )
            word_stream = aligned[0]
            offset_ms = c["start_ms"]
            words_out = [
                {
                    "text": w.text,
                    "start_ms": int(round(w.start_time * 1000)) + offset_ms,
                    "end_ms": int(round(w.end_time * 1000)) + offset_ms,
                }
                for w in word_stream
            ]
        finally:
            try:
                os.remove(wav_path)
            except OSError:
                pass
        results.append({"chunk_idx": c["chunk_idx"], "words": words_out})

    with open(args.output, "w", encoding="utf-8") as f:
        json.dump({"chunks": results}, f, ensure_ascii=False)


def cmd_preload(args):
    """Warm Mel-Roformer + anvuew dereverb + Qwen3-ForcedAligner at bootstrap.

    Surfaces model-download failures before any real song is processed.
    """
    import torch
    from audio_separator.separator import Separator
    from qwen_asr import Qwen3ForcedAligner

    mel = Separator(model_file_dir=args.models_dir, output_format="WAV")
    mel.load_model(MEL_ROFORMER_MODEL)
    _free_vram(mel)

    dereverb = Separator(model_file_dir=args.models_dir, output_format="WAV")
    dereverb.load_model(DEREVERB_MODEL)
    _free_vram(dereverb)

    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"
    # `from_pretrained` downloads + instantiates the aligner. We don't poke
    # `model.parameters()` afterwards — the Qwen3ForcedAligner wrapper isn't
    # an nn.Module subclass and has no `.parameters()` method. Completing
    # `from_pretrained` without raising is proof enough that weights loaded.
    _model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )
    print(
        json.dumps(
            {
                "loaded": True,
                "device": device_map,
                "mel_roformer": MEL_ROFORMER_MODEL,
                "dereverb": DEREVERB_MODEL,
            }
        )
    )


def cmd_isolate_vocals(args):
    """Diagnostic: Mel-Roformer only, 16 kHz mono float32 WAV path printed."""
    import numpy as np
    import librosa
    import soundfile as sf
    from audio_separator.separator import Separator

    stem_dir = tempfile.mkdtemp(prefix="sp_diag_")
    try:
        sep = Separator(
            model_file_dir=args.models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep.load_model(MEL_ROFORMER_MODEL)
        out_files = sep.separate(args.audio)
        vocal_path = _pick_vocal_stem(out_files, stem_dir)
        _free_vram(sep)

        audio, _ = librosa.load(vocal_path, sr=16000, mono=True)
        peak = float(np.max(np.abs(audio))) if audio.size else 0.0
        if peak > 1.0:
            audio = audio / peak

        fd, resampled = tempfile.mkstemp(suffix="_vocals16k.wav")
        os.close(fd)
        sf.write(resampled, audio, 16000, subtype="FLOAT")
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)
    print(json.dumps({"vocal_path": resampled}))


def main():
    parser = argparse.ArgumentParser(description="SongPlayer lyrics Python helper")
    subparsers = parser.add_subparsers(dest="command", required=True)

    p_pre = subparsers.add_parser("preprocess-vocals")
    p_pre.add_argument("--audio", required=True)
    p_pre.add_argument("--output", required=True)
    p_pre.add_argument("--models-dir", required=True)

    p_ac = subparsers.add_parser("align-chunks")
    p_ac.add_argument("--audio", required=True)
    p_ac.add_argument("--chunks", required=True)
    p_ac.add_argument("--output", required=True)

    p_pl = subparsers.add_parser("preload")
    p_pl.add_argument("--models-dir", required=True)

    p_iv = subparsers.add_parser("isolate-vocals")
    p_iv.add_argument("--audio", required=True)
    p_iv.add_argument("--models-dir", required=True)

    args = parser.parse_args()
    dispatch = {
        "preprocess-vocals": cmd_preprocess_vocals,
        "align-chunks": cmd_align_chunks,
        "preload": cmd_preload,
        "isolate-vocals": cmd_isolate_vocals,
    }
    try:
        dispatch[args.command](args)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
