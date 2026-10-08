"""Audio windows and the streamed overlap-add, shared by the GPU workers.

#207 (and the #233 release review, which found the lyrics worker's copy of
the stem worker's helpers): `stem_worker.py` and `lyrics_worker.py` both read
a long sidecar one window at a time and stitch the processed windows back
without ever holding the whole song. This module is that one implementation:

- `audio_info`: a file's rate and frame count from its header, refusing an
  unknown or empty length;
- `read_window`: one native-rate window straight from the file;
- `OverlapAdd`: the streamed overlap-add — the SAME samples as the workers'
  whole-array reference stitch (`_stitch_segments`), settled block by block,
  holding only the overlap tail.

Each worker ships it next to its own script (the Rust side writes both into
the tools dir), like `win_replace.py`.
"""

# A header frame count at or above this is libsndfile's "unknown length"
# sentinel (e.g. a FLAC from a piped encoder has STREAMINFO total samples = 0).
UNKNOWN_FRAMES = 1 << 62


def audio_info(path):
    """(sample_rate, frames) of an audio file, from its header — no samples
    are read. The segment plan trusts this count, so an unknown or empty one
    raises a clear ValueError instead of planning a bogus number of windows."""
    import soundfile as sf

    info = sf.info(path)
    if info.samplerate <= 0 or info.frames <= 0 or info.frames >= UNKNOWN_FRAMES:
        raise ValueError(
            f"unusable header in {path}: frame count {info.frames} "
            f"at {info.samplerate} Hz (unknown or empty length)"
        )
    return info.samplerate, info.frames


def read_window(path, in_sr, start_s, end_s):
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


class OverlapAdd:
    """The streamed overlap-add of `n_segments` segments, segment `i`
    starting at global sample `i * step_samples`: the same arithmetic as the
    reference — accumulated in float64 in segment order with the same linear
    crossfade weights over `overlap_samples`, weight-normalised, cast to
    float32 — settled block by block.

    `add(segment)` adds the next segment (`(n,)` or `(n, ch)`) and returns
    every sample no later segment can still touch (everything before the next
    segment's start; the rest of the song after the last), as float32. Only
    the overlap tail stays held (`retained_samples`, its peak
    `max_retained_samples`). An earlier segment that ends past the last one
    raises ValueError, as the reference's broadcast does."""

    def __init__(self, n_segments, step_samples, overlap_samples):
        import numpy as np

        self._np = np
        self.n_segments = n_segments
        self.step_samples = step_samples
        self.overlap_samples = overlap_samples
        self.added = 0
        # Global sample index of acc[0] == samples already settled.
        self._base = 0
        self._acc = None
        self._wsum = np.zeros(0, dtype=np.float64)
        self.retained_samples = 0
        self.max_retained_samples = 0

    def _weights(self, i, length):
        np = self._np
        w = np.ones(length, dtype=np.float64)
        if self.overlap_samples > 0:
            f = min(self.overlap_samples, length)
            if i > 0:
                w[:f] = np.linspace(0.0, 1.0, f, endpoint=False)
            if i < self.n_segments - 1:
                w[length - f :] = np.linspace(1.0, 0.0, f, endpoint=False)
        return w

    def _grow_to(self, end, tail_shape):
        """Extend the accumulator (zero-filled) to cover global sample `end`."""
        np = self._np
        if self._acc is None:
            self._acc = np.zeros((0, *tail_shape), dtype=np.float64)
        extra = end - self._base - self._wsum.shape[0]
        if extra <= 0:
            return
        self._acc = np.concatenate(
            [self._acc, np.zeros((extra, *tail_shape), dtype=np.float64)]
        )
        self._wsum = np.concatenate([self._wsum, np.zeros(extra, dtype=np.float64)])

    def add(self, segment):
        np = self._np
        i = self.added
        if i >= self.n_segments:
            raise RuntimeError(f"more than the declared {self.n_segments} segments")
        seg = np.asarray(segment, dtype=np.float64)
        length = seg.shape[0]
        start = i * self.step_samples
        end = start + length
        last = i == self.n_segments - 1
        held_end = self._base + self._wsum.shape[0]
        if last and held_end > end:
            # The reference sizes its output by the LAST segment; an earlier
            # segment reaching past it does not fit there either.
            raise ValueError(
                f"an earlier segment ends past the last one ({held_end} > {end} samples)"
            )
        # Invariant: everything before this segment's start is settled.
        assert start >= self._base, (start, self._base)
        settle = end if last else (i + 1) * self.step_samples
        self._grow_to(max(end, settle), seg.shape[1:])
        w = self._weights(i, length)
        a = start - self._base
        self._acc[a : a + length] += seg * (w[:, None] if seg.ndim == 2 else w)
        self._wsum[a : a + length] += w
        self.added += 1
        k = settle - self._base
        block = self._acc[:k].copy()
        nz = self._wsum[:k] > 1e-9
        if block.ndim == 2:
            block[nz] /= self._wsum[:k][nz][:, None]
        else:
            block[nz] /= self._wsum[:k][nz]
        # Copy so the settled head is actually released.
        self._acc = self._acc[k:].copy()
        self._wsum = self._wsum[k:].copy()
        self._base = settle
        self.retained_samples = self._wsum.shape[0]
        self.max_retained_samples = max(
            self.max_retained_samples, self.retained_samples
        )
        return block.astype(np.float32)
