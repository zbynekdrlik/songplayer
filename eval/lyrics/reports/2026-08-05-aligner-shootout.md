# Lyrics Eval — Forced-Aligner Shootout — 2026-08-05

## Corrections (2026-08-06)

An adversarial review of the eval harness on 2026-08-06 found 14 verified
defects; several invalidated numbers published in this report. The scorers
were fixed (commits `d42264e`, `b048953`) and every row below was re-scored
**from the same committed artifacts — no backend was re-run, no API was
called**. The body has been amended in place; this table is the audit
trail so nothing was changed silently.

| # | Was | Is now | Cause |
|---|---|---|---|
| 1 | Baseline row **41.4%** ≤400ms / **573ms** median / **20.2%** untimed / **70.1%** coverage | **42.2%** / **570ms** / **6.2%** / **70.0%** | The poisoned fixture `Xvm4_fWkXe8` was excluded from every aligner row but pooled into the baseline — a 22-fixture baseline compared against 21-fixture aligners under a "21 fixtures" header. **297 of the baseline's 394 untimed lines were that one fixture's**, which is the entire 20.2% → 6.2% move. (`run_combine_experiment.py`, both `build_backend_report` call sites.) |
| 2 | "`ctc-forced-aligner-star` (41.7%) beats the ASR-combiner baseline (41.4%)" | **STRUCK — false. The baseline wins**: 42.2% vs 41.7% conditional, 29.6% vs 27.8% gold-normalized. | Same defect as #1. |
| 3 | The 41–44% band quoted as one comparable quality figure | Restated as **conditional on matched, timed lines** (denominator 1138–1243, different per backend) and published beside a **gold-normalized** column on the shared 1702-gold-line denominator — where the band is **19.7–31.6%**. | `score_one_call.py` divided by matched-and-timed lines only, so the worst backend got the smallest denominator. |
| 4 | (no such view existed) | A third **monotonic** (order-respecting) view added throughout — 27–28% of all scored pairs bind backwards in the song. | The greedy matcher has no monotonicity constraint. |
| 5 | `lyrics-alignment-mtl` "**150s/song** mean vs 1.4s" | **Device-blended.** GPU-only **106.0s (n=19)**, CPU-fallback **575.4s (n=2)**. The GPU-normalized MTL-vs-`ctc-star` ratio is **75.7×**, not the 107.7× the blended figure implies. | The CUDA-OOM→CPU fallback recorded `metadata.device`, but `score_aligner.py` never read it, so two populations were pooled and attributed to the model. |
| 6 | "a ~10 min CPU-fallback retry for **the one fixture** that OOM'd, `q5m09rqOoxE`" | **TWO fixtures ran on CPU after CUDA-OOM**: `hSMJa5tImRU` (565.5s, caught in-process) and `q5m09rqOoxE` (585.4s, re-run separately) — both `cuda_oom_retried: true`. | Same defect as #5. `aligners/lyrics_alignment_mtl/README.md` additionally listed `hSMJa5tImRU` as `cuda`, contradicting its own output JSON; corrected there too. |
| 7 | "21 fixtures" (a hardcoded string in the scorer's own summary) | **21 scored, 2 errored** (`fHYLw-2tTx4`, `vpwDdb8r9Bk` — no output produced), stated with every table. | `score_aligner.py` printed a literal instead of `agg['n_fixtures']` and never printed the errored count. |

**What did NOT change** — re-derived and confirmed identical: all three
aligner rows' conditional %≤400ms (29.4 / 41.7 / 43.5), medians (900 / 650
/ 528), coverage (66.9 / 66.9 / 72.6), untimed (10.4 / 10.4 / 0.0), every
per-category median, and the `star`-vs-no-star delta (+12.3 pts).

**What it means for the verdict.** The ORDER is unchanged —
`lyrics-alignment-mtl` is still 1st on every accuracy view, so the
selection decision stands — but its **margin is materially thinner than
published** (1st-vs-2nd is 0.2–1.3 points, not 2.1), the 2nd-place claim
was wrong, and MTL is **last on runtime by two orders of magnitude**. The
honest framing is "MTL is best of the field on every accuracy metric by a
small margin, and shares with `elevenlabs-fa` the only 0.0% untimed rate"
— not "MTL clears the field". See "Ranked verdict" below, rewritten.

**Question:** the audio-LLM (`qwen35-omni`) already produces good lyric LINE
content but bad line TIMING (north-star sweep:
`reports/2026-08-05-one-call-northstar.md`); recombining it with a dedicated
ASR's WORD timings closed part of that gap but never a real forced aligner
(`reports/2026-08-05-combine-experiment.md`, best: `qwen35-omni` lines ×
`aai-u35-translate` ASR word times — **42.2% of matched-and-timed lines
≤400ms, median 570ms, 6.2% of lines completely UNTIMED**, corrected
2026-08-06 per the table above). **Does an actual forced aligner — given
the fixed correct text plus the isolated-vocal audio, doing real forced
alignment instead of ASR-then-graft — beat that baseline, and by how much?**

This report benchmarks two forced aligners, installed and run on
`win-resolume`, against that exact baseline.

## Method

- **Fixed input, never re-derived**: for each of the 22 pinned manifest
  fixtures with a cached vocal WAV, the reference text is
  `qwen35-omni_<video_id>.json`'s `lines[].text` (English, as already
  transcribed) — the SAME reference every other backend in this harness's
  one-call sweep used. An aligner receives this text UNCHANGED, plus
  `<video_id>_vocal16k.wav` (16kHz, isolated/dereverbed vocals), and returns
  timestamps for that exact text. Neither aligner transcribes or
  reinterprets a single word.
- **Line timing convention**: `start_ms` = the first aligned word's
  `start_ms`; `end_ms` = the last aligned word's `end_ms`. A line with zero
  alignable words is UNTIMED (`start_ms`/`end_ms`/`words` = `null`) — never
  guessed, never interpolated.
- **Scoring, two views** (`eval/lyrics/score_aligner.py`, new — thin glue
  over existing, unmodified scorers): the **official** view is
  `score_one_call.py`'s greedy closest-start text matcher (ratio ≥0.6,
  gold-line-consuming); the **conservative** view
  (`run_combine_experiment.conservative_match`) pairs a produced line to a
  gold line only when that gold line's normalized text is UNIQUE in the
  song (ratio ≥0.75) — no repeat-disambiguation needed, since there is only
  ever one candidate. Both are reported every time, per
  `.claude/rules/lyrics-eval-backends.md`'s standing caution that the
  official matcher has no monotonicity constraint and can mis-pair
  repeated/chant lyrics.
- **The poisoned fixture** (`Xvm4_fWkXe8` — `qwen35-omni` hallucinated a
  395-line degenerate repetition loop, ~161 copies of "and you keep on
  doing it" + ~150 copies of "keep on keep on keep on", for what is really
  a ~4.6-minute song) is run like every other fixture but **excluded from
  every pooled/per-category aggregate below** and reported on its own
  (`score_aligner.py`'s `POISONED_FIXTURE_VIDEO_ID` exclusion) so it cannot
  dominate a 22-fixture picture.
- All pooled/per-category numbers below are over the remaining **21
  fixtures**.
- **Denominators — identical for every row in this report, and stated on
  purpose** (see correction #3): 21 fixtures scored, **2 errored** with no
  output at all (`fHYLw-2tTx4`, `vpwDdb8r9Bk`, added to `manifest.json`
  after this run's scope was fixed), poisoned fixture excluded. **1553**
  `qwen35-omni` reference lines were submitted; the scored fixtures hold
  **1702** gold lines; **1867** gold lines including the two errored
  fixtures.
  - **Conditional %≤400ms** = within-400ms ÷ *matched-and-timed lines*.
    Untimed lines are stripped upstream and unmatched gold lines never
    enter, so this denominator is **1138–1243 and differs per backend** —
    each backend is graded only on the subset it managed to handle. It is
    **not** "% of lines correctly timed" and is not comparable across
    backends on its own.
  - **Gold-normalized %≤400ms** = within-400ms ÷ *the same 1702 gold
    lines*, for every backend. **This is the comparable figure.** A
    variant over all 1867 (counting the errored fixtures' gold) is given
    in the secondary table.
  - **Monotonic** = the same matcher re-run with an ordering constraint,
    which drops the 27–28% of pairs binding backwards in the song. It
    removes large deltas from numerator and denominator together, so
    every backend's number rises; only the relative ordering is
    meaningful.

## Aligners benchmarked

| Aligner | Backend id(s) | What it is |
|---|---|---|
| `ctc-forced-aligner` (MahmoudAshraf97) | `ctc-forced-aligner` (no star), `ctc-forced-aligner-star` (`--star_frequency segment`) | wav2vec2-CTC forced alignment, `MahmoudAshraf/mms-300m-1130-forced-aligner` |
| `LyricsAlignment-MTL` (Huang/Benetos/Ewert, ICASSP 2022) | `lyrics-alignment-mtl` | CNN+BiLSTM acoustic model + DP alignment, `MTL+BDR` checkpoint (the paper's published best configuration) |

Full install READMEs — every trap hit, every version pinned, the exact
commands that worked — live at `eval/lyrics/aligners/ctc_forced_aligner/README.md`
and `eval/lyrics/aligners/lyrics_alignment_mtl/README.md`. This report
summarizes; those READMEs are the "don't debug it again next week" artifact.

## Install reality (summary — see the two READMEs for full detail)

**`ctc-forced-aligner`** — total first-time setup **~45 minutes**, dominated
by needing a C++ compiler it didn't have (no PyPI wheels exist for this
package on any platform — it ships a pybind11 extension that must be built
locally; fixed with a silent `winget install ... VisualStudio.2022.BuildTools`,
~4 min). Five other real traps, each with a small documented fix: `ffmpeg`
not on `PATH` (the library shells out to a bare `ffmpeg` command), the
project's 32-bit-float-PCM WAVs breaking a naive `wave`-module duration
fallback, **a real upstream crash on literal digit characters in the
reference text** (`AssertionError` inside `get_spans` — the MMS
char-vocabulary has no digits, so the library's own
filter/re-walk logic goes out of sync; caught per-fixture, that one fixture
falls back to all-untimed rather than crashing the batch), GPU-VRAM
contention with the sibling `LyricsAlignment-MTL` run sharing the same 8GB
card (fixed with a retry-with-backoff model load), and one non-deterministic
hard subprocess crash on the poisoned fixture (fixed by writing a fallback
output on ANY subprocess failure, not only on timeout). Once installed, the
full 22×2-config batch (model loaded once) ran in **~130 seconds total** —
by far the fastest of the two aligners.

**`LyricsAlignment-MTL`** — total first-time setup **under 15 minutes**, no
dependency-hell dead end. Went straight to a modern `torch==2.6.0+cu124`
stack instead of the 2021-era pinned `torch==1.8.0` (which predates this
box's CUDA 12.4 driver entirely) — only two real API breaks mattered:
`numpy.Inf` removed in NumPy 2.0 (the upstream DP-alignment code uses it as
a score-table initializer; restored the alias before import) and
`torch.load`'s `weights_only=True` default in torch ≥2.6 blocking the
vendored, trusted checkpoint files (opted back into the pre-2.6 default).
Three smaller traps: `g2p_en`/NLTK resource-name version skew (one-time
manual corpus fetch), `resampy` missing (an upstream `requirements.txt`
omission — librosa's `kaiser_fast` resampler needs it), and the same
32-bit-float-PCM WAV issue CTC hit, fixed the same way (`soundfile.info()`
instead of stdlib `wave`). One genuine runtime issue: shared-GPU contention
with the sibling CTC run caused a `torch.cuda.OutOfMemoryError` — caught
and retried on CPU (never silent: every output records `metadata.device`
and `metadata.cuda_oom_retried`). **This hit TWO fixtures, not one**
(corrected 2026-08-06, correction #6): `hSMJa5tImRU` OOM'd and was caught
in-process by the fallback (565.5s alignment on CPU, 579.8s wall), and
`q5m09rqOoxE` OOM'd *before* the fallback landed and had to be re-run
separately (585.4s alignment on CPU, 596.2s wall). Model
load is NOT amortized across fixtures (upstream's `wrapper.align()` bundles
load+inference per call by design, not touched) — each fixture pays a fresh
~7-9s process/model-load overhead on top of its actual DP-alignment time,
which is the dominant cost (a plain, non-vectorized Python DP loop — see the
README). Full 22-fixture batch: **~65 minutes wall-clock**
(53.5 min first pass, which already includes `hSMJa5tImRU`'s in-process CPU
fallback, + a ~10 min separate re-run of `q5m09rqOoxE`). **Because two of
the 21 fixtures ran on CPU, the batch wall-clock is device-blended and
must not be divided by 21 to get "the model's speed"** — see the runtime
table below.

**A cross-cutting trap neither README anticipated going in**: transferring
44+22 output files back through the `win-resolume` MCP text channel
one-by-one proved far too slow/wasteful in practice (small per-file
`FileRead`/`Write` round trips, and `FileRead` turns out to have its own
real ~100,000-character truncation ceiling, undocumented before this run).
Both aligners' large batches were instead pulled with a temporary
`python -m http.server` started on `win-resolume` and `curl`'d directly from
the coordinating side — the same trick both agents converged on
independently — then the server was stopped immediately after. Recorded here
as the actual working transfer method for the next person doing this.

## Headline comparison — official view (21 fixtures scored, 2 errored, poisoned excluded)

**Every row shares one denominator set: 1553 reference lines in, 1702 gold
lines in the scored fixtures, 21 scored / 2 errored.** The two %-columns
must be read together — see the Method section's denominator block. The
sibling `elevenlabs-fa` row (`reports/2026-08-05-elevenlabs-fa.md`) is
included here because it was scored on exactly this denominator set.

| Backend | Cond. %≤400ms *(matched+timed only)* | **Gold-norm. %≤400ms** *(of 1702)* | Median Δ | Coverage | Untimed % | n matched | Mean runtime/song |
|---|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (`qwen35-omni` lines × `aai-u35-translate` ASR times) | 42.2% | 29.6% | 570ms | 70.0% | 6.2% | 1191 | — |
| `ctc-forced-aligner` (no star) | 29.4% | 19.7% | 900ms | 66.9% | 10.4% | 1139 | 1.5s |
| `ctc-forced-aligner-star` (`--star_frequency segment`) | 41.7% | 27.8% | 650ms | 66.9% | 10.4% | 1138 | 1.4s |
| `elevenlabs-fa` (hosted API — sibling report) | 42.2% | 30.7% | 625ms | **72.6%** | **0.0%** | 1236 | 6.9s |
| **`lyrics-alignment-mtl` (MTL+BDR)** | **43.5%** | **31.6%** | **528ms** | **72.6%** | **0.0%** | 1236 | **106.0s (GPU)** |

The conditional column's denominator ranges 1138 → 1243 across these rows;
the gold-normalized column's is 1702 for all of them. **Quote the
gold-normalized column when comparing backends.**

### Secondary views (same 21 fixtures)

| Backend | Mono. cond. %≤400ms | Mono. gold-norm. %≤400ms | Mono. median Δ | Gold-norm. over all 1867 | Coverage over all 1867 | p90 Δ *(NOT quotable)* |
|---|---:|---:|---:|---:|---:|---:|
| Baseline | 48.7% | 22.3% | 419.5ms | 26.9% | 63.8% | 7238ms |
| `ctc-forced-aligner` | 31.8% | 13.7% | 755ms | 17.9% | 61.0% | 8338ms |
| `ctc-forced-aligner-star` | 47.7% | 19.6% | 460ms | 25.4% | 61.0% | 10817ms |
| `elevenlabs-fa` | 49.4% | 23.6% | 417ms | 28.0% | 66.2% | 12580ms |
| **`lyrics-alignment-mtl`** | **50.5%** | **23.8%** | **387.5ms** | **28.8%** | **66.2%** | 7289ms |

The monotonic view discards **34–39% of all pairs** (MTL 1236 → 802). p90
is carried only for continuity with the original publication: per
`.claude/rules/lyrics-eval-backends.md` **only median and %≤400ms are
quotable** — the unconstrained matcher admits physically impossible pairs
that dominate any mean or tail statistic (the pooled conservative *mean*
for the baseline is 33344.7ms).

### Mean runtime per song, split by device (correction #5)

| Backend | Device | n | Mean | Median | Note |
|---|---|---:|---:|---:|---|
| Baseline | — | — | — | — | Offline recombination of already-committed ASR output; no per-song runtime exists. |
| `ctc-forced-aligner` | not recorded | 20 | 1.5s | 1.2s | Backend writes no `metadata.device`; `run.py` defaults to cuda-if-available and this batch ran on the win-resolume GPU box. Alignment call only — `model_load_sec` (14.6s) is paid once per batch. n=20 not 21: `p74PDWAFk0A` crashed on the digit bug with `runtime_sec: null`. |
| `ctc-forced-aligner-star` | not recorded | 20 | 1.4s | 1.2s | Same; `model_load_sec` 18.7s once. |
| `elevenlabs-fa` | remote API | 21 | 6.9s | 6.0s | No local device — wall-clock of the HTTPS call, network included. Partially verified (see the sibling report). |
| **`lyrics-alignment-mtl`** | **cuda** | **19** | **106.0s** | **85.2s** | **The figure to quote for this model.** |
| | **cpu (after CUDA-OOM)** | **2** | **575.4s** | **575.4s** | `hSMJa5tImRU` 565.5s, `q5m09rqOoxE` 585.4s — both `cuda_oom_retried: true`. |
| | *blended (as originally published)* | *21* | *150.8s* | *86.9s* | Device-blended; **do not quote as the model's speed.** |

## Conservative view (unique-gold-line only, ratio ≥0.75)

| Backend | Cond. %≤400ms *(of n pairs)* | **Gold-norm. %≤400ms** *(of 1702)* | Median Δ | n pairs |
|---|---:|---:|---:|---:|
| Baseline (`qwen35-omni` × `aai-u35-translate`) | 33.5% | 7.3% | 1155ms | 373 |
| `ctc-forced-aligner` (no star) | 21.4% | 4.5% | 1570ms | 355 |
| `ctc-forced-aligner-star` | **35.2%** | 7.3% | 1090ms | 355 |
| `elevenlabs-fa` | 34.3% | 7.6% | 1470ms | 379 |
| **`lyrics-alignment-mtl`** | 34.8% | **7.8%** | **1115ms** | 379 |

`lyrics-alignment-mtl` has the most conservative-matchable pairs (379 vs
355) because it lost fewer lines to UNTIMED — the conservative pool only
ever includes lines the aligner actually timed.

**This conditional cell is the ONE accuracy figure in the whole matrix
where `lyrics-alignment-mtl` is not first**, and it is a denominator
artifact of exactly the kind correction #3 exists to expose: `ctc-star`
scores its 35.2% over 355 pairs, MTL over 379. Normalized onto the shared
1702 gold lines the order reverts to MTL 7.8% > `elevenlabs-fa` 7.6% >
`ctc-star` 7.3% = baseline 7.3%. **Do not quote conservative-conditional as
a ranking.**

## Per-category breakdown (official view)

Format: median Δ / **conditional** %≤400ms / **gold-normalized** %≤400ms.
**Bold** = best of the three per category. Per-category coverage differs
per backend (`reverb_heavy`: CTC 35.5% vs MTL 75.0%), so the conditional
column is not comparable across a row — the gold-normalized one is.

| Category | `ctc` (no star) | `ctc-star` | `lyrics-alignment-mtl` |
|---|---|---|---|
| chant_repetition | 902ms / 25.7% / 19.6% | 833ms / 33.8% / 25.8% | **751ms / 34.8% / 26.5%** |
| clean_pop | 390ms / 50.0% / 49.0% | 272.5ms / 71.6% / 70.2% | **260ms / 74.5% / 73.1%** |
| dense_vocal | 820ms / 32.8% / 23.6% | 470ms / 48.1% / 34.5% | **332ms / 51.9% / 37.3%** |
| instrumental_breaks | 561.5ms / 37.6% / 26.9% | 390ms / 51.1% / 36.5% | **323ms / 60.1% / 43.0%** |
| multi_language | 1730ms / 16.0% / 9.9% | 1340ms / 32.1% / 19.8% | **1250ms / 33.8% / 20.9%** |
| reverb_heavy | 805ms / 28.4% / 10.1% | **800ms** / 22.7% / 8.1% | 881.5ms / 26.3% / **19.8%*** |

**Of these three**, `lyrics-alignment-mtl` wins 5 of 6 categories on median
and on conditional %≤400ms; on the comparable **gold-normalized** column it
wins **all six of the three-way comparisons**, including `reverb_heavy`
(19.8% vs 8.1–10.1%). Against the full five-backend field it is first on 4
of 6 — `elevenlabs-fa` takes `clean_pop` and `reverb_heavy`; see the
sibling report's four-backend table and the Ranked verdict below.
`ctc-star`'s apparent
`reverb_heavy` conditional lead (22.7% vs MTL's 26.3% — actually a loss)
and CTC's 28.4% are both computed over a 35.5%-coverage pool; that
inversion is the denominator trap, not an alignment-quality finding.

**Clears the 400ms bar on MEDIAN**: `lyrics-alignment-mtl` on 3 of 6
categories (clean_pop 260ms, dense_vocal 332ms, instrumental_breaks 323ms);
`ctc-star` on 2 of 6 (clean_pop 272.5ms, instrumental_breaks 390ms).
`chant_repetition`, `multi_language`, and `reverb_heavy` never clear on
median under any variant — the same three hardest categories the prior
combine-experiment report identified.

### The three hardest categories under the monotonic view (added 2026-08-06)

Median Δ, order-respecting pairs only:

| Category | `ctc` | `ctc-star` | `lyrics-alignment-mtl` | Pairs kept (MTL) |
|---|---:|---:|---:|---:|
| chant_repetition | 1294ms | 1162ms | 874ms | 155 of 296 (−47.6%) |
| multi_language | 1093.5ms | 775ms | **302ms ✅** | 136 of 237 (−42.6%) |
| reverb_heavy | 700ms | 730ms | 650ms | 125 of 186 (−32.8%) |

**One genuine exception to "the three hardest categories never clear
400ms": `multi_language` DOES clear it for `lyrics-alignment-mtl` (302ms)
— and for the sibling `elevenlabs-fa` (351.5ms) — once out-of-order pairs
are removed.** A large part of that category's apparent difficulty is the
matcher mis-pairing lines across languages, not the aligner mistiming
them. **Caveat, load-bearing: the monotonic view throws away 42.6% of that
category's pairs**, and it removes large deltas from numerator and
denominator together — so this is a *lower bound on the true difficulty*,
not a pass. `chant_repetition` and `reverb_heavy` fail under every backend
and every view, and `chant_repetition` gets *worse* under monotonic for
every aligner (43.7–48.6% pair attrition): repeated-phrase mis-pairing
there is genuinely bidirectional, not a one-sided matcher artifact.

**`reverb_heavy`* is not an apples-to-apples comparison between CTC and
MTL.** `p74PDWAFk0A` (one of `reverb_heavy`'s 3 fixtures) contains the
literal digit token `"20"` in its reference text — the exact upstream CTC
crash described above — so `ctc-forced-aligner`'s whole `p74PDWAFk0A` output
fell back to all-untimed, dropping that fixture's lines out of CTC's
`reverb_heavy` pool entirely (CTC: 35.5% coverage / 14 conservative pairs
for the category; MTL, which has no such crash: 75.0% coverage / 38
conservative pairs). CTC's worse `reverb_heavy` number is a real, honestly
measured consequence of the digit-crash limitation, not a same-condition
alignment-quality comparison.

## Untimed-line rate — did it actually drop to ~0?

**`lyrics-alignment-mtl`: yes, exactly 0.0% (0/1553 lines across 21
fixtures)** — genuinely zero, every single reference line received a real
timestamp on every fixture it ran on, including the digit-containing line
`p74PDWAFk0A` never crashed at all for this aligner (its acoustic model
operates over phonemes derived from the ALREADY-filtered word list, with no
CTC-vocabulary character-matching step to trip on a bare digit).

**`ctc-forced-aligner` (both configs): 10.4% (162/1553)** — **worse, not
better, than the ASR-combiner baseline's corrected 6.2%** (the original
"a real improvement over 20.2%" was measured against the poisoned-fixture
figure — correction #1), and not "near 0" as forced
alignment should structurally achieve. The overwhelming majority of this
162-line total is **one single fixture** (`p74PDWAFk0A`, 162 lines, the
entire fixture) falling back to all-untimed because of the digit-crash
limitation described above — this is a genuine, fixable-in-principle
upstream limitation (pre-filter/route around raw digits before calling
`get_spans`), not evidence that CTC forced alignment itself leaves lines
untimed under normal operation. Every other of the 21 fixtures got 0%
untimed under CTC.

## Did `--star_frequency segment` help or hurt?

**Helped substantially, on every measured axis:**

Both configs share the same denominator (1138/1139 matched, 1702 gold,
1553 reference lines), so for this ONE comparison the conditional and
gold-normalized columns tell the same story:

| Metric | no star | `segment` | Δ |
|---|---:|---:|---:|
| Cond. % ≤400ms (official) | 29.4% | 41.7% | **+12.3 pts** |
| **Gold-norm. % ≤400ms (official)** | **19.7%** | **27.8%** | **+8.1 pts** |
| Median Δ (official) | 900ms | 650ms | **−250ms (−28%)** |
| Cond. % ≤400ms (conservative) | 21.4% | 35.2% | **+13.8 pts** |
| Gold-norm. % ≤400ms (conservative) | 4.5% | 7.3% | +2.8 pts |
| Median Δ (conservative) | 1570ms | 1090ms | **−480ms (−31%)** |
| Mono. cond. % ≤400ms | 31.8% | 47.7% | **+15.9 pts** |
| Mono. median Δ | 755ms | 460ms | **−295ms (−39%)** |
| p90 Δ (official) *(not quotable)* | 8338ms | 10817ms | +2479ms (worse) |
| Untimed % | 10.4% | 10.4% | no change |

The `<star>` wildcard token (absorbing audio with no matching reference
text — worship ad-libs/vamps the LLM didn't transcribe) measurably improves
BOTH the median and the ≤400ms hit rate, consistent with the task's stated
rationale. The one metric that got WORSE is p90 (the tail) — a plausible
trade: `star` frees the alignment path to "skip" un-transcribed audio, which
occasionally lets a bad early skip cascade into a large downstream error on
an already-hard fixture, even as it helps the median case. Untimed % is
unaffected either way (untimed comes from the digit-crash limitation, which
`star_frequency` doesn't touch). **Verdict: use `segment`, not the default.**

## The poisoned fixture (`Xvm4_fWkXe8`) — same finding for both aligners, a real, important difference from the ASR-combiner approach

Both `ctc-forced-aligner` and `lyrics-alignment-mtl` produced all 395
reference lines with a timestamp — **0.0% untimed on this fixture, for
both** — and the timing is essentially useless: 0.0% within 400ms (either
view), 0.0% within 1000ms, median delta over 100 SECONDS (106.8s CTC no-star
/ 119.0s CTC-star / 122.6s MTL, conservative view, n=6 uniquely-matchable
gold lines).

**This is the single most important qualitative finding of this shootout.**
The earlier ASR-word-combiner approach (`combine_lines_times.py`,
2026-08-05-combine-experiment.md) left 284 of this fixture's 395 lines
UNTIMED specifically BECAUSE the monotonic word-alignment ran out of real
audio to match once the genuine occurrences of each hallucinated phrase were
exhausted — an honest "I don't know" for the excess hallucinated content. A
**forced aligner cannot express that.** By construction, CTC forced
alignment (and this DP-based aligner too) walks the ENTIRE given text and
produces exactly one span per word — there is no notion of "this text
probably isn't really sung here." Both aligners here spread all 395 lines'
worth of (mostly hallucinated) text confidently and monotonically across the
real ~280.7s of audio, producing plausible-LOOKING but almost entirely WRONG
timestamps rather than admitting uncertainty.

**Practical implication for a production pipeline**: a forced aligner is
only as trustworthy as its input text. It is not, by itself, a safety net
against an upstream transcription hallucination the way the ASR-combiner's
"leave it untimed" behavior was — a length/repetition-loop guard on the
LLM transcription step (already flagged as a needed follow-up in the prior
combine-experiment report) remains necessary regardless of which timing
method is used downstream.

## Side-by-side: `5JW87KKDTcU` (dense_vocal), gold vs `lyrics-alignment-mtl`, first 12 lines

| Gold start–end | Gold text | MTL start–end | MTL text | Δ (line 1-7) |
|---:|---|---:|---|---:|
| 5110–9010 | Nothing excites us like Jesus | 5259–7558 | Nothing excites us like Jesus | 149ms |
| 9010–12840 | 'Cause in His presence there's freedom | 8882–12643 | 'Cause in His presence there's freedom | 128ms |
| 12840–15860 | All of our sin is forgiven | 12817–15256 | All of our sin is forgiven | 23ms |
| 15860–20330 | So we give to Him the highest praise | 15290–20097 | So we give to Him the highest praise | 570ms |
| 20330–24120 | It's the greatest feeling | 20306–23893 | It's the greatest feeling | 24ms |
| 24120–27630 | When You fill this place | 23998–27585 | When You fill this place | 122ms |
| 27630–31610 | In this moment, You are moving | 27794–29536 | In this moment | 164ms |
| 31610–34970 | As we give You praise | 29605–34969 | You are moving as we give You praise | — |
| 34970–36720 | Come right now | 35039–38487 | Come right now Holy Spirit | — |
| 36720–38730 | Holy Spirit | 38557–40612 | Release Your power | — |
| 38730–40430 | Release Your power | 40647–44095 | Lord we are hungry for more of You | — |
| 40430–42180 | Lord, we are hungry | 44164–48065 | Heaven's open, You're bursting through | — |

Lines 1-7 track gold to within **23-570ms** (6 of 7 within 400ms). The
divergence from line 7 onward is a **line-boundary** disagreement —
`qwen35-omni`'s own line segmentation splits/merges differently than gold's
lrclib sync from here — not a timing failure of the aligner: each MTL line's
own `start_ms` still lands close to where ITS words are actually sung, it's
just grouped into a different-shaped line than gold expected. Same pattern,
same fixture, as the prior combine-experiment report's side-by-side —
line-boundary disagreement, not timing, is the dominant remaining error
source once a real aligner is used.

## Ranked verdict

*(Rewritten 2026-08-06 against the corrected numbers. The ORDER is
unchanged; the margins and the 2nd-place claim were both wrong.)*

**1st — `lyrics-alignment-mtl` (MTL+BDR). Best of the field on every
accuracy view, by a SMALL margin, and one of only two backends with 0%
untimed.** 43.5% conditional / **31.6% gold-normalized** ≤400ms, 528ms
median, **0.0% untimed**, joint-best coverage (72.6%). It is also first on
both monotonic views (50.5% cond. / 23.8% gold-norm. / 387.5ms median). No
correction, alone or combined, dethrones it on any pooled accuracy view —
**the selection decision is safe**.

Per category it is first on 4 of 6 against the whole field on the
gold-normalized column (`chant_repetition` 26.5%, `dense_vocal` 37.3% tied
with `elevenlabs-fa`, `instrumental_breaks` 43.0%, `multi_language`
20.9%), and second on the other two — `elevenlabs-fa` takes `clean_pop`
(77.9% vs 73.1%) and `reverb_heavy` (21.0% vs 19.8%). Against the two CTC
configs alone (the three-way table above) it is first on all six. The
originally published "wins 5 of 6 categories" was measured on that
three-way table; against the five-backend field the honest figure is 4 of
6.

Two honest caveats that were absent from the original verdict:

- **It is not first on literally every cell.** On the *conservative*
  view's conditional %≤400ms, `ctc-forced-aligner-star` leads 35.2% vs
  34.8% — the single accuracy cell in the matrix where MTL is 2nd. It
  reverses on the gold-normalized twin (7.8% vs 7.3%), so it is a
  denominator artifact, not a real loss; but the original claim of a clean
  sweep was overstated.
- **It is LAST on runtime, by two orders of magnitude.** GPU-normalized
  **106.0s/song** — `ctc-forced-aligner-star` at 1.4s is **75.7× faster**,
  and `elevenlabs-fa` at 6.9s is **15.4× faster**. MTL buys +1.3
  conditional / +0.9 gold-normalized points over `elevenlabs-fa` for ~15×
  the latency, a local GPU, and a demonstrated CUDA-OOM path that already
  fired on 2 of 21 fixtures. A real production consideration, explicitly a
  tiebreaker rather than a selection criterion per the task's framing —
  but the corrected margins make the tiebreaker matter much more than the
  original 2.1-point gap suggested.

**Exact 1st-vs-2nd margins** (across this report and the sibling
`elevenlabs-fa` one, all on the same denominators):

| View | 1st | 2nd | Margin |
|---|---|---|---:|
| Conditional %≤400ms | `lyrics-alignment-mtl` 43.5% | `elevenlabs-fa` 42.2% **=** Baseline 42.2% (tie) | 1.3 pts |
| **Gold-normalized %≤400ms** | `lyrics-alignment-mtl` 31.6% | `elevenlabs-fa` 30.7% | **0.9 pts** |
| Gold-norm. over all 1867 | `lyrics-alignment-mtl` 28.8% | `elevenlabs-fa` 28.0% | 0.8 pts |
| Monotonic cond. %≤400ms | `lyrics-alignment-mtl` 50.5% | `elevenlabs-fa` 49.4% | 1.1 pts |
| Monotonic gold-norm. %≤400ms | `lyrics-alignment-mtl` 23.8% | `elevenlabs-fa` 23.6% | **0.2 pts** ← thinnest |
| Median Δ | `lyrics-alignment-mtl` 528ms | **Baseline 570ms** (`elevenlabs-fa` 3rd at 625ms) | 42ms |
| Coverage | `lyrics-alignment-mtl` 72.6% | `elevenlabs-fa` 72.6% | exact tie |
| Untimed % | `lyrics-alignment-mtl` 0.0% | `elevenlabs-fa` 0.0% | exact tie |
| Conservative cond. %≤400ms | **`ctc-forced-aligner-star` 35.2%** | `lyrics-alignment-mtl` 34.8% | MTL is 2nd |
| Mean runtime/song | **`ctc-forced-aligner-star` 1.4s** | `ctc-forced-aligner` 1.5s (**MTL last, 106.0s**) | 0.1s |

**2nd — `elevenlabs-fa` (hosted API, sibling report).** Ties the baseline
on conditional %≤400ms, beats it on gold-normalized (30.7% vs 29.6%), and
matches MTL exactly on coverage and untimed (72.6% / 0.0%) — for 6.9s/song
with no install and no GPU. Its weak spot is the conservative-view median
(1470ms, the worst of any real aligner) and a partially-verified runtime
figure; see that report.

**3rd — the ASR-combiner baseline itself.** Corrected, it is **not** the
floor this shootout assumed: 42.2% conditional / 29.6% gold-normalized and
the **second-best median of the whole field (570ms, ahead of
`elevenlabs-fa`'s 625ms)**. It loses on coverage (70.0%) and untimed
(6.2%), which is exactly what a forced aligner is supposed to fix.

**4th — `ctc-forced-aligner-star` (`--star_frequency segment`).** Behind on
% ≤400ms (41.7% cond. / 27.8% gold-norm. vs MTL's 43.5% / 31.6%), on median
(650ms vs 528ms) and on untimed rate (10.4% vs 0.0%, mostly one fixture's
digit-crash limitation rather than a structural aligner-quality
difference); its p90 tail is also worse, not better (10817ms vs MTL's
7289ms). **It does NOT beat the ASR-combiner baseline** — the original
report's claim that it did was an artifact of the poisoned fixture being
pooled into the baseline only (correction #2). What it does have is
throughput: 1.4s/song, single warm process, model loaded once — the
practical choice if wall-clock at catalog scale (200+ songs) outweighs
~4 gold-normalized points of accuracy.

**5th — `ctc-forced-aligner` (no star, default).** Clearly worse than its
own `--star_frequency segment` variant on every accuracy metric measured;
no reason to use the default over `segment` for this workload.

**Does the winner clear the 400ms bar?** Partially, and by less than
originally reported. `lyrics-alignment-mtl` leads the primary
%-within-400ms metric (43.5% vs the corrected baseline's 42.2% — a
**1.3-point** lead, not the 2.1 points published against the wrong
baseline) and clears 3 of 6 category medians, but the POOLED median
(528ms), the gold-normalized rate (31.6% — i.e. **fewer than a third of
gold lines are inside the gate**) and the conservative view's %≤400ms
(34.8%) all sit on the wrong side of the bar. **All three metrics the task
asked to beat (%≤400ms, median, untimed%) are still beaten simultaneously
only by `lyrics-alignment-mtl`** — but `ctc-star` now beats the corrected
baseline on NONE of the three, where it previously appeared to beat two.

**Where it still fails**: `chant_repetition` (751ms median, 34.8% cond. /
26.5% gold-norm.), `multi_language` (1250ms, 33.8% / 20.9%), and
`reverb_heavy` (881.5ms, 26.3% / 19.8%) never clear 400ms on median under
any aligner tested on the official view — the sole exception, added
2026-08-06, is `multi_language` under the monotonic view (302ms for MTL,
351.5ms for `elevenlabs-fa`), which discards 42.6% of that category's pairs
and is therefore a lower bound on its difficulty, not a pass. The first two are
substantially TEXT-matching/line-boundary problems (repeated/chant content
confuses gold-line disambiguation; `multi_language`-category fixtures carry
their own line-segmentation quirks) inherited from `qwen35-omni`'s own line
splitting, not aligner-timing-accuracy problems — the aligner correctly
times whatever line boundaries it's given, but a badly-drawn line boundary
still produces a "wrong" `start_ms` relative to gold's differently-drawn
line. The poisoned fixture is the sharpest illustration of the same root
cause taken to its extreme: forced alignment cannot fix bad input text, it
can only faithfully (and, on hallucinated text, faithfully WRONGLY) time it.

## Reproducing this experiment

```bash
# Score all three variants (raw output already committed under
# eval/lyrics/reports/2026-08-05-aligner-raw/):
python3 -m eval.lyrics.score_aligner \
  --backends ctc-forced-aligner ctc-forced-aligner-star lyrics-alignment-mtl \
  --raw-dir eval/lyrics/reports/2026-08-05-aligner-raw \
  --out eval/lyrics/reports/2026-08-05-aligner-scores.json
```

Re-running the aligners themselves on win-resolume: see the "Install" and
CLI sections of `eval/lyrics/aligners/ctc_forced_aligner/README.md` and
`eval/lyrics/aligners/lyrics_alignment_mtl/README.md`.

The corrected scorers land the gold-normalized, monotonic and
device-split-runtime fields directly in
`reports/2026-08-05-aligner-scores.json`; the baseline row comes from
`reports/2026-08-05-combine-scores.json`
(`combos → combo-qwen35-omni-lines_aai-u35-translate-times`) and the
`elevenlabs-fa` row from `aligners_11l/scores.json`. Re-running the command
above regenerates all of them byte-identically from the committed raw
artifacts — that determinism is what let this report be corrected without a
single new API call.

Unit tests: `pytest eval/lyrics/tests/test_score_aligner.py` (11 tests:
untimed-line filtering, missing-output handling, poisoned-fixture exclusion
from aggregates while still being individually scored, the gold-normalized
denominator, and the device-split runtime rollup). Whole eval suite:
`pytest eval/lyrics/tests` — 115 passing.
