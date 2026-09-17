#!/usr/bin/env python3
"""stem_worker.py — karaoke stem separation for songplayer (#14).

Separates a normalized `{id}_audio.flac` mix into two full-quality sidecars for
the karaoke playback modes:

  {id}_audio_vocals.flac        — vocals stem
  {id}_audio_instrumental.flac  — instrumental / accompaniment stem

Both are 48 kHz STEREO FLAC (matching the mix + the decoder requirement), with
NO dereverb — the opposite of the lyric-alignment `preprocess-vocals` path
(16 kHz mono, dereverbed, vocals-only), which stays untouched.

Separator: **Mel-Band RoFormer "Kim" (`vocals_mel_band_roformer.ckpt`, MIT)** via
`audio-separator` — chosen 2026-09-14 after a fresh SOTA survey + on-box
measurement (VRAM ~3 GB / 8× realtime / MVSep-Multisong vocals 11.01 ≥ the
old ambiguously-licensed on-box BS-RoFormer). See `.claude/rules/karaoke-stems.md`.

Commands:
  separate  Kim two-stem separation → vocals + instrumental FLAC @ 48 kHz stereo
  preload   warm the Kim model at bootstrap (surface download failures early)

GPU discipline (#154): mirrors lyrics_worker.py — BELOW_NORMAL WDDM scheduling
priority + a per-process VRAM cap (`LYRICS_GPU_MEM_FRACTION`) so separation
leaves headroom for the live MF decoder on the shared win-resolume box. On a
CUDA OOM the separation re-runs on CPU (same model + parameters → identical
output, only slower). The idle GATE (Rust `stems` worker) is the PRIMARY
mechanism — separation only ever runs while the wall is idle.
"""

import argparse
import contextlib
import gc
import json
import os
import re
import shutil
import sys
import tempfile

# The chosen karaoke separator (MIT-licensed, best-measured 8 GB two-stem).
KARAOKE_STEM_MODEL = "vocals_mel_band_roformer.ckpt"

# Output rate/format for the karaoke stems: match the -14 LUFS mix FLAC and the
# SplitSyncedDecoder requirement (48 kHz, stereo).
OUTPUT_SAMPLE_RATE = 48000

# #171 — resumable, segmented separation. The mix is split into fixed windows;
# each is separated independently and written to the work dir, so a killed /
# timed-out run resumes from the segments already on disk. 30 s windows with a
# 2 s overlap linearly crossfaded on stitch — because the crossfade weights are
# IDENTICAL for vocals + instrumental, `vocals + instrumental == mix` additivity
# (the karaoke-stems invariant, karaoke-stems.md) is preserved by linearity.
STEM_SEGMENT_SECONDS = 30.0
STEM_OVERLAP_SECONDS = 2.0


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
    numpy, no I/O."""
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


def _stem_token(fname):
    """audio-separator names each stem by a PARENTHESIZED token, e.g.
    'song_(Vocals)_model.flac' / 'song_(other)_model.flac'. Match on THAT, never
    the raw filename — the Kim model's own name contains the substring 'vocals',
    so a naive `"vocals" in filename` picker grabs the wrong file (measured
    2026-09-14)."""
    m = re.search(r"\(([^)]+)\)", os.path.basename(fname))
    return m.group(1).lower() if m else ""


def _pick(out_files, wanted_tokens, fallback_dir):
    """Return the absolute path of the stem whose parenthesized token is in
    `wanted_tokens`."""
    def _abs(p):
        return p if os.path.isabs(p) else os.path.join(fallback_dir, p)

    for f in out_files:
        if _stem_token(f) in wanted_tokens:
            return _abs(f)
    raise RuntimeError(
        f"audio-separator did not produce a {wanted_tokens} stem (got: {out_files})"
    )


def _set_wddm_gpu_priority():
    """BELOW_NORMAL WDDM GPU scheduling priority on Windows (#154). Best-effort;
    logs to stderr, never raises. No-op off Windows."""
    if sys.platform != "win32":
        return
    try:
        import ctypes

        below_normal = 1  # D3DKMT_SCHEDULINGPRIORITYCLASS_BELOW_NORMAL
        kernel32 = ctypes.WinDLL("kernel32")
        gdi32 = ctypes.WinDLL("gdi32")
        kernel32.GetCurrentProcess.restype = ctypes.c_void_p
        gdi32.D3DKMTSetProcessSchedulingPriorityClass.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
        ]
        gdi32.D3DKMTSetProcessSchedulingPriorityClass.restype = ctypes.c_long
        status = gdi32.D3DKMTSetProcessSchedulingPriorityClass(
            kernel32.GetCurrentProcess(), below_normal
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
        print(f"gpu_polite: WDDM GPU priority call failed (non-fatal): {e}", file=sys.stderr)


def _gpu_mem_fraction():
    """LYRICS_GPU_MEM_FRACTION env → clamped float in [0.2, 0.95], default 0.7."""
    try:
        frac = float(os.environ.get("LYRICS_GPU_MEM_FRACTION", "0.7"))
    except (TypeError, ValueError):
        frac = 0.7
    return min(0.95, max(0.2, frac))


def gpu_polite(force_cpu=False):
    """GPU discipline for the shared win-resolume box (#154): BELOW_NORMAL WDDM
    priority + a per-process VRAM cap. Model parameters are UNCHANGED — quality
    is identical; only scheduling priority + VRAM headroom move. Best-effort.

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
    """True if `exc` is a CUDA out-of-memory error (typed or by message)."""
    import torch

    return isinstance(exc, torch.cuda.OutOfMemoryError) or (
        "out of memory" in str(exc).lower()
    )


@contextlib.contextmanager
def _force_cpu():
    """Temporarily make torch report no CUDA device so audio-separator builds the
    model on CPU for the OOM-retry run (#154). Same model + parameters ⇒
    identical output, only slower. Restored on exit."""
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


def _free_vram(sep):
    """Drop separator state so VRAM is released."""
    import torch

    if hasattr(sep, "model_instance"):
        sep.model_instance = None
    del sep
    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()


def _load_48k_stereo(src_path):
    """Load a separated stem, resample to 48 kHz STEREO, peak-clamp to [-1, 1].
    Mono sources are duplicated to two channels so every stem matches the mix's
    layout. Returns an (n, 2) float32 array."""
    import librosa
    import numpy as np

    y, _ = librosa.load(src_path, sr=OUTPUT_SAMPLE_RATE, mono=False)
    if y.ndim == 1:
        y = np.stack([y, y], axis=0)
    elif y.shape[0] == 1:
        y = np.repeat(y, 2, axis=0)
    elif y.shape[0] > 2:
        y = y[:2, :]
    return np.clip(y.T, -1.0, 1.0).astype(np.float32)  # (n, ch)


def _write_array_48k_stereo(out_array, out_path):
    """Write an (n, 2) 48 kHz array to a FLAC ATOMICALLY (#14): write a sibling
    `.tmp` then `os.replace` it into place (atomic on the same filesystem, POSIX
    + Windows), so a killed subprocess never leaves a HALF-WRITTEN FLAC at the
    final sidecar path — the live playback reader keys on the sidecar's
    existence, and a torn file there would corrupt the wall's NDI audio."""
    import numpy as np
    import soundfile as sf

    out = np.clip(out_array, -1.0, 1.0)
    tmp_path = f"{out_path}.tmp"
    try:
        # format= is REQUIRED: the atomic temp path ends in ".tmp".
        sf.write(tmp_path, out, OUTPUT_SAMPLE_RATE, format="FLAC", subtype="PCM_24")
        os.replace(tmp_path, out_path)
    finally:
        if os.path.exists(tmp_path):
            with contextlib.suppress(OSError):
                os.remove(tmp_path)


def _separate_one_segment(sep, full, in_sr, start_s, end_s, segv_path, segi_path, stem_dir):
    """Separate ONE native-rate window `[start_s, end_s]` of `full` into vocals +
    instrumental, resample each to 48 kHz stereo, and write them atomically
    (WAV scratch) to `segv_path` / `segi_path`. `stem_dir` is cleared after."""
    import numpy as np
    import soundfile as sf

    s0 = max(0, int(round(start_s * in_sr)))
    s1 = int(round(end_s * in_sr))
    data = full[s0:s1] if full.ndim == 1 else full[:, s0:s1].T  # (n[, ch])
    seg_in = os.path.join(stem_dir, "segin_" + os.path.basename(segv_path))
    sf.write(seg_in, data, in_sr, subtype="FLOAT")

    out_files = sep.separate(seg_in)
    vocals = _pick(out_files, {"vocals"}, stem_dir)
    instrumental = _pick(
        out_files, {"instrumental", "other", "no_vocals", "accompaniment"}, stem_dir
    )
    v = _load_48k_stereo(vocals)
    i = _load_48k_stereo(instrumental)
    for arr, path in ((v, segv_path), (i, segi_path)):
        tmp = path + ".tmp"
        sf.write(tmp, arr, OUTPUT_SAMPLE_RATE, format="WAV", subtype="FLOAT")
        os.replace(tmp, path)

    for f in os.listdir(stem_dir):
        with contextlib.suppress(OSError):
            os.remove(os.path.join(stem_dir, f))


def cmd_separate(args):
    """Kim two-stem separation → vocals + instrumental FLAC @ 48 kHz stereo, done
    RESUMABLY per segment (#171).

    The mix is split into `STEM_SEGMENT_SECONDS` windows; each is separated and
    its two 48 kHz stereo stems written to `--work-dir`. Segments already present
    are SKIPPED on start (a killed/timed-out run resumes), the model loads once
    and is reused across every remaining segment, and the final stems are stitched
    (2 s linear crossfade, IDENTICAL weights on both stems so additivity holds)
    and written atomically to `--vocals-out` / `--instrumental-out`, then the work
    dir is removed. On a CUDA OOM the remaining segments re-run on CPU (#154).
    """
    import numpy as np
    import librosa
    from audio_separator.separator import Separator

    if args.force_cpu:
        print("stem_worker: forced CPU inference (--force-cpu)", file=sys.stderr)
    gpu_polite(force_cpu=args.force_cpu)

    work_dir = args.work_dir
    os.makedirs(work_dir, exist_ok=True)

    full, in_sr = librosa.load(args.audio, sr=None, mono=False)
    total_samples = full.shape[0] if full.ndim == 1 else full.shape[1]
    total_s = total_samples / float(in_sr)
    bounds = _segment_bounds(total_s, STEM_SEGMENT_SECONDS, STEM_OVERLAP_SECONDS)
    n_seg = len(bounds)

    def _segv(i):
        return os.path.join(work_dir, f"segv_{i:04d}_of_{n_seg:04d}.wav")

    def _segi(i):
        return os.path.join(work_dir, f"segi_{i:04d}_of_{n_seg:04d}.wav")

    def _seg_done(i):
        return (
            os.path.exists(_segv(i)) and os.path.getsize(_segv(i)) > 0
            and os.path.exists(_segi(i)) and os.path.getsize(_segi(i)) > 0
        )

    done = [_seg_done(i) for i in range(n_seg)]
    if any(done) and not all(done):
        print(f"separation resumed from chunk {sum(done)}/{n_seg}", file=sys.stderr)

    def _process_remaining(force_cpu):
        stem_dir = tempfile.mkdtemp(prefix="sp_karaoke_")
        cpu_ctx = _force_cpu() if force_cpu else contextlib.nullcontext()
        try:
            with cpu_ctx:
                # use_soundfile=True avoids pydub's OOM on long 24-bit stems
                # (pydub#135; 827-s song 2026-09-12). No dereverb — karaoke wants
                # the natural instrumental, reverb tail included.
                sep = Separator(
                    model_file_dir=args.models_dir,
                    output_format="WAV",
                    output_dir=stem_dir,
                    use_soundfile=True,
                )
                sep.load_model(KARAOKE_STEM_MODEL)
                for i, (s_s, e_s) in enumerate(bounds):
                    if done[i]:
                        continue
                    _separate_one_segment(
                        sep, full, in_sr, s_s, e_s, _segv(i), _segi(i), stem_dir
                    )
                    done[i] = True
                    print(f"separation chunk {i + 1}/{n_seg} done", file=sys.stderr)
                _free_vram(sep)
        finally:
            shutil.rmtree(stem_dir, ignore_errors=True)

    if not all(done):
        import torch

        try:
            _process_remaining(force_cpu=args.force_cpu)
        except Exception as e:
            # #162: the CUDA-OOM→CPU retry is for the GPU path only. A forced-CPU
            # run has no GPU to fall back from, so a failure there is a real error.
            if args.force_cpu or not _is_cuda_oom(e):
                raise
            print(
                "gpu_polite: CUDA OOM during stem separation — retrying on CPU "
                "(identical model + parameters, only slower) [#154]",
                file=sys.stderr,
            )
            gc.collect()
            if torch.cuda.is_available():
                torch.cuda.empty_cache()
            _process_remaining(force_cpu=True)

    # Stitch both stems with IDENTICAL crossfade weights (preserves additivity).
    step_samples = int(round((STEM_SEGMENT_SECONDS - STEM_OVERLAP_SECONDS) * OUTPUT_SAMPLE_RATE))
    overlap_samples = int(round(STEM_OVERLAP_SECONDS * OUTPUT_SAMPLE_RATE))
    import soundfile as sf

    def _stitch_paths(path_of):
        segs = [sf.read(path_of(i), dtype="float32")[0] for i in range(n_seg)]
        return _stitch_segments(segs, step_samples, overlap_samples)

    _write_array_48k_stereo(_stitch_paths(_segv), args.vocals_out)
    _write_array_48k_stereo(_stitch_paths(_segi), args.instrumental_out)
    shutil.rmtree(work_dir, ignore_errors=True)

    print(json.dumps({"vocals": args.vocals_out, "instrumental": args.instrumental_out}))


def cmd_preload(args):
    """Warm the Kim model at bootstrap so a download failure surfaces before any
    real song is processed."""
    from audio_separator.separator import Separator

    sep = Separator(model_file_dir=args.models_dir, output_format="FLAC")
    sep.load_model(KARAOKE_STEM_MODEL)
    _free_vram(sep)
    print(json.dumps({"loaded": True, "model": KARAOKE_STEM_MODEL}))


def main():
    parser = argparse.ArgumentParser(description="SongPlayer karaoke stem separator")
    subparsers = parser.add_subparsers(dest="command", required=True)

    p_sep = subparsers.add_parser("separate")
    p_sep.add_argument("--audio", required=True)
    p_sep.add_argument("--vocals-out", required=True)
    p_sep.add_argument("--instrumental-out", required=True)
    p_sep.add_argument("--models-dir", required=True)
    # #171: per-segment scratch dir for resumable separation. Each segment's two
    # stems are written here and skipped on resume; removed once the final stems
    # land.
    p_sep.add_argument("--work-dir", required=True)
    # #162: force in-process CPU inference from the start (leaves the GPU
    # untouched for the live wall) instead of only as the CUDA-OOM fallback.
    p_sep.add_argument("--force-cpu", action="store_true")

    p_pre = subparsers.add_parser("preload")
    p_pre.add_argument("--models-dir", required=True)

    args = parser.parse_args()
    dispatch = {
        "separate": cmd_separate,
        "preload": cmd_preload,
    }
    try:
        dispatch[args.command](args)
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
