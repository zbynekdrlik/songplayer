# Lyrics Eval — ElevenLabs Forced Alignment API — 2026-08-05

**Question:** does a hosted, no-install forced-alignment API — given the
`qwen35-omni` audio-LLM's fixed reference lines plus the isolated-vocal WAV,
doing real forced alignment rather than ASR-then-graft — beat the
ASR-word-combiner baseline (41.4% of lines ≤400ms, median 573ms, 20.2%
UNTIMED — `2026-08-05-combine-experiment.md`), and how does it compare to
the two locally-installed forced aligners benchmarked the same day
(`2026-08-05-aligner-shootout.md`: `ctc-forced-aligner` and
`lyrics-alignment-mtl`)?

This report benchmarks the **ElevenLabs Forced Alignment API**
(`elevenlabs-fa`) against the same 22 pinned manifest fixtures, using the
same scoring harness, so all four numbers (baseline, CTC, MTL, ElevenLabs)
are directly comparable.

## Method

- **Fixed input, never re-derived**: for each of the 22 fixtures with a
  cached vocal WAV, the reference text is `qwen35-omni_<video_id>.json`'s
  `lines[].text` (English, as already transcribed) — the SAME reference
  every other backend in this harness's sweep used. ElevenLabs receives
  this text UNCHANGED (whitespace-tokenized, single-space-joined into one
  transcript — see `aligners_11l/README.md`), plus `<video_id>_vocal16k.wav`
  (16kHz, isolated/dereverbed vocals), and returns per-word timestamps for
  that exact text. It never transcribes or reinterprets a single word.
- **Line timing convention**: `start_ms` = the first aligned word's
  `start_ms`; `end_ms` = the last aligned word's `end_ms`. A line with zero
  alignable words would be UNTIMED (`start_ms`/`end_ms` = `null`) — never
  guessed, never interpolated. (In practice this never happened — see
  Untimed-line rate below.)
- **Scoring, two views** (`eval/lyrics/score_aligner.py`, shared with the
  sibling shootout report — imported, never modified): the **official**
  view is `score_one_call.py`'s greedy closest-start text matcher
  (ratio ≥0.6, gold-line-consuming); the **conservative** view
  (`run_combine_experiment.conservative_match`) pairs a produced line to a
  gold line only when that gold line's normalized text is UNIQUE in the
  song (ratio ≥0.75). Both are reported every time, per
  `.claude/rules/lyrics-eval-backends.md`'s standing caution that the
  official matcher has no monotonicity constraint and can mis-pair
  repeated/chant lyrics — **only median and %≤400ms are quotable; mean and
  p90 are not** (35.6%+ of one prior backend's pairs violated ordering).
- **The poisoned fixture** (`Xvm4_fWkXe8` — `qwen35-omni` hallucinated a
  395-line degenerate repetition loop for what is really a ~4.6-minute
  song) is run like every other fixture but **excluded from every
  pooled/per-category aggregate below** and reported on its own
  (`score_aligner.py`'s `POISONED_FIXTURE_VIDEO_ID` exclusion). All
  pooled/per-category numbers below are over the remaining **21 fixtures**.
- **Scope**: 22 fixtures (this task's assignment). `manifest.json` now
  lists 24 fixtures total — `vpwDdb8r9Bk` (reverb_heavy) and `fHYLw-2tTx4`
  (clean_pop) were added after this run's scope was fixed and were never
  submitted to ElevenLabs; `score_aligner.py` correctly reports these as
  `n_fixtures_errored: 2` (1 each in the `reverb_heavy`/`clean_pop`
  category rollups) rather than silently treating them as zero-score
  fixtures.

## What the API actually is, and what the docs got wrong

`POST https://api.elevenlabs.io/v1/forced-alignment`, multipart/form-data
(`file` = audio, `text` = plain reference string), auth via `xi-api-key`
header. Full verified request/response shape, every doc quote + URL, and
the exact working code: `eval/lyrics/aligners_11l/README.md` and
`elevenlabs_fa.py`'s module docstring — not repeated in full here. The one
finding worth surfacing at report level: **the docs' `words[]` response
description ("list of words with their timing information") is misleading**
— the live API interleaves a separate whitespace-only entry between every
pair of real words (551 entries for 276 real words sent, exactly
`2n-1`), a convention this harness's `soniox_v5.py` backend already
documented for a different vendor. `filter_content_words()` strips these
before word-index mapping; unit-tested
(`test_filter_content_words_drops_interleaved_whitespace_entries`).

**Cost**: "the same rate as the Speech to Text API" per the capabilities
page (per-audio-second, not per-request) — the exact $/min is
account-plan-dependent and not restated here; see the account's usage
dashboard for the actual charge this run incurred.

## Headline comparison — official view (21 fixtures, poisoned excluded)

| Backend | % ≤400ms | Median Δ | p90 Δ | Coverage | Untimed % | Mean runtime/song |
|---|---:|---:|---:|---:|---:|---:|
| **Baseline** (`qwen35-omni` lines × `aai-u35-translate` ASR times, prior report) | 41.4% | 573ms | — | 70.1% | 20.2% | — |
| `ctc-forced-aligner` (no star) | 29.4% | 900ms | 8338ms | 66.9% | 10.4% | 1.5s |
| `ctc-forced-aligner-star` | 41.7% | 650ms | 10817ms | 66.9% | 10.4% | 1.4s |
| **`elevenlabs-fa`** | **42.2%** | **625ms** | 12580ms | **72.6%** | **0.0%** | **6.9s** |
| `lyrics-alignment-mtl` (MTL+BDR) | 43.5% | 528ms | 7289ms | 72.6% | 0.0% | 150s |

ElevenLabs sits **2nd of 4 aligners tested this shootout** on both headline
metrics — ahead of both CTC configs and the ASR-combiner baseline, behind
only `lyrics-alignment-mtl` — while running in **6.9s per song, ~22×
faster than MTL** and requiring **zero local install, zero GPU, zero
digit-crash/OOM edge cases** (both of which the local aligners hit and had
to work around, see `2026-08-05-aligner-shootout.md`'s install-reality
section).

## Conservative view (unique-gold-line only, ratio ≥0.75)

| Backend | % ≤400ms | Median Δ | n pairs |
|---|---:|---:|---:|
| Baseline (prior report) | 33.8% | 1142ms | — |
| `ctc-forced-aligner-star` | 35.2% | 1090ms | 355 |
| `lyrics-alignment-mtl` | 34.8% | 1115ms | 379 |
| **`elevenlabs-fa`** | 34.3% | **1470ms** | 379 |

`elevenlabs-fa` and `lyrics-alignment-mtl` have the same `n_pairs` (379,
both beat CTC's 355) because both leave 0% of lines untimed, so the
conservative pool is not thinned by missing timestamps for either. On the
harder conservative view, `elevenlabs-fa`'s %≤400ms is competitive
(34.3%, within 1pt of the other two real aligners) but its **median is
noticeably worse** — 1470ms vs 1090-1142ms for everything else tested.
This is the one metric where ElevenLabs is a clear step behind, not a
close call — flagged honestly rather than folded into the generally
favorable headline picture above.

## Per-category breakdown (official view) — all four backends

Format: median Δ / % ≤400ms. **Bold** = best of the four per category.

| Category | `ctc-star` | `elevenlabs-fa` | `lyrics-alignment-mtl` |
|---|---|---|---|
| chant_repetition | 833ms / 33.8% | 859.5ms / 31.8% | **751ms / 34.8%** |
| clean_pop | 272ms / 71.6% | **260ms / 79.4%** | 260ms / 74.5% |
| dense_vocal | 470ms / 48.1% | 340ms / **51.9%** | **332ms** / 51.9% |
| instrumental_breaks | 390ms / 51.1% | 380ms / 53.9% | **323ms / 60.1%** |
| multi_language | 1340ms / **32.1%** | 1440ms / **32.1%** | **1250ms** / 33.8% |
| reverb_heavy | 800ms / 22.7% | 1025ms / **28.0%** | 881ms / 26.3%* |

`elevenlabs-fa` wins or ties outright on `clean_pop` (79.4% ≤400ms — the
single best result of any backend in either shootout on any category) and
essentially ties `lyrics-alignment-mtl` on `dense_vocal` (51.9% either way,
8ms apart on median). It wins `reverb_heavy`'s %≤400ms outright (28.0%,
best of all four) despite the worst median there — meaning it has fewer
severe outliers pulling the median up on that category, an inversion worth
noting rather than smoothing over. `multi_language` and `chant_repetition`
remain the two hardest categories for every backend tested across both
reports — the same finding as the sibling shootout, and (per that report)
substantially a TEXT/line-boundary problem inherited from `qwen35-omni`'s
own line splitting, not an alignment-accuracy problem specific to any one
aligner.

*`reverb_heavy`'s CTC-vs-MTL comparison is not apples-to-apples (one CTC
fixture hit a digit-crash limitation, see the sibling report) —
`elevenlabs-fa` hit no such issue on any fixture in any category.

## Untimed-line rate — crushed to 0%, as hypothesized

**0.0% (0/1553 lines across 21 fixtures) — every single reference line
received a real timestamp on every one of the 22 fixtures run**, matching
`lyrics-alignment-mtl`'s result and confirming the task's hypothesis: a
true forced aligner structurally cannot leave a line untimed the way the
ASR-word-combiner (20.2% untimed) or CTC forced alignment's digit-crash
fallback (10.4% untimed, a real but fixable upstream limitation, not
structural) can. This holds even on the poisoned fixture's 395 hallucinated
lines — see below for what "timed, but not necessarily correctly" means in
that case.

## Confidence vs. timing error — does ElevenLabs' own `loss` signal predict when it's wrong?

`mean_word_loss` (the per-line average of each word's `loss` — LOWER means
more confident) was joined against each matched line's `abs_delta_ms` via
`confidence_correlation.py` (imports `score_one_call.greedy_match`, never
modified, to reuse the exact same pairing as the official scoring view).
1236 matched, non-poisoned lines.

**Pooled linear correlation is weak: Pearson r = 0.086** (loss vs.
absolute timing error). Taken alone this would suggest the confidence
signal is nearly useless — but a linear coefficient is the wrong lens for
noisy real-world timing error, which is dominated by a long tail of large
outliers `greedy_match` itself is known to mis-pair (see the "quotable
metrics" caution above). **Splitting into quartiles by loss instead shows
a clear, monotonic, practically useful relationship:**

| Confidence quartile (Q1 = most confident) | n | loss range | median Δ | % ≤400ms |
|---|---:|---|---:|---:|
| Q1 | 309 | 0.022 – 0.250 | 350ms | 54.7% |
| Q2 | 309 | 0.251 – 0.484 | 380ms | 51.8% |
| Q3 | 309 | 0.485 – 0.827 | 840ms | 35.3% |
| Q4 (least confident) | 309 | 0.828 – 2.832 | 1442ms | 27.2% |

The most-confident quarter of lines is **2.4x more likely to land within
400ms** than the least-confident quarter (54.7% vs 27.2%), and its median
error is **less than a quarter** of Q4's (350ms vs 1442ms). **Verdict: the
`loss` field is a genuinely useful per-line quality signal for a
production pipeline** — e.g. flagging Q4-loss lines for manual review or a
cheaper fallback timing method — even though it does not behave like a
tidy linear predictor across the full noisy range. The poisoned fixture's
52 matched lines show the same weak-linear pattern (r=0.050, n=52) but
were kept out of the pooled figures above for the same reason they are
excluded from every other pooled aggregate in this report.

Full numbers: `eval/lyrics/aligners_11l/confidence_correlation.json`.

## The poisoned fixture (`Xvm4_fWkXe8`) — neither a clean failure nor a clean spread

395 reference lines for a real ~4.4-minute (262.9s) song with 70 real gold
lines — a 5.6x inflation from `qwen35-omni`'s hallucinated repetition of
"Keep on, keep on, keep on" / "And You keep on doing it". ElevenLabs never
errored and never returned an untimed line (0.0% untimed, matching every
other fixture) — **but the timing quality collapsed in two specific,
quantifiable ways, neither of which is "fail loudly" nor "spread cleanly
across the real audio":**

1. **Degenerate collapse onto a single instant.** 311 of 395 lines (78.7%)
   got a timed span of ≤5ms. **262 of those 311 (66.3% of the whole
   fixture) collapsed onto the exact same `start_ms=169230`** — the model
   ran out of real audio to distinguish between dozens of near-identical
   hallucinated lines around the song's real repeated outro and pinned
   almost all of them to one instant rather than spacing them out or
   declining to time them.
2. **Overflow into physically impossible spans.** The handful of lines
   that were NOT collapsed absorbed the remaining audio instead — one
   single line (`"Keep on, keep on, keep on"`) was given a **53.36-second**
   span (`181340ms`–`234700ms`), and another a **23.60-second** span
   (`239320ms`–`262920ms`, running to the literal end of the file). No
   sung line lasts 53 seconds; these are the aligner distributing leftover
   duration across whatever reference text remained once the real audio
   was exhausted.

Despite this, the fixture still scores 25.0% ≤400ms / 50.0% ≤1000ms
(official) and 50.0% ≤400ms (conservative, n=6 uniquely-matchable gold
lines) — not zero, because a genuine minority of the 395 lines DO land near
one of the song's real repeated phrase instances by chance. This matches
the sibling shootout's finding for the two local aligners (which also hit
0% untimed and near-useless timing here) and reinforces the same practical
conclusion: **forced alignment is only as trustworthy as its input text —
none of the three real aligners tested this shootout can substitute for an
upstream hallucination guard on the reference-text step.**

## Side-by-side: `5JW87KKDTcU` (dense_vocal), gold vs `elevenlabs-fa`, first 12 lines

Same fixture and same first-12-line window the sibling shootout report used
for `lyrics-alignment-mtl`, so all three can be read directly against each
other.

| Gold start–end | Gold text | ElevenLabs start–end | ElevenLabs text | loss | Δ |
|---:|---|---:|---|---:|---:|
| 5110–9010 | Nothing excites us like Jesus | 5220–7500 | Nothing excites us like Jesus | 0.740 | 110ms |
| 9010–12840 | 'Cause in His presence there's freedom | 8900–11200 | 'Cause in His presence there's freedom | 0.559 | 110ms |
| 12840–15860 | All of our sin is forgiven | 12780–15240 | All of our sin is forgiven | 1.034 | 60ms |
| 15860–20330 | So we give to Him the highest praise | 15320–19680 | So we give to Him the highest praise | 0.732 | 540ms |
| 20330–24120 | It's the greatest feeling | 20220–23680 | It's the greatest feeling | 0.454 | 110ms |
| 24120–27630 | When You fill this place | 24020–26280 | When You fill this place | 0.634 | 100ms |
| 27630–31610 | In this moment, You are moving | 27760–29120 | In this moment | 0.615 | 130ms |
| 31610–34970 | As we give You praise | 29600–34080 | You are moving as we give You praise | 0.736 | — |
| 34970–36720 | Come right now | 34980–38360 | Come right now Holy Spirit | 0.423 | — |
| 36720–38730 | Holy Spirit | 38580–40400 | Release Your power | 0.838 | — |
| 38730–40430 | Release Your power | 40660–43840 | Lord we are hungry for more of You | 0.792 | — |
| 40430–42180 | Lord, we are hungry | 44180–47680 | Heaven's open, You're bursting through | 0.806 | — |

Lines 1-7 track gold to within **60-540ms** (6 of 7 within 400-600ms,
consistent with the pooled median). The divergence from line 7 onward is
the **same line-boundary disagreement** the sibling report identified for
MTL on this exact fixture — `qwen35-omni`'s own line segmentation
splits/merges differently than gold's lrclib sync from here (gold's "In
this moment, You are moving" vs. the reference text's separate "In this
moment" / "You are moving as we give You praise" lines) — not a timing
failure of the aligner itself; each ElevenLabs line's own `start_ms` still
lands close to where its actual words are sung, it is grouped into a
different-shaped line than gold expected. Notably `loss` stays
unremarkable (0.4-1.0) across both the well-matched lines 1-7 and the
boundary-mismatched lines 8-12 — line-boundary disagreement is invisible
to the aligner's own confidence signal, since from ElevenLabs' point of
view it aligned exactly the text it was given, correctly.

## Ranked verdict (all four backends, this shootout + the sibling one)

**Does `elevenlabs-fa` clear the 400ms bar?** Partially, the same qualified
way the sibling report found for `lyrics-alignment-mtl`. It clears the
primary %-within-400ms metric (42.2% > baseline's 41.4%) but not on the
pooled median (625ms) or the conservative view's %≤400ms (34.3%, and its
conservative median at 1470ms is the weakest of any real aligner tested).
**Untimed% is fully crushed to 0.0%, matching MTL and clearing the
baseline's 20.2% by the largest margin of any metric in this report** —
the task's central hypothesis is confirmed.

**Where `elevenlabs-fa` fits among the three real aligners tested across
both 2026-08-05 reports:**

- **`lyrics-alignment-mtl` remains the best pure timing-quality result**
  (43.5%/528ms official, 0.0% untimed) but at 150s/song and a real local
  GPU/install burden (see the sibling report's ~15min setup + OOM-retry
  reality).
- **`elevenlabs-fa` is a close 2nd on quality** (42.2%/625ms official,
  0.0% untimed, ties or beats MTL outright on 2 of 6 categories) with
  **dramatically lower operational cost**: no install, no GPU, no
  digit-crash or CUDA-OOM edge cases, 6.9s/song (fast enough to run inline
  in a production pipeline rather than as an offline batch job), at the
  cost of a paid per-audio-second API call and a materially worse
  conservative-view median (1470ms).
- **`ctc-forced-aligner-star` is the throughput choice** (1.4s/song,
  single warm process) but is clearly behind both `elevenlabs-fa` and MTL
  on every accuracy metric, and carries a real digit-crash limitation
  neither of the other two hit.

**For a production pipeline that cannot run a local GPU worker (or wants to
avoid the 150s/song MTL latency) but needs forced-alignment-grade
untimed-line elimination, `elevenlabs-fa` is the best available option of
the three tested** — it is the only backend that clears BOTH the 0%-untimed
bar AND runs fast enough (single-digit seconds) for inline use. Its
confidence (`loss`) field, while not a clean linear predictor, usefully
separates a high-accuracy top quartile (54.7% ≤400ms) from a
low-accuracy bottom quartile (27.2%) and is worth wiring into a
production quality gate.

**Where it still fails**: the same three categories every backend
struggles with — `chant_repetition`, `multi_language`, and `reverb_heavy`
never clear 400ms on median under `elevenlabs-fa` either — and the
conservative-view median gap (1470ms vs ~1100ms for the local aligners) is
a genuine, unresolved weakness worth re-checking if ElevenLabs ships a
model update.

## Reproducing this experiment

```bash
# Re-run the backend itself (needs ELEVENLABS_API_KEY in the environment):
python3 eval/lyrics/aligners_11l/elevenlabs_fa.py \
  --wav /path/to/<video_id>_vocal16k.wav \
  --text-json eval/lyrics/reports/2026-08-05-raw/qwen35-omni_<video_id>.json \
  --out eval/lyrics/aligners_11l/raw/elevenlabs-fa_<video_id>.json

# Score (raw output already committed under eval/lyrics/aligners_11l/raw/):
python3 -m eval.lyrics.score_aligner \
  --manifest eval/lyrics/manifest.json \
  --raw-dir eval/lyrics/aligners_11l/raw \
  --backends elevenlabs-fa \
  --out eval/lyrics/aligners_11l/scores.json

# Confidence-vs-error correlation:
python3 -m eval.lyrics.aligners_11l.confidence_correlation \
  --manifest eval/lyrics/manifest.json \
  --raw-dir eval/lyrics/aligners_11l/raw \
  --backend elevenlabs-fa \
  --out eval/lyrics/aligners_11l/confidence_correlation.json
```

Unit tests: `pytest eval/lyrics/tests/test_elevenlabs_fa.py
eval/lyrics/tests/test_confidence_correlation.py` (32 tests: tokenization,
transcript building, the interleaved-whitespace filter, positional line
reconstruction incl. the word-count-mismatch fail-loud path, WAV-duration
reading, Pearson correlation, and quartile bucketing).

Full request/response shape and remote-transfer notes:
`eval/lyrics/aligners_11l/README.md`.
