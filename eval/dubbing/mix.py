#!/usr/bin/env python3
"""mix.py — build the dub track from placed clips and produce the three listening
variants over the ambient stem.

Three variants per engine (epic #174 sermon-dubbing spec, §5):

  1. **dub_only**       — the Slovak dub track + the ambient (non-voice) stem.
  2. **dub_plus_voice** — the dub track + the original English voice at -12 dB
                          + the ambient stem (the operator-mix preview).
  3. **original_only**  — the original English voice + the ambient stem (control).

All three are mixed over the SAME ambient stem so the listener compares like for
like. Sums are clamped to [-1, 1]. Uses numpy + soundfile (present in the
`eval-checks` deps); this module runs at RUN time on dev1, never in CI (the CI
`eval-checks` job only exercises the pure `fit.py` via `tests/test_fit.py`).
"""

from __future__ import annotations

import argparse

import numpy as np
import soundfile as sf

VOICE_UNDER_DB = -12.0


def db_to_gain(db: float) -> float:
    return float(10.0 ** (db / 20.0))


def to_stereo(arr: np.ndarray) -> np.ndarray:
    """Return a float32 (n, 2) array. Mono is duplicated to both channels; more
    than 2 channels are downmixed to the first two."""
    a = np.asarray(arr, dtype=np.float32)
    if a.ndim == 1:
        return np.stack([a, a], axis=1)
    if a.shape[1] == 1:
        return np.repeat(a, 2, axis=1)
    return a[:, :2]


def _read_stereo(path: str, target_sr: int) -> np.ndarray:
    audio, sr = sf.read(path, dtype="float32", always_2d=True)
    if sr != target_sr:
        raise RuntimeError(
            f"{path}: sample rate {sr} != target {target_sr}; resample before mixing"
        )
    return to_stereo(audio)


def ms_to_samples(ms: int, sr: int) -> int:
    return int(round(ms * sr / 1000.0))


def build_dub_track(
    clips: list[tuple[int, np.ndarray]], sr: int, total_ms: int
) -> np.ndarray:
    """Lay each (at_ms, clip) onto a silent stereo buffer of `total_ms` by
    overlap-add. Clips that would run past the buffer end are truncated."""
    total = ms_to_samples(total_ms, sr)
    track = np.zeros((total, 2), dtype=np.float32)
    for at_ms, clip in clips:
        c = to_stereo(clip)
        start = ms_to_samples(at_ms, sr)
        if start >= total:
            continue
        end = min(total, start + c.shape[0])
        track[start:end] += c[: end - start]
    return track


def _fit_len(a: np.ndarray, n: int) -> np.ndarray:
    """Pad with silence or truncate `a` to exactly `n` frames."""
    if a.shape[0] == n:
        return a
    if a.shape[0] > n:
        return a[:n]
    pad = np.zeros((n - a.shape[0], a.shape[1]), dtype=np.float32)
    return np.concatenate([a, pad], axis=0)


def clamp(a: np.ndarray) -> np.ndarray:
    return np.clip(a, -1.0, 1.0)


def build_variants(
    dub_track: np.ndarray,
    voice_stem: np.ndarray,
    ambient_stem: np.ndarray,
    voice_under_db: float = VOICE_UNDER_DB,
) -> dict[str, np.ndarray]:
    """Return the three mix variants, each clamped to [-1, 1] and the same
    length (the longest input)."""
    dub = to_stereo(dub_track)
    voice = to_stereo(voice_stem)
    ambient = to_stereo(ambient_stem)
    n = max(dub.shape[0], voice.shape[0], ambient.shape[0])
    dub, voice, ambient = _fit_len(dub, n), _fit_len(voice, n), _fit_len(ambient, n)
    under = db_to_gain(voice_under_db)
    return {
        "dub_only": clamp(dub + ambient),
        "dub_plus_voice": clamp(dub + under * voice + ambient),
        "original_only": clamp(voice + ambient),
    }


def _cli(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Build the three dub mix variants.")
    p.add_argument("--dub-track", required=True, help="pre-built dub track WAV")
    p.add_argument("--voice", required=True, help="original voice stem WAV")
    p.add_argument("--ambient", required=True, help="ambient/instrumental stem WAV")
    p.add_argument("--sr", type=int, required=True, help="working sample rate")
    p.add_argument("--out-prefix", required=True, help="output path prefix")
    p.add_argument("--voice-db", type=float, default=VOICE_UNDER_DB)
    args = p.parse_args(argv)

    dub = _read_stereo(args.dub_track, args.sr)
    voice = _read_stereo(args.voice, args.sr)
    ambient = _read_stereo(args.ambient, args.sr)
    variants = build_variants(dub, voice, ambient, voice_under_db=args.voice_db)
    for name, audio in variants.items():
        out = f"{args.out_prefix}_{name}.wav"
        sf.write(out, audio, args.sr)
        print(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
