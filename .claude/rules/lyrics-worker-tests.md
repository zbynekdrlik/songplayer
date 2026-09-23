---
paths:
  - "scripts/lyrics_worker.py"
  - "scripts/tests/**"
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
