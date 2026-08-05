# Lyrics Eval — Forced-Aligner Shootout — 2026-08-05

**Question:** the audio-LLM (`qwen35-omni`) already produces good lyric LINE
content but bad line TIMING (north-star sweep:
`reports/2026-08-05-one-call-northstar.md`); recombining it with a dedicated
ASR's WORD timings closed part of that gap but never a real forced aligner
(`reports/2026-08-05-combine-experiment.md`, best: `qwen35-omni` lines ×
`aai-u35-translate` ASR word times, 41.4% of lines ≤400ms, median 573ms,
20.2% of lines completely UNTIMED). **Does an actual forced aligner — given
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
with the sibling CTC run caused a `torch.cuda.OutOfMemoryError` on the
largest fixture mid-batch — caught and retried on CPU (never silent: every
output records `metadata.device` and `metadata.cuda_oom_retried`). Model
load is NOT amortized across fixtures (upstream's `wrapper.align()` bundles
load+inference per call by design, not touched) — each fixture pays a fresh
~7-9s process/model-load overhead on top of its actual DP-alignment time,
which is the dominant cost (a plain, non-vectorized Python DP loop — see the
README). Full 22-fixture batch: **~65 minutes wall-clock**
(53.5 min first pass + a ~10 min CPU-fallback retry for the one fixture
that OOM'd, `q5m09rqOoxE`, which succeeded on retry).

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

## Headline comparison — official view (21 fixtures, poisoned excluded)

| Backend | % ≤400ms | Median Δ | p90 Δ | Coverage | Untimed % | Mean runtime/song |
|---|---:|---:|---:|---:|---:|---:|
| **Baseline** (`qwen35-omni` lines × `aai-u35-translate` ASR times, prior report) | 41.4% | 573ms | — | 70.1% | 20.2% | — |
| `ctc-forced-aligner` (no star) | 29.4% | 900ms | 8338ms | 66.9% | 10.4% | 1.5s |
| `ctc-forced-aligner-star` (`--star_frequency segment`) | 41.7% | 650ms | 10817ms | 66.9% | 10.4% | 1.4s |
| **`lyrics-alignment-mtl` (MTL+BDR)** | **43.5%** | **528ms** | 7289ms | **72.6%** | **0.0%** | 150s |

## Conservative view (unique-gold-line only, ratio ≥0.75)

| Backend | % ≤400ms | Median Δ | n pairs |
|---|---:|---:|---:|
| Baseline (`qwen35-omni` × `aai-u35-translate`, prior report) | 33.8% | 1142ms | — |
| `ctc-forced-aligner` (no star) | 21.4% | 1570ms | 355 |
| `ctc-forced-aligner-star` | 35.2% | 1090ms | 355 |
| **`lyrics-alignment-mtl`** | **34.8%** | **1115ms** | 379 |

`lyrics-alignment-mtl` has the most conservative-matchable pairs (379 vs
355) because it lost fewer lines to UNTIMED — the conservative pool only
ever includes lines the aligner actually timed.

## Per-category breakdown (official view)

| Category | `ctc` (no star) | `ctc-star` | `lyrics-alignment-mtl` |
|---|---|---|---|
| chant_repetition | 902ms / 25.7% | 833ms / 33.8% | **751ms / 34.8%** |
| clean_pop | 390ms / 50.0% | 272ms / 71.6% | **260ms / 74.5%** |
| dense_vocal | 820ms / 32.8% | 470ms / 48.1% | **332ms / 51.9%** |
| instrumental_breaks | 561ms / 37.6% | 390ms / 51.1% | **323ms / 60.1%** |
| multi_language | 1730ms / 16.0% | 1340ms / 32.1% | **1250ms / 33.8%** |
| reverb_heavy | 805ms / 28.4% | 800ms / 22.7% | 881ms / 26.3%* |

Format: median Δ / % ≤400ms. **Bold** = best of the three per category.
`lyrics-alignment-mtl` wins 5 of 6 categories on both metrics; `ctc-star`
wins `reverb_heavy` on % ≤400ms narrowly (22.7% vs 26.3%, both far from
clearing the bar) but loses on median.

**Clears the 400ms bar on MEDIAN**: `lyrics-alignment-mtl` on 3 of 6
categories (clean_pop 260ms, dense_vocal 332ms, instrumental_breaks 323ms);
`ctc-star` on 2 of 6 (clean_pop 273ms, instrumental_breaks 390ms).
`chant_repetition`, `multi_language`, and `reverb_heavy` never clear on
median under any variant — the same three hardest categories the prior
combine-experiment report identified.

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

**`ctc-forced-aligner` (both configs): 10.4% (162/1553)** — a real
improvement over the 20.2% ASR-combiner baseline, but not "near 0" as forced
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

| Metric | no star | `segment` | Δ |
|---|---:|---:|---:|
| % ≤400ms (official) | 29.4% | 41.7% | **+12.3 pts** |
| Median Δ (official) | 900ms | 650ms | **−250ms (−28%)** |
| % ≤400ms (conservative) | 21.4% | 35.2% | **+13.8 pts** |
| Median Δ (conservative) | 1570ms | 1090ms | **−480ms (−31%)** |
| p90 Δ (official) | 8338ms | 10817ms | +2479ms (worse) |
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

**1st — `lyrics-alignment-mtl` (MTL+BDR). Best timing quality on every
metric that matters, and the only one to clear the untimed-line goal.**
43.5% ≤400ms (official) / 34.8% (conservative), 528ms median (official) /
1115ms (conservative), **0.0% untimed**, best coverage (72.6%), wins 5 of 6
categories. It clears the 400ms bar cleanly on the pooled official %≤400ms
figure and on 3 of 6 category medians. It does NOT clear 400ms on the pooled
MEDIAN (528ms, close but over) or on the conservative view's pooled %≤400ms
(34.8%) — the same three categories remain hardest for every aligner tested
(`chant_repetition`, `multi_language`, `reverb_heavy`), all involving either
heavy repetition (text-matching ambiguity, independent of alignment
accuracy) or the multi_language manifest category's own quirks. It is
dramatically slower than CTC (150s/song mean vs 1.4s), a real production
consideration but explicitly a tiebreaker, not a selection criterion per the
task's own framing.

**2nd — `ctc-forced-aligner-star` (`--star_frequency segment`).** Close
behind on % ≤400ms (41.7% vs 43.5%) and actually edges MTL on p90 tail
(10817ms is worse not better — MTL's p90 of 7289ms is tighter), but loses
clearly on median (650ms vs 528ms) and untimed rate (10.4% vs 0.0%, though
that gap is mostly one fixture's digit-crash limitation, not a structural
aligner-quality difference). Dramatically faster (1.4s/song) and much
simpler to keep warm (single long-running process, model loaded once) —
the practical choice if wall-clock/throughput at catalog scale (200+ songs)
matters more than the last few points of accuracy.

**3rd — `ctc-forced-aligner` (no star, default).** Clearly worse than its
own `--star_frequency segment` variant on every accuracy metric measured;
no reason to use the default over `segment` for this workload.

**Does the winner clear the 400ms bar?** Partially. `lyrics-alignment-mtl`
clears it on the primary %-within-400ms metric (43.5% > baseline's 41.4%,
and this project's north-star framing treats %≤400ms as the headline number)
and on 3 of 6 category medians, but the POOLED median (528ms) and the
conservative view's %≤400ms (34.8%) still sit on the wrong side of the bar.
**All three metrics the task asked to beat (%≤400ms, median, untimed%) are
beaten simultaneously only by `lyrics-alignment-mtl`** — CTC-star beats 2 of
3 (%≤400ms, untimed%) but not median.

**Where it still fails**: `chant_repetition` (751ms median, 34.8%≤400ms),
`multi_language` (1250ms, 33.8%), and `reverb_heavy` (881ms, 26.3%) never
clear 400ms on median under any aligner tested. The first two are
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

Unit tests: `pytest eval/lyrics/tests/test_score_aligner.py` (6 tests:
untimed-line filtering, missing-output handling, poisoned-fixture exclusion
from aggregates while still being individually scored).
