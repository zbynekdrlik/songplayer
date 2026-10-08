---
paths:
  - "scripts/lyrics_worker.py"
  - "scripts/tests/**"
  - "scripts/stem_worker.py"
---

# Testing `scripts/lyrics_worker.py` in the `eval-checks` CI job

The `Eval Checks (ruff + pytest)` job runs `pytest scripts/tests` with a
DELIBERATELY MINIMAL install (`.github/workflows/ci.yml`):

```
pip install ruff pytest jsonschema requests numpy soundfile
```

There is **NO `torch`, `librosa` or `audio_separator`** in that environment
(they are heavy GPU/model deps that only live in `lyrics_venv` on the box). So a
`scripts/tests/` test must run on **numpy + soundfile only**.

## How the module stays testable

`lyrics_worker.py` keeps every heavy import INSIDE its command functions
(`import torch`, `from audio_separator.separator import Separator`,
`import librosa` are all local to `cmd_*` / `_*` helpers). The module BODY is
import-safe, so `importlib.util.spec_from_file_location(...)` loads it without
pulling any heavy dep (see `test_segment_stitch.py::_load_lyrics_worker`).

## Testing a command function (e.g. `cmd_preprocess_vocals`, `cmd_preload`)

Inject FAKE modules into `sys.modules` BEFORE calling the command — the local
imports then resolve to the fakes. `numpy`/`soundfile` stay REAL (they are in the
job), so file I/O + segmentation + stitch are exercised for real; only the model
stack is faked. Pattern (see `test_lyrics_worker_vocals_in.py`):

- **`torch`**: a `ModuleType` with `bfloat16 = object()` and a
  `cuda = SimpleNamespace(is_available=lambda: False, device_count=…,
  empty_cache=…, set_per_process_memory_fraction=…, OutOfMemoryError=<a class>)`.
  `is_available()==False` makes `gpu_polite`/`_free_vram`/`_force_cpu` no-op the
  GPU paths.
- **`audio_separator.separator.Separator`**: a fake whose `.separate(path)` writes
  a real WAV (via real soundfile) and returns its path; record `load_model(name)`
  calls so a test can assert WHICH models were warmed. Register BOTH
  `audio_separator` and `audio_separator.separator` in `sys.modules`.
- **`librosa.load`**: back it with real `soundfile.read` + a numpy linear
  resample — enough to exercise the 16 kHz-resample plumbing without real librosa.
- **The retired Qwen aligner (#144):** the script no longer imports it.
  `test_preload_warms_only_the_dereverb_model` puts a `None` entry for its
  package in `sys.modules` (any import of it then raises), and
  `test_the_script_has_no_retired_aligner` greps the script. Those tests split
  the names (`"qwen" + "_asr"`): the CI deletion audit greps `scripts/` for
  them.

Use `monkeypatch.setitem(sys.modules, "<name>", fake)` so the injection auto-undoes
between tests. FLAC round-trips through the bundled libsndfile in the job, so a
synthetic `.flac` input via `soundfile.write` is fine.

## Structural guards live in Rust (`aligner_tests_timeout.rs`, `gpu_policy_script_guard_tests.rs`)

Some invariants of `lyrics_worker.py` are asserted by `include_str!`-ing the
script into a Rust test and matching substrings (e.g. `use_soundfile=True` count,
`gpu_polite(` present, no `from_secs(600)`). When you change the script's model
structure, UPDATE those counts too — they are CRLF-normalised and body-scoped.

## `preprocess-vocals` streams too (#207, `test_lyrics_worker_streaming.py`)

The same design as the stems: the vocals sidecar is read one window at a
time (`_audio_info` header + `_read_window`, exactly the old
`librosa.load(sr=None, mono=False)` slice), and the 16 kHz segments are
stitched by `_stitched_blocks` (one block per segment read, only the
overlap tail kept) in TWO passes in `_stitch_to_wav`: the first takes the
global peak, the second writes `<out>.tmp` (divided by the peak over 1.0)
and `os.replace`s it. `_stitch_segments` stays as the reference. The tests:

- the fake `librosa.load` refuses the vocals path, and `_stitch_segments`
  is a trap in the end-to-end test (a whole-array path makes identical
  samples, so only a trap catches a regression);
- each dereverb input equals the old whole-file slice exactly, and the
  output equals the reference stitch of the very segments written
  (captured through `_atomic_write_wav`);
- the streamed stitch equals the reference + normalisation + writer bit
  for bit (FLOAT WAV), peaks under and over 1.0, a gap, a short last
  segment; reads and blocks alternate one to one;
- local measure (tracemalloc, the fakes, 30 s / 2 s windows): a 12 min
  sidecar peaked at 678 MiB before, 29 MiB after, and 29 MiB at 3 min too.

`_stitched_blocks` handles any segment lengths the reference does (a gap,
an overlap longer than the step); an earlier segment ending past the last
one raises, like the reference's broadcast. Do not name a helper
`*segment_size*`: `gpu_policy_script_guard_tests.rs` bans that substring in
the script.

## `stem_worker.py` tests follow the same pattern (#207)

`scripts/tests/test_stem_streaming.py` loads `stem_worker.py` by path and fakes
torch / librosa / audio_separator the same way (`scripts/tests/stem_fakes.py`,
shared with `test_stem_publish.py`). It drives `cmd_separate` end to end with
a real soundfile mix. Its gotchas:

- **Trap, don't trust.** Make the fake `librosa.load` RAISE on the mix path,
  and monkeypatch the whole-array reference functions (`_stitch_segments`,
  `_write_array_48k_stereo`) to raise. A streaming regression then fails
  loudly; checking the output values alone cannot catch it, because a
  whole-array path produces identical samples.
- **Order spies catch "read all, then write".** Record the order of segment
  reads and writer adds, and assert they alternate 1:1.
- **libsndfile cannot read an EMPTY FLAC** (`Format not recognised`), so an
  "empty file" case is already an error at `sf.info`.
- **An unknown-length FLAC is easy to fake:** zero the 36-bit STREAMINFO
  `total samples` field (bytes 18..25, the low 36 bits).
  `sf.info().frames` then reports the ~2^63 sentinel
  (`_flac_with_total_samples` in the test).
- **The CI ruff list must include every script you change.** It is not the
  whole of `scripts/` (`.github/workflows/ci.yml` "Eval Checks"). From a
  worktree lane, run that list from a small script file: the worktree Bash
  guard rejects a command line containing the `eval/` path.
