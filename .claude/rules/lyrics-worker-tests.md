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

There is **NO `torch`, `librosa`, `audio_separator`, or `qwen_asr`** in that
environment (they are heavy GPU/model deps that only live in `lyrics_venv` on the
box). So a `scripts/tests/` test must run on **numpy + soundfile only**.

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
- **`qwen_asr.Qwen3ForcedAligner`**: a fake with a `from_pretrained` staticmethod
  recording the model name.

Use `monkeypatch.setitem(sys.modules, "<name>", fake)` so the injection auto-undoes
between tests. FLAC round-trips through the bundled libsndfile in the job, so a
synthetic `.flac` input via `soundfile.write` is fine.

## Structural guards live in Rust (`aligner_tests_timeout.rs`, `gpu_policy_script_guard_tests.rs`)

Some invariants of `lyrics_worker.py` are asserted by `include_str!`-ing the
script into a Rust test and matching substrings (e.g. `use_soundfile=True` count,
`gpu_polite(` present, no `from_secs(600)`). When you change the script's model
structure, UPDATE those counts too — they are CRLF-normalised and body-scoped.

## `stem_worker.py` tests follow the same pattern (#207)

`scripts/tests/test_stem_streaming.py` loads `stem_worker.py` by path and fakes
torch / librosa / audio_separator the same way. It drives `cmd_separate`
end to end with a real soundfile mix. Its gotchas:

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
