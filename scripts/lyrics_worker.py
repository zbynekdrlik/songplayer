#!/usr/bin/env python3
"""
lyrics_worker.py — narrow Python entry points for the lyrics pipeline.

Commands:
  preprocess-vocals  anvuew dereverb + 16 kHz mono float32 WAV of the stems
                     worker's vocals sidecar (--vocals-in); no isolation pass
  preload            Warm the anvuew dereverb model at boot

#144: the forced alignment is the mtl aligner's (its own venv and script);
the retired Qwen3 aligner and its chunk-alignment command are deleted.
"""

import argparse
import contextlib
import gc
import json
import os
import shutil
import sys
import tempfile


# #144: one separation per video — the mtl aligner's vocals come from the stems
# worker's Kim vocals sidecar (`{base}_audio_vocals.flac`, #184 G0). The second
# BS-RoFormer isolation model is deleted; `preprocess-vocals` only dereverbs.
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


# #207: the heavy child runs under a 10 GiB per-process job cap, so
# `preprocess-vocals` never holds a whole-song array: each window is read from
# the vocals sidecar on demand (`_read_window`) and the 16 kHz segments are
# stitched block by block (`_stitched_blocks` / `_stitch_to_wav`), the stem
# worker's design (`stem_worker.py`, 7ff38c73). `_stitch_segments` above stays
# as the reference the tests compare the streamed stitch against.

# A header frame count at or above this is libsndfile's "unknown length"
# sentinel (e.g. a FLAC from a piped encoder has STREAMINFO total samples = 0).
_UNKNOWN_FRAMES = 1 << 62


def _audio_info(path):
    """(sample_rate, frames) of an audio file, from its header — no samples
    are read. The segment plan trusts this count, so an unknown or empty one
    raises a clear ValueError instead of planning a bogus number of windows."""
    import soundfile as sf

    info = sf.info(path)
    if info.samplerate <= 0 or info.frames <= 0 or info.frames >= _UNKNOWN_FRAMES:
        raise ValueError(
            f"unusable header in {path}: frame count {info.frames} "
            f"at {info.samplerate} Hz (unknown or empty length)"
        )
    return info.samplerate, info.frames


def _read_window(path, in_sr, start_s, end_s):
    """Read ONE native-rate window `[start_s, end_s]` straight from the file.

    Same sample bounds and layout as the old whole-file slice
    (`full[s0:s1]` / `full[:, s0:s1].T` of `librosa.load(sr=None, mono=False)`,
    which is itself a soundfile float32 read): float32, `(n,)` for mono,
    `(n, ch)` otherwise. soundfile clamps `stop` to the file length exactly
    like the numpy slice did."""
    import soundfile as sf

    s0 = max(0, int(round(start_s * in_sr)))
    s1 = int(round(end_s * in_sr))
    data, _ = sf.read(path, start=s0, stop=s1, dtype="float32", always_2d=False)
    return data


def _stitched_blocks(read_segment, n_seg, step_samples, overlap_samples):
    """Yield the samples of
    `_stitch_segments([read_segment(i) for i in range(n_seg)], step_samples,
    overlap_samples)` in order, ONE block per segment read, holding only the
    tail the next segment still adds to — never the whole song.

    The same arithmetic as the reference: mono segments, segment i starting at
    global sample `i * step_samples`, accumulated in float64 in segment order
    with the same linear crossfade weights, weight-normalised, cast to
    float32. Once segment i is added, every sample before segment i + 1's
    start is final, so it is yielded at once. An earlier segment that ends
    past the last one raises ValueError, as the reference's broadcast does."""
    import numpy as np

    acc = np.zeros(0, dtype=np.float64)
    wsum = np.zeros(0, dtype=np.float64)
    base = 0  # global index of acc[0] == samples already yielded
    for i in range(n_seg):
        seg = np.asarray(read_segment(i), dtype=np.float64)
        length = seg.shape[0]
        end = i * step_samples + length
        settle = (i + 1) * step_samples if i < n_seg - 1 else end
        held_end = base + acc.shape[0]
        if i == n_seg - 1 and held_end > end:
            raise ValueError(
                f"an earlier segment ends past the last one ({held_end} > {end} samples)"
            )
        grow = max(end, settle) - held_end
        if grow > 0:
            acc = np.concatenate([acc, np.zeros(grow, dtype=np.float64)])
            wsum = np.concatenate([wsum, np.zeros(grow, dtype=np.float64)])
        # IDENTICAL to the weights in `_stitch_segments`.
        w = np.ones(length, dtype=np.float64)
        if overlap_samples > 0:
            f = min(overlap_samples, length)
            if i > 0:
                w[:f] = np.linspace(0.0, 1.0, f, endpoint=False)
            if i < n_seg - 1:
                w[length - f :] = np.linspace(1.0, 0.0, f, endpoint=False)
        a = i * step_samples - base
        acc[a : a + length] += seg * w
        wsum[a : a + length] += w
        k = settle - base
        block = acc[:k].copy()
        nz = wsum[:k] > 1e-9
        block[nz] /= wsum[:k][nz]
        # Copy so the yielded head is actually released.
        acc = acc[k:].copy()
        wsum = wsum[k:].copy()
        base = settle
        yield block.astype(np.float32)


def _stitch_to_wav(read_segment, n_seg, step_samples, overlap_samples, out_path):
    """Stream-stitch the `n_seg` 16 kHz mono segments (`read_segment(i)`)
    into a FLOAT WAV at `out_path`: the samples of `_stitch_segments`, divided
    by their global peak when it is over 1.0, exactly as the whole-array path
    did. Two passes, each holding one segment and the overlap tail: the first
    finds the peak, the second writes. Published atomically: `<out>.tmp`, then
    `os.replace`; a failure removes the `.tmp` and leaves `out_path` alone.
    Returns the peak."""
    import numpy as np
    import soundfile as sf

    peak = 0.0
    for block in _stitched_blocks(read_segment, n_seg, step_samples, overlap_samples):
        if block.size:
            peak = max(peak, float(np.max(np.abs(block))))
    tmp = out_path + ".tmp"
    try:
        # format= is REQUIRED: the atomic temp path ends in ".tmp".
        with sf.SoundFile(
            tmp, "w", samplerate=16000, channels=1, format="WAV", subtype="FLOAT"
        ) as f:
            for block in _stitched_blocks(read_segment, n_seg, step_samples, overlap_samples):
                f.write(block / peak if peak > 1.0 else block)
        os.replace(tmp, out_path)
    finally:
        if os.path.exists(tmp):
            with contextlib.suppress(OSError):
                os.remove(tmp)
    return peak


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


def _atomic_write_wav(path, audio, sr):
    """Write `audio` (mono or (n, ch)) to `path` atomically as a FLOAT WAV: write
    a `<path>.tmp` scratch, then `os.replace` into place.

    `format="WAV"` is REQUIRED — the `.tmp` scratch extension is unknown to
    soundfile, which otherwise infers no format and raises
    `TypeError: ... unable to get format from file extension` and kills every
    isolation run before its first segment lands (#171, win-resolume 0.53.0-dev.2:
    `seg_0000_of_0013.wav.tmp`). The whole-song path wrote straight to a `.wav`
    output so it never hit this; `stem_worker._separate_one_segment` already
    passes `format="WAV"`, which is why stem separation was unaffected."""
    import soundfile as sf

    tmp = path + ".tmp"
    sf.write(tmp, audio, sr, format="WAV", subtype="FLOAT")
    os.replace(tmp, path)


def _dereverb_one_segment(
    sep_dereverb, vocals_path, in_sr, start_s, end_s, out_path, stem_dir
):
    """Dereverb ONE native-rate window `[start_s, end_s]` of the file at
    `vocals_path`, resample to 16 kHz mono float32, and write it ATOMICALLY to
    `out_path`.

    #144: the file is the stems worker's VOCALS sidecar (already isolated by
    the Kim Mel-Band RoFormer), so there is no second isolation pass — just
    anvuew dereverb + resample. #207: only this window is ever in memory
    (`_read_window`, the same samples the old whole-file slice gave).
    `stem_dir` is a scratch dir the already-loaded separator writes into; it is
    cleared after each segment so it never grows across a long song."""
    import numpy as np
    import librosa
    import soundfile as sf

    data = _read_window(vocals_path, in_sr, start_s, end_s)  # (n[, ch]) float32
    seg_in = os.path.join(stem_dir, "segin_" + os.path.basename(out_path))
    sf.write(seg_in, data, in_sr, subtype="FLOAT")

    # anvuew dereverb on the supplied vocals window (one pass, no isolation).
    dry_path = _pick_dereverbed_stem(sep_dereverb.separate(seg_in), stem_dir)

    # Resample to exactly 16 kHz mono float32, peak-clamp, atomic write.
    audio, _ = librosa.load(dry_path, sr=16000, mono=True)
    peak = float(np.max(np.abs(audio))) if audio.size else 0.0
    if peak > 1.0:
        audio = audio / peak
    _atomic_write_wav(out_path, audio, 16000)

    # Clear the scratch dir for the next segment (we keep only out_path, which
    # lives in the work dir, not here).
    for f in os.listdir(stem_dir):
        with contextlib.suppress(OSError):
            os.remove(os.path.join(stem_dir, f))


def cmd_preprocess_vocals(args):
    """anvuew dereverb → 16 kHz mono float32 WAV of the stems worker's vocals
    sidecar (`--vocals-in`), done RESUMABLY per segment (#171).

    #144: the mtl aligner's vocals come from the stems worker's Kim vocals
    sidecar (`{base}_audio_vocals.flac`, #184 G0), so this ONLY dereverbs +
    resamples — the second BS-RoFormer isolation pass is deleted (it stalled on
    the contained-CPU box; one separation per video, the owner's one-thing
    doctrine). The input is split into `ISOLATION_SEGMENT_SECONDS` windows; each
    is dereverbed+resampled and written to `--work-dir/seg_NNNN_of_MMMM.wav`.
    Segments already present are SKIPPED on start (so a killed/timed-out run
    resumes — logs `isolation resumed from chunk N/M`), the dereverb model loads
    once and is reused across every remaining segment, and the final WAV is
    stitched from the segment set (2 s linear crossfade over each overlap),
    written atomically to `--output`, and the work dir is removed. Writes a FLOAT
    WAV to --output. Exits 0 on success.

    #207: memory is O(segment), independent of the song's length — each window
    is read from the sidecar on demand (`_read_window`) and the segments are
    stitched in two streamed passes (`_stitch_to_wav`). Never load the whole
    sidecar or build a whole-length array here: the child runs under a 10 GiB
    per-process job cap.

    GPU discipline (#154): `gpu_polite()` sets a BELOW_NORMAL WDDM scheduling
    priority + a per-process VRAM cap before any model loads. On a CUDA OOM the
    dereverb re-runs on CPU (same model + parameters → identical output, only
    slower) — the separator's model parameters are never changed.
    """
    import numpy as np
    import soundfile as sf
    import torch
    from audio_separator.separator import Separator

    if args.force_cpu:
        print("lyrics_worker: forced CPU inference (--force-cpu)", file=sys.stderr)
    gpu_polite(force_cpu=args.force_cpu)

    work_dir = args.work_dir
    os.makedirs(work_dir, exist_ok=True)

    print(
        f"preprocess-vocals: vocals from stems sidecar {args.vocals_in}",
        file=sys.stderr,
    )
    # #207: header only — each native-rate window is read from the sidecar on
    # demand, so the dereverb sees the full-quality signal (the 16 kHz
    # downsample happens only on each segment's dereverbed output) and the
    # whole sidecar is never in memory (10 GiB per-child job cap).
    in_sr, total_samples = _audio_info(args.vocals_in)
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
        """Load the dereverb model once and process every not-yet-done segment.
        Wrapped so a CUDA OOM can retry the whole loop on CPU (resume skips
        finished segments)."""
        stem_dir = tempfile.mkdtemp(prefix="sp_stems_")
        cpu_ctx = _force_cpu() if force_cpu else contextlib.nullcontext()
        try:
            with cpu_ctx:
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
                    _dereverb_one_segment(
                        sep_dereverb,
                        args.vocals_in,
                        in_sr,
                        s_s,
                        e_s,
                        _seg_path(i),
                        stem_dir,
                    )
                    done[i] = True
                    print(f"isolation chunk {i + 1}/{n_seg} done", file=sys.stderr)
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

    # Stitch every segment (all 16 kHz mono) into the final WAV, STREAMED
    # segment by segment in two passes (#207) — never a whole-length array.
    step_samples = int(round((ISOLATION_SEGMENT_SECONDS - ISOLATION_OVERLAP_SECONDS) * 16000))
    overlap_samples = int(round(ISOLATION_OVERLAP_SECONDS * 16000))

    def _read_segment(i):
        a, _ = sf.read(_seg_path(i), dtype="float32")
        if a.ndim > 1:
            a = np.mean(a, axis=1).astype("float32")
        return a

    _stitch_to_wav(_read_segment, n_seg, step_samples, overlap_samples, args.output)
    shutil.rmtree(work_dir, ignore_errors=True)

    print(json.dumps({"output": args.output}))


def cmd_preload(args):
    """Warm the anvuew dereverb model at bootstrap.

    Surfaces a model-download failure before any real song is processed. #144:
    the BS-RoFormer isolation model is no longer warmed — the mtl vocals come
    from the stems worker's sidecar, so `preprocess-vocals` only dereverbs —
    and neither is the retired Qwen aligner (v22: mtl in its own venv).
    """
    from audio_separator.separator import Separator

    dereverb = Separator(model_file_dir=args.models_dir, output_format="WAV")
    dereverb.load_model(DEREVERB_MODEL)
    _free_vram(dereverb)
    print(json.dumps({"loaded": True, "dereverb": DEREVERB_MODEL}))


def main():
    parser = argparse.ArgumentParser(description="SongPlayer lyrics Python helper")
    subparsers = parser.add_subparsers(dest="command", required=True)

    p_pre = subparsers.add_parser("preprocess-vocals")
    # #144: the input is the stems worker's vocals sidecar, not the mix — the
    # BS-RoFormer isolation pass is deleted, so `preprocess-vocals` only dereverbs
    # + resamples this already-isolated vocals track.
    p_pre.add_argument("--vocals-in", dest="vocals_in", required=True)
    p_pre.add_argument("--output", required=True)
    p_pre.add_argument("--models-dir", required=True)
    # #171: per-segment scratch dir for resumable isolation. Each segment WAV is
    # written here and skipped on resume; removed once the final --output stitch
    # lands.
    p_pre.add_argument("--work-dir", required=True)
    # #162: force in-process CPU inference from the start (leaves the GPU
    # untouched for the live wall) instead of only as the CUDA-OOM fallback.
    p_pre.add_argument("--force-cpu", action="store_true")

    p_pl = subparsers.add_parser("preload")
    p_pl.add_argument("--models-dir", required=True)

    args = parser.parse_args()
    dispatch = {
        "preprocess-vocals": cmd_preprocess_vocals,
        "preload": cmd_preload,
    }
    try:
        dispatch[args.command](args)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
