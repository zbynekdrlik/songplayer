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


def gpu_polite():
    """GPU discipline for the shared win-resolume box (#154): BELOW_NORMAL WDDM
    priority + a per-process VRAM cap. Model parameters are UNCHANGED — quality
    is identical; only scheduling priority + VRAM headroom move. Best-effort."""
    _set_wddm_gpu_priority()
    try:
        import torch

        if torch.cuda.is_available():
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
    os.environ["CUDA_VISIBLE_DEVICES"] = ""
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


def _write_stem_48k_stereo(src_path, out_path):
    """Load a separated stem, resample to 48 kHz STEREO, peak-clamp to [-1, 1],
    and write a FLAC. Keeps stereo (mono=False); mono sources are duplicated to
    two channels so every stem matches the mix's channel layout.

    ATOMIC (#14): write to a sibling temp file first, then `os.replace` it into
    place. `os.replace` is atomic on the same filesystem (POSIX + Windows), so a
    killed/timed-out subprocess (`kill_on_drop` on a server restart) never leaves
    a HALF-WRITTEN FLAC at the FINAL sidecar path — the live playback reader keys
    on the sidecar's existence, and a torn file there would corrupt the wall's
    NDI audio. A crash leaves only the discardable `.tmp` beside it."""
    import librosa
    import numpy as np
    import soundfile as sf

    # mono=False keeps the channel dimension; librosa returns shape (ch, n) for
    # multi-channel or (n,) for mono.
    y, _ = librosa.load(src_path, sr=OUTPUT_SAMPLE_RATE, mono=False)
    if y.ndim == 1:
        y = np.stack([y, y], axis=0)  # duplicate mono → stereo
    elif y.shape[0] == 1:
        y = np.repeat(y, 2, axis=0)
    elif y.shape[0] > 2:
        y = y[:2, :]  # keep the first two channels
    # (n, ch) for soundfile; clamp to avoid FLAC integer clipping.
    out = np.clip(y.T, -1.0, 1.0)
    tmp_path = f"{out_path}.tmp"
    try:
        sf.write(tmp_path, out, OUTPUT_SAMPLE_RATE, subtype="PCM_24")
        os.replace(tmp_path, out_path)  # atomic on the same filesystem
    finally:
        # If os.replace never ran (write failed), don't leave the temp behind.
        if os.path.exists(tmp_path):
            with contextlib.suppress(OSError):
                os.remove(tmp_path)


def cmd_separate(args):
    """Kim two-stem separation → vocals + instrumental FLAC @ 48 kHz stereo.

    Exits 0 on success and prints {"vocals": ..., "instrumental": ...}.
    On a CUDA OOM the whole separation re-runs on CPU (#154).
    """
    from audio_separator.separator import Separator

    gpu_polite()

    stem_dir = tempfile.mkdtemp(prefix="sp_karaoke_")

    def _separate(force_cpu):
        cpu_ctx = _force_cpu() if force_cpu else contextlib.nullcontext()
        with cpu_ctx:
            # use_soundfile=True avoids pydub's OOM on long 24-bit stems
            # (pydub#135; observed on an 827-s song 2026-09-12). No dereverb —
            # karaoke wants the natural instrumental, reverb tail included.
            sep = Separator(
                model_file_dir=args.models_dir,
                output_format="FLAC",
                output_dir=stem_dir,
                use_soundfile=True,
            )
            sep.load_model(KARAOKE_STEM_MODEL)
            out_files = sep.separate(args.audio)
            vocals = _pick(out_files, {"vocals"}, stem_dir)
            instrumental = _pick(
                out_files, {"instrumental", "other", "no_vocals", "accompaniment"}, stem_dir
            )
            _free_vram(sep)
        return vocals, instrumental

    try:
        import torch

        try:
            vocals, instrumental = _separate(force_cpu=False)
        except Exception as e:
            if not _is_cuda_oom(e):
                raise
            print(
                "gpu_polite: CUDA OOM during stem separation — retrying on CPU "
                "(identical model + parameters, only slower) [#154]",
                file=sys.stderr,
            )
            gc.collect()
            if torch.cuda.is_available():
                torch.cuda.empty_cache()
            vocals, instrumental = _separate(force_cpu=True)

        _write_stem_48k_stereo(vocals, args.vocals_out)
        _write_stem_48k_stereo(instrumental, args.instrumental_out)
    finally:
        shutil.rmtree(stem_dir, ignore_errors=True)

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
