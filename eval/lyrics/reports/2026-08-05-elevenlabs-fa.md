# Lyrics Eval — ElevenLabs Forced Alignment API — 2026-08-05

## Corrections (2026-08-06)

An adversarial review of the eval harness on 2026-08-06 found 14 verified
defects; several invalidated numbers published in this report. The scorers
were fixed (commits `d42264e`, `b048953`) and every row below was re-scored
**from the same committed artifacts — no backend was re-run for the score
table, no API call was made for it**. The body has been amended in place;
this table is the audit trail so nothing was changed silently.

| # | Was | Is now | Cause |
|---|---|---|---|
| 1 | Baseline row **41.4%** ≤400ms / **573ms** median / **20.2%** untimed / **70.1%** coverage | **42.2%** / **570ms** / **6.2%** / **70.0%** | The poisoned fixture `Xvm4_fWkXe8` was excluded from every aligner row but pooled into the baseline. **297 of the baseline's 394 untimed lines were that one fixture's** — the entire 20.2% → 6.2% move. (`run_combine_experiment.py`, both `build_backend_report` call sites.) |
| 2 | "It clears the primary %-within-400ms metric (**42.2% > baseline's 41.4%**)" | **A TIE, not a win: 42.2% vs 42.2%.** On the comparable gold-normalized view `elevenlabs-fa` does genuinely lead, 30.7% vs 29.6%. | Same defect as #1. |
| 3 | "clears the baseline's **20.2% untimed** by the largest margin of any metric in this report" | Measured against a figure **3.3× too large**. Restated against the corrected **6.2%** — still a clean win (0.0% vs 6.2%), no longer the largest margin in the report. | Same defect as #1. |
| 4 | "**2nd of 4** … on both headline metrics — ahead of both CTC configs and the ASR-combiner baseline" | On %≤400ms it **ties** the baseline rather than leading it; **on median Δ it is 3rd, BEHIND the baseline** (625ms vs 570ms). | Same defect as #1. |
| 5 | "6.9s per song, **~22× faster than MTL**" | **~15×.** 21.9× was computed against MTL's *device-blended* 150.8s; against MTL's GPU-only **106.0s** it is 15.4×. | MTL's CUDA-OOM→CPU fallback was pooled into one runtime number (two fixtures ran on CPU at 575.4s mean). |
| 6 | The 41–44% band quoted as one comparable quality figure | Restated as **conditional on matched, timed lines** (denominator 1138–1236, different per backend) and published beside a **gold-normalized** column on the shared 1702-gold-line denominator, where the band is **19.7–31.6%**. | `score_one_call.py` divided by matched-and-timed lines only. |
| 7 | (not surfaced in this report) `scores.json` → `fixtures_with_word_timings: 0` | **An artifact of the transfer, not a capability finding.** `aligners_11l/README.md` documents that `words[]` was stripped remotely before commit to shrink the diff. The 8 re-run fixtures in `aligners_11l/raw_rerun_20260806/` carry per-line `words[]` on **every** line of all 8 songs. This word-level aligner does return word timings. | Remote `words[]` stripping, never annotated in the scored artifacts. |
| 8 | The **6.9s** runtime figure, presented as measured | **Partially verified.** The committed `elevenlabs_fa.py` could not have written the `runtime_sec` it reports (no `import time`); `d42264e` restored the instrumentation and a partial re-run confirmed it emits `runtime_sec`. Only **8 of the 21** scored fixtures have been re-measured (mean **5.74s** — identical to the same 8 in the committed set); the other 13 still come from the pre-fix-era script. | Instrumentation absent from the committed script. Full re-run blocked until the free-tier quota resets **2026-09-05** — issue **#125 (eval: elevenlabs-fa re-run blocked mid-way by exhausted free-tier quota)**. |
| 9 | Conditional-column denominator published as **1138–1243** (this table's own row #6 and the Method section) | **1138–1236.** The max `n matched` across every row in the sibling shootout (baseline 1191, `ctc` 1139, `ctc-star` 1138, `elevenlabs-fa` 1236, `lyrics-alignment-mtl` 1236) is 1236 — no `1243` value exists in any committed JSON. | Independently re-derived from `2026-08-05-aligner-scores.json` / `2026-08-05-combine-scores.json` / `aligners_11l/scores.json`. |
| 10 | Method section: "so every backend's number rises" under the monotonic view, stated as a universal law | **Usually rises, but not always** — it can FALL when the reordered greedy pass re-pairs to worse deltas (e.g. `gemini36-flash × soniox-v5` 29.3%→21.5% in the sibling combine-experiment report). Only the relative ORDER is meaningful, never the direction of change. | Overgeneralized without checking the sibling report's combo rows. |
| 11 | Method section: monotonic view "drops the 27–28% of pairs that bind backwards in the song" — read as though 27–28% IS the drop rate | **27–28% of pairs bind backwards; rejecting them cascades and removes 34–39% of all pairs** overall (see the sibling shootout report's "discards 34–39% of all pairs") — two different numbers, not one. | Conflated the backwards-binding rate with the total pairs-dropped rate. |

**What did NOT change** — re-derived and confirmed identical:
`elevenlabs-fa`'s own conditional 42.2% ≤400ms, 625ms median, 72.6%
coverage, 0.0% untimed, every per-category figure, the conservative view
(34.3% / 1470ms / 379 pairs) and the whole confidence-quartile analysis.
The 8 re-run fixtures are **byte-identical to the committed ones** on every
line's `start_ms` / `end_ms` / `text`, which is why finding #8 touches only
the runtime figure and none of the accuracy figures.

**Question:** does a hosted, no-install forced-alignment API — given the
`qwen35-omni` audio-LLM's fixed reference lines plus the isolated-vocal WAV,
doing real forced alignment rather than ASR-then-graft — beat the
ASR-word-combiner baseline (**42.2% of matched-and-timed lines ≤400ms,
median 570ms, 6.2% UNTIMED** — `2026-08-05-combine-experiment.md`,
corrected 2026-08-06 per the table above), and how does it compare to
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
- **Denominators — identical for every row in this report, and stated on
  purpose** (see correction #6): 21 fixtures scored, **2 errored**,
  poisoned fixture excluded. **1553** `qwen35-omni` reference lines
  submitted; **1702** gold lines in the scored fixtures; **1867** gold
  lines counting the two errored ones.
  - **Conditional %≤400ms** = within-400ms ÷ *matched-and-timed lines*.
    That denominator is **1138–1236 and differs per backend** — each
    backend is graded only on the subset it handled. It is **not** "% of
    lines correctly timed", and it is not comparable across backends on
    its own.
  - **Gold-normalized %≤400ms** = within-400ms ÷ *the same 1702 gold
    lines*, for every backend. **This is the comparable figure.**
  - **Monotonic** = the same matcher re-run with an ordering constraint.
    Roughly 27–28% of pairs bind backwards in the song; rejecting them
    cascades and removes 34–39% of all pairs overall — two different
    numbers, not one (correction #11). It removes large deltas from
    numerator and denominator together, which usually RAISES each
    backend's number — but not always: it can FALL when the reordered
    greedy pass re-pairs to worse deltas (e.g. `gemini36-flash × soniox-v5`
    falls 29.3%→21.5% in the sibling combine-experiment report; correction
    #10). Only the relative ORDER is meaningful, never the direction of
    change.

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

## Headline comparison — official view (21 fixtures scored, 2 errored, poisoned excluded)

**Every row shares one denominator set: 1553 reference lines in, 1702 gold
lines in the scored fixtures, 21 scored / 2 errored.** The two %-columns
must be read together — see the Method section's denominator block.

| Backend | Cond. %≤400ms *(matched+timed only)* | **Gold-norm. %≤400ms** *(of 1702)* | Median Δ | Coverage | Untimed % | n matched | Mean runtime/song |
|---|---:|---:|---:|---:|---:|---:|---:|
| **Baseline** (`qwen35-omni` lines × `aai-u35-translate` ASR times) | 42.2% | 29.6% | **570ms** | 70.0% | 6.2% | 1191 | — |
| `ctc-forced-aligner` (no star) | 29.4% | 19.7% | 900ms | 66.9% | 10.4% | 1139 | 1.5s |
| `ctc-forced-aligner-star` | 41.7% | 27.8% | 650ms | 66.9% | 10.4% | 1138 | 1.4s |
| **`elevenlabs-fa`** | **42.2%** | **30.7%** | 625ms | **72.6%** | **0.0%** | 1236 | **6.9s** |
| `lyrics-alignment-mtl` (MTL+BDR) | 43.5% | 31.6% | 528ms | 72.6% | 0.0% | 1236 | 106.0s (GPU) |

### Secondary views (same 21 fixtures)

| Backend | Mono. cond. %≤400ms | Mono. gold-norm. %≤400ms | Mono. median Δ | Gold-norm. over all 1867 | Coverage over all 1867 | p90 Δ *(NOT quotable)* |
|---|---:|---:|---:|---:|---:|---:|
| Baseline | 48.7% | 22.3% | 419.5ms | 26.9% | 63.8% | 7238ms |
| `ctc-forced-aligner` | 31.8% | 13.7% | 755ms | 17.9% | 61.0% | 8338ms |
| `ctc-forced-aligner-star` | 47.7% | 19.6% | 460ms | 25.4% | 61.0% | 10817ms |
| **`elevenlabs-fa`** | 49.4% | 23.6% | 417ms | 28.0% | 66.2% | 12580ms |
| `lyrics-alignment-mtl` | **50.5%** | **23.8%** | **387.5ms** | **28.8%** | **66.2%** | 7289ms |

p90 is carried only for continuity with the original publication: per
`.claude/rules/lyrics-eval-backends.md` **only median and %≤400ms are
quotable**.

**Where `elevenlabs-fa` actually sits** (corrected — see corrections #2 and
#4). It is **2nd of 5 on the comparable gold-normalized metric** (30.7%,
behind only MTL's 31.6%) and 2nd on both monotonic views. But on the
conditional %≤400ms it **ties** the ASR-combiner baseline at 42.2% rather
than leading it, and **on median Δ it is 3rd, behind the baseline** (625ms
vs 570ms) as well as MTL. It matches MTL exactly on coverage and untimed
rate (72.6% / 0.0%) while running in **6.9s per song, ~15× faster than
MTL's GPU-normalized 106.0s** (the originally published "~22×" was measured
against MTL's device-blended 150.8s), and requiring **zero local install,
zero GPU, zero digit-crash/OOM edge cases** — both of which the local
aligners hit and had to work around, see
`2026-08-05-aligner-shootout.md`'s install-reality section.

**Runtime provenance (correction #8).** The 6.9s mean is **partially
verified**: commit `d42264e` restored the `runtime_sec` instrumentation the
committed script was missing, and a re-run confirmed it emits the field —
but the ElevenLabs free-tier character quota was exhausted after **8 of 22**
fixtures, so only 8 of the 21 scored runtimes have been re-measured. Those
8 come out at **mean 5.74s / median 5.55s**, and the *same* 8 in the
committed set also mean 5.74s (individual values differ by live-network
variance; the means coincide). The other 13 still originate from the
pre-fix-era script. A clean full re-run is blocked until the quota resets
**2026-09-05** — issue **#125 (eval: elevenlabs-fa re-run blocked mid-way by
exhausted free-tier quota)**. The 8 salvaged artifacts are committed at
`aligners_11l/raw_rerun_20260806/` as evidence; they are **not** a scoreable
row and must never be pooled beside the 21-fixture rows above.

## Conservative view (unique-gold-line only, ratio ≥0.75)

| Backend | Cond. %≤400ms *(of n pairs)* | **Gold-norm. %≤400ms** *(of 1702)* | Median Δ | n pairs |
|---|---:|---:|---:|---:|
| Baseline | 33.5% | 7.3% | 1155ms | 373 |
| `ctc-forced-aligner` (no star) | 21.4% | 4.5% | 1570ms | 355 |
| `ctc-forced-aligner-star` | **35.2%** | 7.3% | 1090ms | 355 |
| `lyrics-alignment-mtl` | 34.8% | **7.8%** | 1115ms | 379 |
| **`elevenlabs-fa`** | 34.3% | 7.6% | **1470ms** | 379 |

`elevenlabs-fa` and `lyrics-alignment-mtl` have the same `n_pairs` (379,
both beat CTC's 355) because both leave 0% of lines untimed, so the
conservative pool is not thinned by missing timestamps for either. On the
harder conservative view, `elevenlabs-fa`'s %≤400ms is competitive
(34.3% conditional, within 1pt of the other two real aligners; 7.6%
gold-normalized, 2nd of five) but its **median is noticeably worse** —
1470ms vs 1090–1155ms for everything else tested.
This is the one metric where ElevenLabs is a clear step behind, not a
close call — flagged honestly rather than folded into the generally
favorable headline picture above.

Note the conditional column's ranking here is not trustworthy: `ctc-star`'s
35.2% is computed over 355 pairs and MTL's 34.8% over 379. On the shared
1702-line denominator the order is MTL 7.8% > `elevenlabs-fa` 7.6% >
`ctc-star` 7.3% = baseline 7.3%.

## Per-category breakdown (official view) — all four backends

Format: median Δ / **conditional** %≤400ms / **gold-normalized** %≤400ms.
**Bold** = best of the four per category. Per-category coverage differs per
backend (`reverb_heavy`: CTC-star 35.5% vs 75.0% for the other two), so the
conditional column is not comparable across a row — the gold-normalized one
is.

| Category | `ctc-star` | `elevenlabs-fa` | `lyrics-alignment-mtl` |
|---|---|---|---|
| chant_repetition | 833ms / 33.8% / 25.8% | 859.5ms / 31.8% / 24.2% | **751ms / 34.8% / 26.5%** |
| clean_pop | 272.5ms / 71.6% / 70.2% | **260ms / 79.4% / 77.9%** | 260ms / 74.5% / 73.1% |
| dense_vocal | 470ms / 48.1% / 34.5% | 340ms / 51.9% / 37.3% | **332ms** / 51.9% / 37.3% |
| instrumental_breaks | 390ms / 51.1% / 36.5% | 380ms / 53.9% / 38.6% | **323ms / 60.1% / 43.0%** |
| multi_language | 1340ms / 32.1% / 19.8% | 1440ms / 32.1% / 19.8% | **1250ms / 33.8% / 20.9%** |
| reverb_heavy | **800ms** / 22.7% / 8.1% | 1025ms / 28.0% / **21.0%** | 881.5ms / 26.3% / 19.8%* |

`elevenlabs-fa` wins outright on `clean_pop` (79.4% conditional / 77.9%
gold-normalized — the single best result of any backend in either shootout
on any category) and essentially ties `lyrics-alignment-mtl` on
`dense_vocal` (identical 37.3% gold-normalized, 8ms apart on median). It
wins `reverb_heavy` on both %-columns (28.0% / 21.0%, best of all four)
despite the worst median there — meaning it has fewer severe outliers
pulling the median up on that category, an inversion worth noting rather
than smoothing over. `multi_language` and `chant_repetition`
remain the two hardest categories for every backend tested across both
reports — the same finding as the sibling shootout, and (per that report)
substantially a TEXT/line-boundary problem inherited from `qwen35-omni`'s
own line splitting, not an alignment-accuracy problem specific to any one
aligner.

*`reverb_heavy`'s CTC-vs-MTL comparison is not apples-to-apples (one CTC
fixture hit a digit-crash limitation, see the sibling report) —
`elevenlabs-fa` hit no such issue on any fixture in any category. The
gold-normalized column is precisely what makes that visible: `ctc-star`'s
22.7% conditional collapses to 8.1% once the lines it never timed are put
back into the denominator.

### The three hardest categories under the monotonic view (added 2026-08-06)

Median Δ, order-respecting pairs only:

| Category | `ctc-star` | `elevenlabs-fa` | `lyrics-alignment-mtl` | Baseline | Pairs kept (11l) |
|---|---:|---:|---:|---:|---:|
| chant_repetition | 1162ms | 928ms | 874ms | 694ms | 165 of 296 (−44.3%) |
| multi_language | 775ms | **351.5ms ✅** | **302ms ✅** | 512.5ms | 136 of 237 (−42.6%) |
| reverb_heavy | 730ms | 660ms | 650ms | 693ms | 125 of 186 (−32.8%) |

`multi_language` is the **only** hard category that clears the 400ms median
under any view, and only for `elevenlabs-fa` and MTL — i.e. a large part of
that category's apparent difficulty is the matcher mis-pairing lines across
languages rather than the aligner mistiming them. **Caveat, load-bearing:
the monotonic view discards 42.6% of that category's pairs** and removes
large deltas from numerator and denominator together, so this is a lower
bound on the category's true difficulty, not a pass. `chant_repetition` and
`reverb_heavy` fail under every backend and every view.

## Untimed-line rate — crushed to 0%, as hypothesized

**0.0% (0/1553 lines across 21 fixtures) — every single reference line
received a real timestamp on every one of the 22 fixtures run**, matching
`lyrics-alignment-mtl`'s result and confirming the task's hypothesis: a
true forced aligner structurally cannot leave a line untimed the way the
ASR-word-combiner (**6.2% untimed** — corrected 2026-08-06 from the
poisoned-fixture-inflated 20.2%, correction #3) or CTC forced alignment's
digit-crash fallback (10.4% untimed, a real but fixable upstream
limitation, not structural) can. The corrected baseline figure makes this a
**smaller** win than published — 0.0% vs 6.2%, not 0.0% vs 20.2% — and it
is no longer the largest margin in this report; the hypothesis itself is
still confirmed, and note that the CTC pair is now *worse* than the
baseline on this metric, not better. This holds even on the poisoned fixture's 395 hallucinated
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

*(Rewritten 2026-08-06 against the corrected numbers.)*

**Does `elevenlabs-fa` clear the 400ms bar?** Partially, the same qualified
way the sibling report found for `lyrics-alignment-mtl` — and by less than
originally reported. On the primary %-within-400ms metric it **ties the
corrected baseline** (42.2% vs 42.2% conditional) rather than beating it;
on the comparable gold-normalized view it does genuinely lead (30.7% vs
29.6%), but that number also says plainly that **fewer than a third of gold
lines land inside the gate**. It does not clear the pooled median (625ms —
**3rd of five, behind the baseline's 570ms**) or the conservative view's
%≤400ms (34.3%, with a 1470ms conservative median, the weakest of any real
aligner tested). **Untimed% is fully crushed to 0.0%, matching MTL and
clearing the corrected baseline's 6.2%** — the task's central hypothesis is
confirmed, though the margin is 6.2 points rather than the 20.2 originally
claimed, so this is no longer the report's largest margin.

**Where `elevenlabs-fa` fits among the backends tested across both
2026-08-05 reports:**

- **`lyrics-alignment-mtl` remains the best pure timing-quality result**
  (43.5% conditional / 31.6% gold-normalized, 528ms median, 0.0% untimed)
  — but by a **0.9-point gold-normalized margin over `elevenlabs-fa`, and
  only 0.2 points on the monotonic gold-normalized view**, at
  **106.0s/song GPU-normalized** plus a real local GPU/install burden (see
  the sibling report's ~15min setup + the CUDA-OOM fallback that fired on
  **two** of 21 fixtures).
- **`elevenlabs-fa` is a genuinely close 2nd on quality** (42.2%
  conditional / 30.7% gold-normalized, 625ms median, 0.0% untimed, wins 2
  of 6 categories on the gold-normalized column) with **dramatically lower
  operational cost**: no install, no GPU, no digit-crash or CUDA-OOM edge
  cases, 6.9s/song — ~15× faster than MTL's GPU-normalized figure, fast
  enough to run inline in a production pipeline rather than as an offline
  batch job — at the cost of a paid per-audio-second API call and a
  materially worse conservative-view median (1470ms).
- **The ASR-combiner baseline is not the floor this report assumed.**
  Corrected, it ties `elevenlabs-fa` on conditional %≤400ms and **beats it
  on median** (570ms vs 625ms). What the forced aligners genuinely buy over
  it is coverage (72.6% vs 70.0%) and untimed lines (0.0% vs 6.2%) — not a
  large timing-accuracy jump.
- **`ctc-forced-aligner-star` is the throughput choice** (1.4s/song,
  single warm process) but is clearly behind `elevenlabs-fa`, MTL **and the
  corrected baseline** on the gold-normalized metric (27.8% vs 30.7 /
  31.6 / 29.6), and carries a real digit-crash limitation neither of the
  other two hit.

**For a production pipeline that cannot run a local GPU worker (or wants to
avoid MTL's 106s/song GPU-normalized latency) but needs
forced-alignment-grade untimed-line elimination, `elevenlabs-fa` is the
best available option of the backends tested** — it is the only one that
clears BOTH the 0%-untimed bar AND runs fast enough (single-digit seconds)
for inline use. With the corrected numbers the quality cost of choosing it
over MTL is **0.9 gold-normalized points** (0.2 on the monotonic view), for
~15× the throughput and no GPU — a materially better trade than the
originally published 2.1-point gap implied. Its
confidence (`loss`) field, while not a clean linear predictor, usefully
separates a high-accuracy top quartile (54.7% ≤400ms) from a
low-accuracy bottom quartile (27.2%) and is worth wiring into a
production quality gate. It also **does** return per-word timings — the
`fixtures_with_word_timings: 0` in `scores.json` is an artifact of `words[]`
being stripped before transfer, not a capability gap (correction #7).

**Where it still fails**: the same three categories every backend
struggles with — `chant_repetition`, `multi_language`, and `reverb_heavy`
never clear 400ms on median under `elevenlabs-fa` on the official view
(the one exception, `multi_language` at 351.5ms under the order-respecting
monotonic view, discards 42.6% of that category's pairs and is a lower
bound on its difficulty, not a pass) — and the
conservative-view median gap (1470ms vs ~1100ms for the local aligners) is
a genuine, unresolved weakness worth re-checking if ElevenLabs ships a
model update. Its runtime headline also remains **partially verified**
until the quota-blocked full re-run lands (#125).

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

**The scored row above regenerates byte-identically from the committed
`aligners_11l/raw/` — no API call is needed to reproduce any accuracy
figure in this report.** The separate `aligners_11l/raw_rerun_20260806/`
directory holds 8 fixtures re-run on 2026-08-06 with the restored
instrumentation; it is **evidence only** (runtime reproducibility +
per-word `words[]`), never a scoreable row, and its README explains why.
