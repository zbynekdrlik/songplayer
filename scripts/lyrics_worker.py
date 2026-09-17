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
import contextlib
import gc
import json
import os
import shutil
import sys
import tempfile


MEL_ROFORMER_MODEL = "model_bs_roformer_ep_317_sdr_12.9755.ckpt"
DEREVERB_MODEL = "dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"

# #171 — resumable, segmented vocal isolation. The input is split into fixed
# time windows and each window is isolated+dereverbed+resampled independently,
# so a killed/timed-out run resumes from the segments already on disk instead of
# discarding the whole song. 30 s segments with a 2 s overlap that is linearly
# crossfaded on stitch (see `_stitch_segments`) smooth any per-window boundary
# artifact — the isolation output is internal 16 kHz mono thrown at the ASR /
# forced-aligner, so the exact crossfade shape is not audible-critical; the
# window is small enough that one CPU segment finishes well inside the Rust
# stall timeout (`heavy_plan::stall_timeout`), which is how a slow-but-
# progressing run is distinguished from a hung one.
ISOLATION_SEGMENT_SECONDS = 30.0
ISOLATION_OVERLAP_SECONDS = 2.0


def _segment_bounds(total_s, seg_s, overlap_s):
    """List of (start_s, end_s) windows covering [0, total_s]: `seg_s`-long,
    stepping by `seg_s - overlap_s`, the last window clamped to `total_s`.
    Adjacent windows share `overlap_s`, crossfaded on stitch. Pure — no I/O."""
    if total_s <= 0:
        return []
    step = max(1e-3, seg_s - overlap_s)
    bounds = []
    start = 0.0
    while True:
        end = min(start + seg_s, total_s)
        bounds.append((start, end))
        if end >= total_s - 1e-9:
            break
        start += step
    return bounds


def _stitch_segments(segments, step_samples, overlap_samples):
    """Overlap-add stitch of equally-stepped float32 segments (mono 1-D or
    (n, ch) 2-D) with a linear crossfade over `overlap_samples`. Segment i
    starts at global sample `i * step_samples`. Weight-normalised, so two
    adjacent linear ramps (fade-out + fade-in) sum to 1 and the crossfade region
    reconstructs the source exactly — and, applied with IDENTICAL weights to two
    additive stems (vocals + instrumental), preserves `v + i == mix`. Pure
    numpy, no I/O — locally unit-testable."""
    import numpy as np

    if not segments:
        return np.zeros(0, dtype=np.float32)
    first = np.asarray(segments[0])
    ch = first.shape[1] if first.ndim == 2 else 1
    last_len = np.asarray(segments[-1]).shape[0]
    total = (len(segments) - 1) * step_samples + last_len
    out = np.zeros((total, ch) if ch > 1 else (total,), dtype=np.float64)
    wsum = np.zeros(total, dtype=np.float64)
    n = len(segments)
    for i, seg in enumerate(segments):
        seg = np.asarray(seg, dtype=np.float64)
        length = seg.shape[0]
        w = np.ones(length, dtype=np.float64)
        if overlap_samples > 0:
            f = min(overlap_samples, length)
            if i > 0:
                w[:f] = np.linspace(0.0, 1.0, f, endpoint=False)
            if i < n - 1:
                w[length - f:] = np.linspace(1.0, 0.0, f, endpoint=False)
        s = i * step_samples
        out[s:s + length] += seg * (w[:, None] if seg.ndim == 2 else w)
        wsum[s:s + length] += w
    nz = wsum > 1e-9
    if out.ndim == 2:
        out[nz] /= wsum[nz][:, None]
    else:
        out[nz] /= wsum[nz]
    return out.astype(np.float32)


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


def _set_wddm_gpu_priority():
    """Drop this process's WDDM GPU scheduling priority to BELOW_NORMAL on
    Windows (#154) so vocal isolation leaves GPU-scheduling headroom for the
    live Media Foundation decoder + OBS/Resolume on the shared event PC.
    Best-effort: logs to stderr, never raises. No-op off Windows."""
    if sys.platform != "win32":
        return
    try:
        import ctypes

        # D3DKMT_SCHEDULINGPRIORITYCLASS: IDLE=0, BELOW_NORMAL=1, NORMAL=2,
        # ABOVE_NORMAL=3, HIGH=4, REALTIME=5.
        D3DKMT_SCHEDULINGPRIORITYCLASS_BELOW_NORMAL = 1
        kernel32 = ctypes.WinDLL("kernel32")
        gdi32 = ctypes.WinDLL("gdi32")
        kernel32.GetCurrentProcess.restype = ctypes.c_void_p
        # NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, enum): the
        # argument is the priority enum value directly.
        gdi32.D3DKMTSetProcessSchedulingPriorityClass.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
        ]
        gdi32.D3DKMTSetProcessSchedulingPriorityClass.restype = ctypes.c_long
        status = gdi32.D3DKMTSetProcessSchedulingPriorityClass(
            kernel32.GetCurrentProcess(),
            D3DKMT_SCHEDULINGPRIORITYCLASS_BELOW_NORMAL,
        )
        if status == 0:
            print("gpu_polite: WDDM GPU priority set to BELOW_NORMAL", file=sys.stderr)
        else:
            print(
                "gpu_polite: D3DKMTSetProcessSchedulingPriorityClass returned "
                f"NTSTATUS 0x{status & 0xFFFFFFFF:08x} (non-fatal)",
                file=sys.stderr,
            )
    except Exception as e:  # never fatal — priority is only an optimisation
        print(
            f"gpu_polite: WDDM GPU priority call failed (non-fatal): {e}",
            file=sys.stderr,
        )


def _gpu_mem_fraction():
    """LYRICS_GPU_MEM_FRACTION env → clamped float in [0.2, 0.95], default 0.7."""
    try:
        frac = float(os.environ.get("LYRICS_GPU_MEM_FRACTION", "0.7"))
    except (TypeError, ValueError):
        frac = 0.7
    return min(0.95, max(0.2, frac))


def gpu_polite(force_cpu=False):
    """GPU discipline for the shared win-resolume event PC (#154): BELOW_NORMAL
    WDDM scheduling priority + a per-process VRAM cap so vocal isolation leaves
    GPU headroom for the live MF decoder and OBS/Resolume. Model parameters are
    untouched — separation quality is unchanged; only scheduling priority and
    the VRAM ceiling move. Best-effort — never raises.

    #162: with `force_cpu` the VRAM cap is SKIPPED — the GPU is left completely
    untouched (inference is forced onto CPU in-process by `_force_cpu()`, which
    keeps CUDA initialized so the NVIDIA driver never unloads and crashes)."""
    # WDDM priority ONLY when CUDA is visible: on a CPU-only run (#162,
    # CUDA_VISIBLE_DEVICES=-1) nothing keeps the NVIDIA user-mode driver
    # loaded after D3DKMTSetProcessSchedulingPriorityClass returns, the DLL
    # unloads and a later call into it crashes the process
    # (nvdxgdmal64.dll_unloaded, 0xc0000005 — win-resolume 2026-09-15).
    try:
        import torch

        if torch.cuda.is_available():
            _set_wddm_gpu_priority()
            if force_cpu:
                return  # #162: forcing CPU — leave GPU memory untouched
            frac = _gpu_mem_fraction()
            torch.cuda.set_per_process_memory_fraction(frac)
            print(
                f"gpu_polite: CUDA per-process memory fraction capped at {frac}",
                file=sys.stderr,
            )
    except Exception as e:  # never fatal — the cap is only headroom insurance
        print(f"gpu_polite: VRAM cap failed (non-fatal): {e}", file=sys.stderr)


def _is_cuda_oom(exc):
    """True if `exc` is a CUDA out-of-memory error (typed or by message).

    torch is always imported before this is reached (the separator needs it),
    so a plain import is safe — no swallowed-exception path.
    """
    import torch

    return isinstance(exc, torch.cuda.OutOfMemoryError) or (
        "out of memory" in str(exc).lower()
    )


@contextlib.contextmanager
def _force_cpu():
    """Temporarily make torch report no CUDA device so audio-separator's device
    autodetection builds the model on CPU for the OOM-retry run (#154).
    audio-separator has no `use_cpu` flag, and setting CUDA_VISIBLE_DEVICES after
    the CUDA runtime is already initialised does not take effect in-process —
    patching the availability probe is the reliable in-process CPU force. Same
    model + same parameters ⇒ identical output, only slower. Restored on exit."""
    import torch

    orig_available = torch.cuda.is_available
    orig_count = torch.cuda.device_count
    orig_env = os.environ.get("CUDA_VISIBLE_DEVICES")
    torch.cuda.is_available = lambda: False
    torch.cuda.device_count = lambda: 0
    # "-1" (not ""): an invalid ordinal disables CUDA on every platform, and
    # Windows drops an empty-valued variable so "" would leave the GPU visible.
    os.environ["CUDA_VISIBLE_DEVICES"] = "-1"
    try:
        yield
    finally:
        torch.cuda.is_available = orig_available
        torch.cuda.device_count = orig_count
        if orig_env is None:
            os.environ.pop("CUDA_VISIBLE_DEVICES", None)
        else:
            os.environ["CUDA_VISIBLE_DEVICES"] = orig_env


def _isolate_one_segment(sep_mel, sep_dereverb, full, in_sr, start_s, end_s, out_path, stem_dir):
    """Isolate + dereverb ONE native-rate window `[start_s, end_s]` of `full`,
    resample to 16 kHz mono float32, and write it ATOMICALLY to `out_path`.

    `full` is (n,) mono or (ch, n) multi-channel at `in_sr`. `stem_dir` is a
    scratch dir the two already-loaded separators write into; it is cleared
    after each segment so it never grows across a long song."""
    import numpy as np
    import librosa
    import soundfile as sf

    s0 = max(0, int(round(start_s * in_sr)))
    s1 = int(round(end_s * in_sr))
    if full.ndim == 1:
        data = full[s0:s1]
    else:
        data = full[:, s0:s1].T  # (n, ch) for soundfile
    seg_in = os.path.join(stem_dir, "segin_" + os.path.basename(out_path))
    sf.write(seg_in, data, in_sr, subtype="FLOAT")

    # Step 1: Mel-Roformer vocal isolation. Step 2: anvuew dereverb on it.
    vocal_path = _pick_vocal_stem(sep_mel.separate(seg_in), stem_dir)
    dry_path = _pick_dereverbed_stem(sep_dereverb.separate(vocal_path), stem_dir)

    # Step 3: resample to exactly 16 kHz mono float32, peak-clamp, atomic write.
    audio, _ = librosa.load(dry_path, sr=16000, mono=True)
    peak = float(np.max(np.abs(audio))) if audio.size else 0.0
    if peak > 1.0:
        audio = audio / peak
    tmp = out_path + ".tmp"
    sf.write(tmp, audio, 16000, subtype="FLOAT")
    os.replace(tmp, out_path)

    # Clear the scratch dir for the next segment (we keep only out_path, which
    # lives in the work dir, not here).
    for f in os.listdir(stem_dir):
        with contextlib.suppress(OSError):
            os.remove(os.path.join(stem_dir, f))


def cmd_preprocess_vocals(args):
    """Mel-Roformer isolate → anvuew dereverb → 16 kHz mono float32 WAV, done
    RESUMABLY per segment (#171).

    The input is split into `ISOLATION_SEGMENT_SECONDS` windows; each is
    isolated+dereverbed+resampled and written to `--work-dir/seg_NNNN_of_MMMM.wav`.
    Segments already present are SKIPPED on start (so a killed/timed-out run
    resumes — logs `isolation resumed from chunk N/M`), the two models load once
    and are reused across every remaining segment, and the final WAV is stitched
    from the segment set (2 s linear crossfade over each overlap), written
    atomically to `--output`, and the work dir is removed. Writes a FLOAT WAV to
    --output. Exits 0 on success.

    GPU discipline (#154): `gpu_polite()` sets a BELOW_NORMAL WDDM scheduling
    priority + a per-process VRAM cap before any model loads. On a CUDA OOM the
    isolation re-runs on CPU (same model + parameters → identical output, only
    slower) — the separator's model parameters are never changed.
    """
    import numpy as np
    import librosa
    import soundfile as sf
    import torch
    from audio_separator.separator import Separator

    if args.force_cpu:
        print("lyrics_worker: forced CPU inference (--force-cpu)", file=sys.stderr)
    gpu_polite(force_cpu=args.force_cpu)

    work_dir = args.work_dir
    os.makedirs(work_dir, exist_ok=True)

    # Native-rate load, so the separator sees the full-quality signal (the 16 kHz
    # downsample happens only on each segment's dereverbed output).
    full, in_sr = librosa.load(args.audio, sr=None, mono=False)
    total_samples = full.shape[0] if full.ndim == 1 else full.shape[1]
    total_s = total_samples / float(in_sr)
    bounds = _segment_bounds(total_s, ISOLATION_SEGMENT_SECONDS, ISOLATION_OVERLAP_SECONDS)
    n_seg = len(bounds)

    def _seg_path(i):
        return os.path.join(work_dir, f"seg_{i:04d}_of_{n_seg:04d}.wav")

    done = [
        os.path.exists(_seg_path(i)) and os.path.getsize(_seg_path(i)) > 0
        for i in range(n_seg)
    ]
    if any(done) and not all(done):
        print(f"isolation resumed from chunk {sum(done)}/{n_seg}", file=sys.stderr)

    def _process_remaining(force_cpu):
        """Load both models once and process every not-yet-done segment. Wrapped
        so a CUDA OOM can retry the whole loop on CPU (resume skips finished
        segments)."""
        stem_dir = tempfile.mkdtemp(prefix="sp_stems_")
        cpu_ctx = _force_cpu() if force_cpu else contextlib.nullcontext()
        try:
            with cpu_ctx:
                sep_mel = Separator(
                    model_file_dir=args.models_dir,
                    output_format="WAV",
                    output_dir=stem_dir,
                    use_soundfile=True,
                )
                sep_mel.load_model(MEL_ROFORMER_MODEL)
                sep_dereverb = Separator(
                    model_file_dir=args.models_dir,
                    output_format="WAV",
                    output_dir=stem_dir,
                    use_soundfile=True,
                )
                sep_dereverb.load_model(DEREVERB_MODEL)
                for i, (s_s, e_s) in enumerate(bounds):
                    if done[i]:
                        continue
                    _isolate_one_segment(
                        sep_mel, sep_dereverb, full, in_sr, s_s, e_s, _seg_path(i), stem_dir
                    )
                    done[i] = True
                    print(f"isolation chunk {i + 1}/{n_seg} done", file=sys.stderr)
                _free_vram(sep_mel)
                _free_vram(sep_dereverb)
        finally:
            shutil.rmtree(stem_dir, ignore_errors=True)

    if not all(done):
        try:
            _process_remaining(force_cpu=args.force_cpu)
        except Exception as e:
            # #162: the CUDA-OOM→CPU retry is for the GPU path only. A forced-CPU
            # run has no GPU to fall back from, so a failure there is a real error.
            if args.force_cpu or not _is_cuda_oom(e):
                raise
            print(
                "gpu_polite: CUDA OOM during vocal isolation — retrying on CPU "
                "(identical model + parameters, only slower) [#154]",
                file=sys.stderr,
            )
            gc.collect()
            if torch.cuda.is_available():
                torch.cuda.empty_cache()
            _process_remaining(force_cpu=True)

    # Stitch every segment (all 16 kHz mono) into the final WAV.
    step_samples = int(round((ISOLATION_SEGMENT_SECONDS - ISOLATION_OVERLAP_SECONDS) * 16000))
    overlap_samples = int(round(ISOLATION_OVERLAP_SECONDS * 16000))
    segs = []
    for i in range(n_seg):
        a, _ = sf.read(_seg_path(i), dtype="float32")
        if a.ndim > 1:
            a = np.mean(a, axis=1).astype("float32")
        segs.append(a)
    stitched = _stitch_segments(segs, step_samples, overlap_samples)
    peak = float(np.max(np.abs(stitched))) if stitched.size else 0.0
    if peak > 1.0:
        stitched = stitched / peak
    tmp_out = args.output + ".tmp"
    sf.write(tmp_out, stitched, 16000, subtype="FLOAT")
    os.replace(tmp_out, args.output)
    shutil.rmtree(work_dir, ignore_errors=True)

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

    # preprocess_vocals is the only producer of --audio and always writes
    # 16 kHz mono float32. Don't re-check the sample rate here — the
    # previous guard was dead defense that hid resample bugs upstream
    # behind a generic RuntimeError. If the WAV drifts from 16 kHz we
    # want Qwen3's own assertion (it reads at 16 kHz internally) to
    # surface the actual stack trace.
    audio, _sr = sf.read(args.audio, dtype="float32")
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
    # #171: per-segment scratch dir for resumable isolation. Each segment WAV is
    # written here and skipped on resume; removed once the final --output stitch
    # lands.
    p_pre.add_argument("--work-dir", required=True)
    # #162: force in-process CPU inference from the start (leaves the GPU
    # untouched for the live wall) instead of only as the CUDA-OOM fallback.
    p_pre.add_argument("--force-cpu", action="store_true")

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
