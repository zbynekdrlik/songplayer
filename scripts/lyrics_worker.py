#!/usr/bin/env python3
"""
lyrics_worker.py — Python ML helper for Qwen3 lyrics alignment and transcription.

Commands:
  check-gpu         Print GPU availability as JSON
  download-models   Download Qwen3 models via huggingface_hub
  transcribe        Transcribe audio to text using Qwen3-ASR-1.7B
  align             Align lyrics text to audio using Qwen3-ForcedAligner-0.6B
"""

import argparse
import json
import sys
import os


def cmd_check_gpu(args):
    """Print GPU info as JSON: {"gpu": bool, "device": str, "vram_gb": float}"""
    try:
        import torch
        if torch.cuda.is_available():
            idx = torch.cuda.current_device()
            name = torch.cuda.get_device_name(idx)
            props = torch.cuda.get_device_properties(idx)
            vram_gb = props.total_memory / (1024 ** 3)
            result = {"gpu": True, "device": name, "vram_gb": round(vram_gb, 2)}
        else:
            result = {"gpu": False, "device": "cpu", "vram_gb": 0.0}
    except ImportError:
        result = {"gpu": False, "device": "cpu", "vram_gb": 0.0}

    print(json.dumps(result))


def cmd_download_models(args):
    """Download Qwen3 aligner and ASR models to models_dir."""
    from huggingface_hub import snapshot_download

    os.makedirs(args.models_dir, exist_ok=True)

    models = [
        "Qwen/Qwen3-ForcedAligner-0.6B",
        "Qwen/Qwen3-ASR-1.7B",
    ]

    for model_id in models:
        local_name = model_id.replace("/", "--")
        local_dir = os.path.join(args.models_dir, local_name)
        print(f"Downloading {model_id} -> {local_dir}", flush=True)
        snapshot_download(repo_id=model_id, local_dir=local_dir)
        print(f"Done: {model_id}", flush=True)


def cmd_transcribe(args):
    """
    Transcribe audio using Qwen3-ASR-1.7B.
    Writes JSON {"text": "..."} to --output.
    """
    import torch
    from transformers import AutoProcessor, AutoModelForSpeechSeq2Seq

    model_dir = os.path.join(args.models_dir, "Qwen--Qwen3-ASR-1.7B")
    device = "cuda" if torch.cuda.is_available() else "cpu"
    dtype = torch.float16 if device == "cuda" else torch.float32

    processor = AutoProcessor.from_pretrained(model_dir)
    model = AutoModelForSpeechSeq2Seq.from_pretrained(
        model_dir,
        torch_dtype=dtype,
        low_cpu_mem_usage=True,
    ).to(device)

    import librosa
    audio, sr = librosa.load(args.audio, sr=16000, mono=True)

    inputs = processor(
        audio,
        sampling_rate=16000,
        return_tensors="pt",
    ).to(device)
    if dtype == torch.float16:
        inputs = {k: v.half() if v.is_floating_point() else v for k, v in inputs.items()}

    with torch.no_grad():
        generated_ids = model.generate(**inputs)

    transcription = processor.batch_decode(generated_ids, skip_special_tokens=True)[0]
    result = {"text": transcription.strip()}

    with open(args.output, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False)


MEL_ROFORMER_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"


def _pick_vocal_stem(out_files, fallback_dir):
    """Return the absolute path of the vocal stem among `out_files`.

    Prefers filenames that contain 'Vocals'/'vocals'; falls back to the
    single file that does NOT contain 'Instrumental' so we are robust to
    audio-separator changing its stem-filename template across versions.
    """
    import os

    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    vocal = [p for p in out_files if "Vocals" in p or "vocals" in p]
    if vocal:
        return _abs(vocal[0])

    non_inst = [p for p in out_files if "Instrumental" not in p and "instrumental" not in p]
    if len(non_inst) == 1:
        return _abs(non_inst[0])

    raise RuntimeError(
        f"audio-separator did not produce an identifiable Vocals stem (got: {out_files})"
    )


def _isolate_vocals(audio_path, models_dir):
    """Run Mel-Roformer to extract the vocal stem, then resample it to
    exactly 16 kHz mono float32 WAV in [-1, 1] — the input format
    Qwen3-ForcedAligner expects. Returns the absolute path to a temp WAV
    that the caller MUST delete after use.

    Sequential-load pattern: this function loads Mel-Roformer, separates,
    then relies on garbage collection + explicit torch.cuda.empty_cache()
    to free ~6-8 GB of VRAM before the caller loads Qwen3.

    Temp-file hygiene: writes all stems to a per-call `mkdtemp` directory
    and removes the entire directory on return, so the Instrumental stem
    Mel-Roformer emits alongside Vocals does not leak (~50-80 MB/song).
    The returned 16 kHz WAV lives outside that dir so the caller controls
    its lifetime.
    """
    import gc
    import os
    import shutil
    import tempfile
    import numpy as np
    import torch
    from audio_separator.separator import Separator
    import librosa
    import soundfile as sf

    stem_dir = tempfile.mkdtemp(prefix="sp_stems_")
    try:
        sep = Separator(
            model_file_dir=models_dir,
            output_format="WAV",
            output_dir=stem_dir,
        )
        sep.load_model(MEL_ROFORMER_MODEL)
        out_files = sep.separate(audio_path)
        vocal_path = _pick_vocal_stem(out_files, stem_dir)

        # Free VRAM before loading the aligner. audio-separator keeps an
        # ONNX Runtime session that GC can miss — dereference the public
        # model_instance first so ORT session teardown is invoked.
        if hasattr(sep, "model_instance"):
            sep.model_instance = None
        del sep
        gc.collect()
        if torch.cuda.is_available():
            torch.cuda.empty_cache()

        # Resample to exactly 16 kHz mono float32. Qwen3's docstring: "All
        # audios will be converted into mono 16k float32 arrays in [-1, 1]."
        # We do this explicitly instead of relying on qwen_asr.normalize_audios()
        # so we control the mono-conversion strategy (librosa averages channels,
        # preserving energy on hard-panned vocals) and get a smaller intermediate
        # file for faster subprocess I/O.
        audio, _ = librosa.load(vocal_path, sr=16000, mono=True)

        # Peak-normalize only if the separator output exceeds [-1, 1].
        # Hot-mastered source material can survive Mel-Roformer with
        # residual peaks > 1.0, which can confuse the aligner's feature
        # extractor. No-op for typical vocal stems (peak < 1).
        peak = float(np.max(np.abs(audio))) if audio.size else 0.0
        if peak > 1.0:
            audio = audio / peak

        fd, resampled_path = tempfile.mkstemp(suffix="_vocals16k.wav")
        os.close(fd)
        try:
            sf.write(resampled_path, audio, 16000, subtype="FLOAT")
        except Exception:
            try:
                os.remove(resampled_path)
            except OSError:
                pass
            raise
        return resampled_path
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)


def cmd_align(args):
    """
    Align lyrics text to audio using Qwen3-ForcedAligner-0.6B.

    Pipeline:
        1. Mel-Roformer isolates the vocal stem from the mixed audio.
        2. Resample vocal stem to 16 kHz mono float32 (Qwen3's expected input).
        3. Qwen3-ForcedAligner aligns text to the clean vocal WAV.

    --text is a PATH to a UTF-8 text file with one lyric line per row.
    Writes JSON {"lines": [{"en": str, "words": [{"text": str, "start_ms": int, "end_ms": int}]}]}
    to --output.
    """
    import os
    import torch
    from qwen_asr import Qwen3ForcedAligner

    with open(args.text, "r", encoding="utf-8") as f:
        lyrics_lines = [line.strip() for line in f.read().splitlines() if line.strip()]

    if not lyrics_lines:
        _write_output([], args.output)
        return

    # Step 1+2: isolate + resample. Returns a 16 kHz mono float32 WAV path.
    vocal_path = _isolate_vocals(args.audio, args.models_dir)

    try:
        # Step 3: align against the clean vocal stem, not the mixed audio.
        device_map = "cuda:0" if torch.cuda.is_available() else "cpu"

        model = Qwen3ForcedAligner.from_pretrained(
            "Qwen/Qwen3-ForcedAligner-0.6B",
            dtype=torch.bfloat16,
            device_map=device_map,
        )

        full_text = "\n".join(lyrics_lines)

        results = model.align(
            audio=vocal_path,
            text=full_text,
            language="English",
        )

        word_stream = results[0]
        lines_out = _group_words_into_lines(word_stream, lyrics_lines)
        _write_output(lines_out, args.output)
    finally:
        try:
            os.remove(vocal_path)
        except OSError:
            pass


def _group_words_into_lines(word_stream, lyrics_lines):
    """Walk the flat aligned word stream and the source line list in parallel,
    assigning each aligned word to the next source line based on expected word
    count per line. Returns the list of {en, words} dicts."""
    words_flat = [
        {
            "text": w.text,
            "start_ms": int(round(w.start_time * 1000)),
            "end_ms": int(round(w.end_time * 1000)),
        }
        for w in word_stream
    ]

    out = []
    idx = 0
    total = len(words_flat)
    for line_text in lyrics_lines:
        expected = max(1, len(line_text.split()))
        end = min(idx + expected, total)
        out.append({"en": line_text, "words": words_flat[idx:end]})
        idx = end
    return out


def _write_output(lines, output_path):
    result = {"lines": lines}
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False)


def cmd_preload(args):
    """Force model download + load to surface failures at bootstrap time
    rather than on the first real song. Preloads BOTH the Mel-Roformer
    separator checkpoint (~500 MB) and the Qwen3-ForcedAligner model
    (~1.2 GB). Exits 0 on success, non-zero otherwise.

    Without Mel-Roformer preload the first real song pays ~500 MB of
    downloads inside the 300 s alignment subprocess timeout, which can
    push the first-song alignment past the timeout on a slow link.
    """
    import torch
    from audio_separator.separator import Separator
    from qwen_asr import Qwen3ForcedAligner

    # 1. Warm Mel-Roformer: download the .ckpt into models_dir. Load it
    # and then dereference so the VRAM is free before Qwen3 loads.
    mel_sep = Separator(model_file_dir=args.models_dir, output_format="WAV")
    mel_sep.load_model(MEL_ROFORMER_MODEL)
    if hasattr(mel_sep, "model_instance"):
        mel_sep.model_instance = None
    del mel_sep
    if torch.cuda.is_available():
        torch.cuda.empty_cache()

    # 2. Warm Qwen3-ForcedAligner.
    device_map = "cuda:0" if torch.cuda.is_available() else "cpu"
    model = Qwen3ForcedAligner.from_pretrained(
        "Qwen/Qwen3-ForcedAligner-0.6B",
        dtype=torch.bfloat16,
        device_map=device_map,
    )
    _ = next(model.parameters())
    print(json.dumps({"loaded": True, "device": device_map, "mel_roformer": MEL_ROFORMER_MODEL}))


def cmd_isolate_vocals(args):
    """Diagnostic: run Mel-Roformer vocal isolation + 16 kHz mono resample
    on a given audio file and print the resulting WAV path. Useful for
    manual validation on win-resolume after deploy. The caller owns the
    resulting file and should delete it when done."""
    path = _isolate_vocals(args.audio, args.models_dir)
    print(json.dumps({"vocal_path": path}))


def main():
    parser = argparse.ArgumentParser(
        description="Qwen3 ML helper for lyrics alignment and transcription"
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # check-gpu
    subparsers.add_parser("check-gpu", help="Print GPU availability as JSON")

    # download-models
    p_dl = subparsers.add_parser("download-models", help="Download Qwen3 models")
    p_dl.add_argument("--models-dir", required=True, help="Directory to store models")

    # transcribe
    p_tr = subparsers.add_parser("transcribe", help="Transcribe audio to text")
    p_tr.add_argument("--audio", required=True, help="Path to audio file")
    p_tr.add_argument("--output", required=True, help="Path to write output JSON")
    p_tr.add_argument("--models-dir", required=True, help="Directory containing models")

    # align
    p_al = subparsers.add_parser("align", help="Align lyrics text to audio")
    p_al.add_argument("--audio", required=True, help="Path to audio file")
    p_al.add_argument("--text", required=True, help="Path to text file with lyrics")
    p_al.add_argument("--output", required=True, help="Path to write output JSON")
    p_al.add_argument("--models-dir", required=True, help="Directory containing models")

    # preload
    subparsers.add_parser("preload", help="Download + load model to surface failures early")

    # isolate-vocals
    p_iso = subparsers.add_parser(
        "isolate-vocals",
        help="Isolate vocals with Mel-Roformer and resample to 16 kHz mono (diagnostic)",
    )
    p_iso.add_argument("--audio", required=True, help="Path to mixed audio file")
    p_iso.add_argument("--models-dir", required=True, help="Directory containing models")

    args = parser.parse_args()

    dispatch = {
        "check-gpu": cmd_check_gpu,
        "download-models": cmd_download_models,
        "transcribe": cmd_transcribe,
        "align": cmd_align,
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
