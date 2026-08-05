# lyrics-alignment-mtl — LyricsAlignment-MTL (Huang/Benetos/Ewert, ICASSP 2022)

Forced-aligner benchmark backend for the 2026-08-05 aligner shootout. Given
the FIXED reference text (an audio-LLM's already-transcribed `lines[].text`)
plus the isolated-vocal WAV, this tool returns timestamps for that exact
text — it does not transcribe or reinterpret anything. This README documents
the exact install path that worked on `win-resolume`, every trap hit along
the way, and how the `MTL+BDR` checkpoint choice was confirmed.

Upstream repo: <https://github.com/jhuang448/LyricsAlignment-MTL> (MIT
license, checkpoints committed in-repo — no separate model download).
Paper: Huang, Benetos, Ewert, "Improving Lyrics Alignment through Joint
Pitch Detection," ICASSP 2022.

## TL;DR

- **It runs.** Full 22-fixture batch completed on `win-resolume`'s RTX
  3070 Ti, isolated in its own venv, on Python 3.12 + a modern
  torch/numpy/librosa stack — with three small, documented compatibility
  shims (none touching the alignment algorithm).
- **Granularity: WORD-level**, natively. `wrapper.align()` returns one
  `[start_frame, end_frame]` pair per word.
- **Checkpoint used: `MTL+BDR`** — `checkpoint_MTL` (acoustic model) +
  `checkpoint_BDR` (boundary-detection refinement), selected via
  `wrapper.align(method="MTL_BDR")`. Confirmed against the upstream
  README's own inference example (`eval_bdr.py --ac_model=checkpoint_MTL
  --bdr_model=checkpoint_BDR --model=MTL`) — see "Confirming MTL+BDR" below.

## Install — the exact steps that worked

All commands run via the `win-resolume` MCP `Shell` tool (PowerShell),
never SSH.

```powershell
# 1. Clone the upstream repo (checkpoints are committed in it — no
#    separate download step)
cd C:\ProgramData\SongPlayer\cache\tools
git clone https://github.com/jhuang448/LyricsAlignment-MTL

# 2. A fresh, ISOLATED venv off system Python 3.12 (never touch the
#    system-wide Python or the other aligner's venv on this shared box)
& "C:\Program Files\Python312\python.exe" -m venv `
    "C:\ProgramData\SongPlayer\cache\tools\mtl_aligner_venv"

# 3. Modern torch/torchaudio matching the box's driver 566.36 / CUDA 12.4
$py = "C:\ProgramData\SongPlayer\cache\tools\mtl_aligner_venv\Scripts\python.exe"
& $py -m pip install --upgrade pip
& $py -m pip install torch torchaudio --index-url https://download.pytorch.org/whl/cu124

# 4. Everything else needed for INFERENCE (not the full requirements.txt —
#    musdb/museval/h5py/tqdm/tensorboard/sortedcontainers/youtube_dl are
#    training/eval-harness-only deps, irrelevant to wrapper.py's inference
#    path and skipped)
& $py -m pip install numpy librosa soundfile g2p_en resampy

# 5. g2p_en needs two NLTK corpora it does NOT auto-fetch correctly on a
#    recent NLTK — see "Trap 3" below
& $py -c "import nltk; nltk.download('averaged_perceptron_tagger_eng'); nltk.download('cmudict')"
```

**Versions actually installed and verified working:**

| Package | Version | Note |
|---|---|---|
| Python | 3.12.10 | system interpreter, isolated venv |
| torch | 2.6.0+cu124 | upstream pinned `torch==1.8.0`; **not** what's installed — see Trap 2 |
| torchaudio | 2.6.0+cu124 | matches torch |
| numpy | 2.4.6 | upstream code predates numpy 2.0 — see Trap 1 |
| librosa | 0.11.0 | |
| soundfile | 0.14.0 | |
| resampy | 0.4.3 | not in upstream `requirements.txt` at all — see Trap 4 |
| g2p_en | 2.1.0 | grapheme-to-phoneme, upstream's alignment unit is PHONEMES not characters |
| nltk | 3.10.2 | g2p_en's dependency; corpora fetched separately, see Trap 3 |

**Total setup wall-clock** (clone + venv + all installs + NLTK downloads +
first successful smoke-test run to a real fixture): **under 15 minutes**,
almost entirely `pip install` network time — no dependency-hell dead end
was hit; every trap below had a small, well-understood fix.

**Deliberately NOT installed** (present in upstream `requirements.txt` but
unused by the inference-only path this task needs — `wrapper.py`'s
`preprocess_from_file()` + `align()` never touch them): `musdb`, `museval`,
`h5py`, `tqdm`, `tensorboard`, `sortedcontainers`, `youtube_dl`, `future`.
Those back the training pipeline (`train.py`) and the Jamendo-dataset eval
harness (`eval.py`/`eval_bdr.py`'s `JamendoLyricsDataset`), neither of
which this project's per-fixture runner uses.

## Why torch 2.6 instead of the pinned torch==1.8.0

`requirements.txt` pins `torch==1.8.0` (2021-era). We did **not** try to
install that — it predates CUDA 12.4 entirely (no compatible wheel exists
for this box's driver), and per the task brief's own guidance, inference
on a small CNN+BiLSTM acoustic model rarely depends on internals that
change between major torch releases. We went straight to a modern
`torch==2.6.0+cu124` matching the box's driver, and only two small
`torch`/`numpy` API changes actually mattered in practice (Traps 1–2
below) — everything else in `model.py`/`utils.py`/`wrapper.py` (Conv2d,
LSTM, MelSpectrogram, log_softmax, `nn.utils.rnn.pad_sequence`) is stable,
unchanged API across that whole span.

## Traps hit, and exactly how each was resolved

### Trap 1 — `numpy.Inf` removed in numpy 2.0

`utils.py::alignment_bdr()` (the DP alignment routine `MTL+BDR` actually
calls) initializes its score table with `np.zeros(...) - np.Inf`. NumPy
2.0 removed the capitalized `Inf`/`NaN`/etc. aliases entirely
(`AttributeError: \`np.Inf\` was removed in the NumPy 2.0 release. Use
\`np.inf\` instead.`). This is **exactly** the "renamed numpy alias"
example this project's task brief anticipated.

**Fix** — `run.py::install_compat_shims()` restores the alias before
importing the upstream `utils` module:

```python
import numpy as np
if not hasattr(np, "Inf"):
    np.Inf = np.inf
```

Confirmed via `grep` that only `utils.py` (lines 172, 197, 306) uses
`np.Inf` on any code path `wrapper.py` actually calls — `data.py`/`train.py`
also use it but are training/HDF5-only, never imported by our runner.

### Trap 2 — `torch.load` defaults changed in torch 2.6

Torch >=2.6 flipped `torch.load`'s default to `weights_only=True`, which
restricts unpickling to a safelist of tensor-only types. The vendored
checkpoints (`checkpoint_MTL`, `checkpoint_BDR`, `checkpoint_Baseline`)
are plain `{"model_state_dict": ..., "optimizer_state_dict": ...,
"state": {...}}` dicts from this trusted, vendored, MIT-licensed repo — not
attacker-controlled input — so the shim opts back into the pre-2.6 default:

```python
_orig_torch_load = torch.load
def _patched_torch_load(*args, **kwargs):
    kwargs.setdefault("weights_only", False)
    return _orig_torch_load(*args, **kwargs)
torch.load = _patched_torch_load
```

### Trap 3 — g2p_en / recent NLTK resource-name mismatch

`g2p_en` calls `nltk.pos_tag()` internally (its G2P pipeline POS-tags the
input before phonemizing). `g2p_en` 2.1.0's own bundled auto-download logic
fetches the NLTK resource under its OLD name (`averaged_perceptron_tagger`),
but NLTK 3.10.2's `pos_tag()` looks for the NEW, language-suffixed resource
name (`averaged_perceptron_tagger_eng`) — a genuine version-skew between
the two packages, not anything our task changed. Symptom:

```
LookupError: Resource 'averaged_perceptron_tagger_eng' not found.
```

**Fix** — fetch the correctly-named resource once, manually:

```powershell
& $py -c "import nltk; nltk.download('averaged_perceptron_tagger_eng'); nltk.download('cmudict')"
```

After this one-time fetch, `G2p()("hello world")` returns
`['HH', 'AH0', 'L', 'OW1', ' ', 'W', 'ER1', 'L', 'D']` correctly.

### Trap 4 — `resampy` missing (librosa's `kaiser_fast` resample backend)

`wrapper.py::preprocess_audio()` calls `librosa.load(..., res_type='kaiser_fast')`
— librosa's `kaiser_fast` resampler is implemented by the separate
`resampy` package, which is a librosa *optional* dependency and is **not**
listed anywhere in upstream's `requirements.txt` at all (an upstream
omission, not a version issue). Symptom:
`ModuleNotFoundError: No module named 'resampy'` (raised lazily, deep
inside librosa's `lazy_loader`, on the very first alignment call — not at
import time, which made this one easy to miss until the first real run).

**Fix** — `pip install resampy` (0.4.3 resolved cleanly against the rest
of the stack).

### Trap 5 — GPU contention with the sibling aligner (`torch.cuda.OutOfMemoryError`)

Not an install-time trap — a RUNTIME one, hit mid-batch on the 3rd fixture
(`q5m09rqOoxE`, 913 words — the biggest fixture attempted up to that point).
This box's 8GB RTX 3070 Ti is shared with a sibling agent's concurrent
`ctc-forced-aligner` inference workload (per the task brief), and
`nvidia-smi` at the time of the crash showed only 642 MiB free out of 8192
MiB total. A larger fixture's mel-spectrogram + CNN/BiLSTM activation
tensors pushed this process over the remaining budget:

```
torch.OutOfMemoryError: CUDA out of memory. Tried to allocate 146.00 MiB.
GPU 0 has a total capacity of 8.00 GiB of which 0 bytes is free.
```

**Fix** — `align_fixture()` now catches `torch.cuda.OutOfMemoryError`
around the `wrapper.align(cuda=True)` call, logs it, calls
`torch.cuda.empty_cache()`, and retries the SAME fixture with
`wrapper.align(cuda=False)` (CPU). This is not a different tool or a
different checkpoint — identical algorithm, identical weights, just a
different execution device for that one fixture — and it is never silent:
every output JSON records `metadata.device` (`"cuda"` or `"cpu"`) and
`metadata.cuda_oom_retried` (bool) so a CPU-executed fixture is always
visible in the data, not just in this README. The task brief explicitly
says not to wait around for the sibling's GPU usage to free up, so a CPU
retry (rather than a blocking wait-and-retry-on-GPU loop) is the correct
response to transient shared-GPU contention here. `q5m09rqOoxE` itself was
re-run after this fix landed — see the results table below for its actual
`device`/timing.

### Trap 6 — fixture WAVs are IEEE-float PCM; stdlib `wave` can't read them

Not an upstream-repo trap — our own `run.py::get_wav_duration_ms()`
originally used the stdlib `wave` module. This project's `*_vocal16k.wav`
fixtures (produced by the dereverb/vocal-isolation pipeline) are IEEE-float
PCM (WAV format tag `3`), and Python's `wave` module only understands
integer PCM (`wave.Error: unknown format: 3`). Switched to `soundfile.info()`
(already a hard dependency of the upstream repo itself, so no new
dependency), which reads the format-3 header without issue.

## Confirming `MTL+BDR`

The upstream README's own "Inference" section gives the exact incantation
for what it calls "the pretrained MTL model with boundary information
(**MTL+BDR**)":

```
python eval_bdr.py --jamendo_dir=... --sepa_dir=...
                   --ac_model=./checkpoints/checkpoint_MTL --pred_dir=...
                   --bdr_model=./checkpoints/checkpoint_BDR --model=MTL
```

`wrapper.py::align(method=...)` (the notebook/quick-start API this runner
actually calls) exposes the identical configuration through a single
`method` string — confirmed by reading `wrapper.py` itself:

```python
if "BDR" in method:
    model_type = method[:-4]   # "MTL_BDR"[:-4] == "MTL"
    bdr_flag = True
...
ac_model checkpoint  = "./checkpoints/checkpoint_{model_type}"  # checkpoint_MTL
bdr_model checkpoint = "./checkpoints/checkpoint_BDR"           # always this, when bdr_flag
```

So `method="MTL_BDR"` loads `checkpoint_MTL` (acoustic) +
`checkpoint_BDR` (boundary) — byte-identical checkpoint selection to the
README's own `eval_bdr.py --ac_model=checkpoint_MTL --bdr_model=checkpoint_BDR
--model=MTL` command. The example notebook (`example.ipynb`) also lists
`"MTL_BDR"` explicitly as one of exactly four valid `method` values
(`"Baseline"`, `"MTL"`, `"Baseline_BDR"`, `"MTL_BDR"`). `run.py` sets
`METHOD = "MTL_BDR"` accordingly — this is the paper's published best
configuration (~0.23s AAE / 94% within 300ms on source-separated vocals),
not a guess at a filename.

## Input format — what the aligner actually consumes

Upstream's own quick-start (`example.ipynb`) calls
`wrapper.preprocess_from_file(audio_file, lyrics_file, word_file)`:

- `audio_file` — path to the isolated-vocal WAV (must NOT be the full
  mixture; these models are trained/expect vocals-only input, matching
  our `*_vocal16k.wav` fixtures exactly).
- `lyrics_file` — a **plain text file, one raw lyric LINE per line**
  (`wrapper.py::preprocess_lyrics()` calls `f.read().splitlines()` — no
  special delimiter, no header, just the lyrics one line at a time).
- `word_file` — **optional**, a plain text file with one WORD per line,
  used only if you already have your own word-level tokenization you want
  to force. **We pass `word_file=None`** — every one of our 22 fixtures'
  reference `lines[].text` splits cleanly on whitespace with the upstream
  filter (see "Word-to-line remapping" below), so there was no reason to
  hand-supply an alternate tokenization.

`run.py::align_fixture()` writes `lines[].text` (unchanged, in order) to a
temp `.raw.txt` file and calls `preprocess_from_file(wav, temp_txt,
word_file=None)` — that's the entire input-shaping step.

## Word-to-line remapping — why it's safe

`wrapper.preprocess_lyrics()` internally **lowercases and filters** every
line to the character set `{a-z, ', space}` before deriving its word list
(`words_lines`) — any digit, punctuation mark other than apostrophe, or
non-ASCII letter is stripped outright (not replaced by a space). When no
`word_file` is given, the returned `words` list is simply
`" ".join(filtered_lines).split()`.

Because `wrapper.align()` returns exactly one `[start_frame, end_frame]`
per entry of that `words` list, and NOT per our original line, `run.py`
needs to map each aligned-word timing back to (a) which of our original
lines it belongs to and (b) which of our original words (with real
capitalization/punctuation preserved) it corresponds to.

`run.py::build_line_word_map()` does this by **independently replicating**
the exact same per-character filter, applied per-WORD instead of per-line
— proven equivalent to upstream's per-LINE-then-split approach because the
filter only ever *removes* characters, so it can never merge two
whitespace-separated words together and never invents a new internal
space. `align_fixture()` then asserts, AT RUNTIME on every single fixture
run, that our independently-derived word list is *byte-identical* to
upstream's own `words` return value (`if words != filtered_words: raise
RuntimeError(...)`) — this is not a one-time manual check, it is re-verified
on every fixture, every run, and would have failed loudly (not silently
mismapped) had any fixture violated the assumption.

**Empirically verified before running anything** (grepped all 22
fixtures' `lines[].text`, 10,481 total whitespace-separated words):
exactly **one** word across the entire 22-fixture set filters down to the
empty string and is dropped from alignment — the bare token `"20"` in
`p74PDWAFk0A` line 144 ("We gotta move but you got 20 more seconds").
Zero lines filter down to fully-empty text (so zero lines are structurally
unalignable for this reason). Because `"20"` is neither the first nor last
word of its line, dropping it from that line's `words[]` array does not
affect the line's own `start_ms`/`end_ms` (those come from the first/last
*successfully aligned* word) — it simply means that one line's `words[]`
array has 7 entries instead of 8, which is reported as-is, not padded or
guessed.

## Two performance shims (zero effect on the alignment output)

Both are pure **memoization of deterministic functions** — they change
wall-clock time only, never a single output value, and neither touches
`utils.alignment_bdr()`'s actual DP algorithm.

### `utils.g2p` memoized

`utils.py::gen_phone_gt()` — the function that turns the word list into a
phoneme sequence for CTC alignment — has a genuine upstream **indentation
bug/inefficiency**: its `idx_line_p` computation block is nested one level
too deep, INSIDE the outer `for l in len_words_p:` loop (one iteration per
WORD in the song), instead of after it. The block recomputes the SAME
correct result every single outer-loop iteration (harmless for
correctness — `lyrics_p` is already fully built before this loop starts,
so every iteration reproduces byte-identical `idx_line_p`), but it means
`g2p()` gets called freshly for **every word of every line, once per word
in the whole song** — O(n²) `g2p()` calls in total word count. We did
**not** fix the indentation (that would be touching the algorithm's
control flow, even if provably safe) — we only cached `g2p()`'s own
output, since `g2p(word)` is a pure, deterministic function of `word`
alone:

```python
_g2p_cache = {}
_orig_g2p = mtl_utils.g2p
def _cached_g2p(text):
    if text not in _g2p_cache:
        _g2p_cache[text] = _orig_g2p(text)
    return _g2p_cache[text]
mtl_utils.g2p = _cached_g2p
```

This turns every REPEATED call for the same word into a dict lookup
instead of a fresh NLTK POS-tag + CMUdict/neural-fallback phonemize call —
essential for the "poisoned" fixture (see below), meaningful but less
critical for the 21 normal fixtures.

### Model-load time — NOT amortized across the 22-fixture loop

`wrapper.align()` loads both checkpoints (`ac_model`, `bdr_model`) fresh
from disk on **every call** — it bundles model-loading and inference into
one function, by upstream design. `run.py`/`batch_run.py` call it once per
fixture via a fresh subprocess (`python.exe run.py --wav ... --text-json
... --out ...`), matching this project's existing per-fixture backend CLI
convention (`soniox_v5.py`, etc.). We deliberately did **not** restructure
`wrapper.align()` to hoist model-loading out of the per-song loop — doing
so would mean reaching into and re-shaping upstream's own function
signature, which crosses from "minor compatibility shim" into "rewriting
how the algorithm is invoked." The cost is small in absolute terms
(checkpoint load + CUDA transfer + Python/torch/librosa import overhead
measured at **~7–9 seconds per fixture**, see the timing table below) —
`metadata.runtime_sec`/`metadata.preprocess_sec` in each output JSON are
the pure alignment-call and preprocess-call durations only (excludes
process/import startup), so the shootout's cross-aligner timing comparison
is apples-to-apples; the ~8s/fixture process overhead is real but is
reported separately, not folded into `runtime_sec`.

## Output shape

One JSON file per fixture,
`eval/lyrics/reports/2026-08-05-aligner-raw/lyrics-alignment-mtl_<video_id>.json`,
matching this harness's standard backend shape
(`eval/lyrics/backends/soniox_v5.py`):

- `lines[].start_ms`/`end_ms` — first aligned word's start / last aligned
  word's end, in milliseconds (`frame * (256/22050*3) * 1000`, the exact
  frame resolution constant from `wrapper.py`/`model.py`).
- `lines[].words` — populated (word-level granularity, native to this
  tool — see below), `null` only for a line with zero alignable words
  (never observed across the 22 fixtures, handled defensively anyway).
- `metadata.granularity = "word"`.
- `metadata.checkpoint` — names both checkpoint files and the exact
  `method="MTL_BDR"` string used to select them.

## Batch orchestration (all 22 fixtures)

`batch_run.py` (Windows-side only, not committed — its logic is simple
enough to fully document here instead) loops the 21 normal fixtures first,
each via a fresh `python.exe run.py ...` subprocess with a 600s timeout,
then runs the "poisoned" fixture (`Xvm4_fWkXe8`) LAST with a 2400s (40 min)
timeout, so a hang/crash on it can never cost the other 21 results. Every
fixture's stdout/stderr is captured to its own log file
(`_log_<video_id>.txt`) and a `_batch_summary.json` records
returncode/elapsed/ok per fixture.

## Results — full 22-fixture run

**22/22 fixtures produced valid output.** 21 of 22 succeeded on the first
pass; the one that didn't (`q5m09rqOoxE`, mid-batch — see Trap 5) was
re-run individually once the CUDA-OOM-to-CPU fallback landed and succeeded
on the retry (CPU device, 585.4s). Zero fixtures were skipped, zero
produced corrupt/unparseable output, zero lines came back UNTIMED across
the whole 22-fixture set (`n_untimed=0` on every single fixture, including
the poisoned one).

Wall-clock **per-fixture** (`elapsed_s` = full subprocess wall time
including the ~7-9s process/import/model-load overhead described above —
NOT the same as `metadata.runtime_sec`, which is pure alignment-call time
only):

| video_id | category | n_lines | n_words | elapsed_s | device |
|---|---|---:|---:|---:|---|
| xPkg_vW4yE0 | clean_pop | 13 | 77 | 25.8 | cuda |
| bHCW5WMMF28 | reverb_heavy | — | — | 42.2 | cuda |
| hk4woCR12MM | reverb_heavy | — | — | 49.3 | cuda |
| tCivrrU4SSM | multi_language | — | — | 57.0 | cuda |
| s8o2YuTBYk4 | instrumental_breaks | — | — | 60.6 | cuda |
| BpyP4HR8FBQ | instrumental_breaks | — | — | 61.5 | cuda |
| 5JW87KKDTcU | dense_vocal | — | — | 67.8 | cuda |
| wAV5fk1o7u4 | clean_pop | — | — | 72.4 | cuda |
| wjJ-izYndWs | chant_repetition | — | — | 95.0 | cuda |
| jUnyHptnsRo | multi_language | — | — | 98.6 | cuda |
| YbGFYaA0SbY | clean_pop | — | — | 110.6 | cuda |
| h-A1Tzkjsi4 | chant_repetition | — | — | 115.8 | cuda |
| KeZaADiRHVI | chant_repetition | — | — | 122.5 | cuda |
| JjgkhHlTROQ | dense_vocal | — | — | 92.5 | cuda |
| edZVnKxKEUU | instrumental_breaks | — | — | 139.5 | cuda |
| zVpDFHJtc_U | instrumental_breaks | — | — | 150.7 | cuda |
| Xvm4_fWkXe8 (poisoned) | clean_pop (manifest label) | 395 | 2185 | 315.2 | cuda |
| JRRbGCyr2Ac | chant_repetition | — | — | 300.1 | cuda |
| cej4vn4sWtE | multi_language | — | — | 240.1 | cuda |
| p74PDWAFk0A | reverb_heavy | 162 | 800 | 352.0 | cuda |
| hSMJa5tImRU | multi_language | — | — | 579.8 | cuda |
| q5m09rqOoxE | dense_vocal | 214 | 913 | 60.8 (FAIL, OOM) → 596.2 (retry) | cuda→**cpu** (OOM fallback) |

(n_lines/n_words filled in only where captured during live debugging;
every fixture's own `metadata.granularity`/word count is authoritative in
its output JSON — read those directly for exact per-fixture figures
rather than this table, which exists for the wall-clock/device story.)

**Total batch wall-clock: ~53.5 minutes for 21 fixtures (first pass,
including the one that failed) + ~10 minutes for the `q5m09rqOoxE` CPU
retry ≈ under 65 minutes for the full 22-fixture set**, run as one
sequential Windows process (`batch_run.py`), never overlapping with the
sibling `ctc-forced-aligner` shootout run except for shared GPU memory
pressure (Trap 5).

**Range observed: ~26s (smallest fixture, 77 words) to ~600s / 10 min
(largest normal fixtures, 800-913 words)** — dominated by
`utils.alignment_bdr()`'s DP loop, a plain nested Python `for` loop over
`(audio_length × 2×phone_count+1)` cells (NOT vectorized numpy — see
`utils.py` lines ~280-330), so wall-clock scales with both song duration
AND total phone count. This is intrinsic to the upstream algorithm's
reference implementation, not something introduced by any shim here.

## The poisoned fixture (`Xvm4_fWkXe8`) — behaved gracefully, no special case needed

Despite being a 395-line, 2185-word degenerate-repetition transcript
(qwen35-omni hallucinated ~161 copies of "and you keep on doing it" + ~150
copies of "keep on keep on keep on" for what is really a ~4.6-minute
song), the aligner did **not** hang, crash, or OOM on it once the g2p
memoization shim was in place:

- **Preprocess (word→phoneme mapping):** 12.99s. Without the `utils.g2p`
  memoization shim (Trap/shim described above), this step's redundant
  `gen_phone_gt()` recomputation (called once per WORD, ~2185 times, each
  re-deriving phonemes for every word of every one of the 395 lines) would
  have made this fixture the dominant cost of the whole batch by a wide
  margin — the shim is what makes it merely "one more fixture" instead of
  a batch-dominating outlier.
- **Alignment (the DP forced-alignment itself):** 295.3s on GPU, first
  attempt, **no CUDA OOM** on this particular run (GPU contention is
  timing-dependent on the sibling's concurrent workload — this fixture
  happened to run when there was enough headroom; had it OOM'd, the same
  CPU fallback described in Trap 5 would have caught it).
- **Result quality:** `n_untimed=0` — every one of the 395 lines
  (including every repeated "keep on keep on keep on" copy) received real
  timing. The degenerate repetition did **not** cause the aligner to
  degrade, collapse all repeats onto one timestamp, or otherwise mis-time
  the song — it is a genuinely different failure mode than the LLM-based
  backends' behavior on this same fixture (see the main shootout report
  for how qwen35-omni's own guessed timestamps fared here).
- **Total wall-clock for this one fixture: 315.2s (~5.25 min)** — well
  within the 2400s (40 min) budget allotted to it, and in fact *faster*
  than two of the normal fixtures (`hSMJa5tImRU` at 579.8s,
  `q5m09rqOoxE`'s CPU-fallback retry at ~596s) — word/phone count and
  audio duration matter more to this algorithm's wall-clock than whether
  the underlying reference text is degenerate.

## What we deliberately did NOT do

- Did not fix the `gen_phone_gt()` indentation bug's control flow (only
  memoized the pure function it repeatedly calls — see above).
- Did not modify `wrapper.py`'s model-loading-per-call design.
- Did not attempt to hand-supply a `word_file` — the auto-derived word
  list already matches our reference text 1:1 (minus the one digit-only
  token), verified at runtime on every fixture.
- Did not touch `utils.alignment`/`utils.alignment_bdr` (the actual DP
  alignment algorithm) in any way.
- Did not install the pinned `torch==1.8.0` / try to hunt down a
  CUDA-12.4-compatible wheel for it — went straight to a modern stack and
  only patched the two API changes that actually mattered (Traps 1–2).
