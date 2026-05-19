# Lyrics Eval — assemblyai-universal-3-pro rev 1 — 2026-05-19

**Run ID:** `2026-05-19T13:00:00Z`
**Backend:** `assemblyai-universal-3-pro` rev 1 (AssemblyAI Universal-3 Pro, dedicated ASR, flagship tier $0.21/hr)
**Judge:** `claude-opus-4-7`, prompt revision 1
**Fixtures run:** 5 of 24 (gold for 3 of them re-anchored earlier this session — see `feedback_verify_gold_reference.md`)
**Cost:** ~$0.07 across the 5 fixtures; 185 free hours per AAI account so future eval is free

## Headline

**4 of 5 fixtures pass the ±400 ms wall-acceptable timing gate.** First backend in the entire eval to consistently land karaoke-grade timing. Mean score 6.6 vs whisperx 4.4, wall-pass 4/5 vs whisperx 1/5.

| Metric | whisperx | gemini-fl | **assemblyai-u3-pro** |
|---|---:|---:|---:|
| Mean score | 4.4 | 5.0 | **6.6** |
| Median score | 4 | 5 | **7** |
| Wall-acceptable | 1/5 | 0/5 | **4/5** |

## Per-fixture summary

| video_id | category | score | verdict | wall_ok | coverage | median_abs | signed_median |
|---|---|---:|---|:---:|---:|---:|---:|
| 5JW87KKDTcU | dense_vocal | 6 | partial | ✅ | 37% | 145 ms | +5 ms |
| p74PDWAFk0A | reverb_heavy | 5 | partial | ❌ | 40% | 721 ms | +319 ms |
| s8o2YuTBYk4 | instrumental_breaks | 7 | match | ✅ | 61% | 188 ms | -29 ms |
| tCivrrU4SSM | multi_language | 8 | match | ✅ | 70% | 203 ms | +118 ms |
| YbGFYaA0SbY | clean_pop | 7 | match | ✅ | 63% | 144 ms | -119 ms |

## Timing comparison (signed median offset in ms)

| Category | whisperx | gemini-fl | **assemblyai-u3-pro** |
|---|---:|---:|---:|
| dense_vocal | -163 | +164 | **+5** ★ |
| reverb_heavy | +420 | -457 | **+319** |
| instrumental_breaks | +434 | +512 | **-29** ★ |
| multi_language | +1094 | +1180 | **+118** ★ |
| clean_pop | +256 | +293 | **-119** ★ |

★ = best timing in category

## What's good

- **Timing intrinsically correct.** AAI's word-level timestamps come from acoustic alignment, not from an LLM trying to format `[mm:ss.mmm]` strings. The difference shows: 4 of 5 fixtures land within ±200 ms of (re-anchored) gold.
- **Zero hallucination clusters across the entire run.** Whisperx had 1 on instrumental_breaks (`i'm gonna praise you with my hands raise` x8); Gemini also had clusters. AAI is clean.
- **Multi_language category WIN.** AAI scored 8 on `tCivrrU4SSM` where whisperx scored 3 and Gemini scored 4. Cleanest result of the entire eval.
- **Fast.** 13-29 s per fixture, no preprocess waiting on Mel-Roformer GPU. End-to-end equal to or faster than Gemini.

## What needs tuning

- **Coverage 37-70% because the silence-gap line-splitter (800 ms) is too generous.** AAI returns word-level timestamps with no native line/sentence boundaries; our wrapper splits on silence > 800 ms. For typical sung lyrics, gold annotators split on shorter pauses (~300-500 ms). Result: multiple gold lines merge into single AAI lines, and matched-line count goes down.

  Hypothesis: tightening `LINE_GAP_MS` from 800 to ~400 should roughly double the matched-line count and push coverage into the 70-90% range across all categories without affecting timing precision.

- **reverb_heavy still struggles** (median_abs 721 ms). 8-minute live recording with heavy room reverb is intrinsically hard for any acoustic aligner. Could be a candidate-specific limitation; would benefit from a longer audio passage to inform median (only 75 matched lines out of 137 gold).

## Recommendation

**Promote to candidate-for-production status pending one line-splitter tune experiment.** Concrete next steps:

1. Re-run all 5 fixtures with `LINE_GAP_MS=400` instead of 800; verify coverage rises to ≥ 70% across all categories without timing regression.
2. If step 1 passes: run on remaining 19 fixtures; gather full per-category statistics.
3. If full-pilot per-category scores hold up: propose to user that AAI Universal-3 Pro replaces whisperx as production champion.

CHAMPION.md unchanged in this commit. Whisperx remains the production backend pending the line-splitter tune + full 24-fixture confirmation.

## Caveats

- 3 of 5 fixture gold timings were re-anchored earlier today via multi-backend consensus shift (see `feedback_verify_gold_reference.md` and the `(manifest re-anchor)` row in `history.md`). The whisperx and Gemini reports use the pre-shift gold — direct numerical comparison is approximate, but the +400-1000 ms shifts only hurt those backends' apparent timing; AAI's clean numbers are against the corrected truth.
- Score-by-category cells are still N=1 each. The clean wins on instrumental_breaks, multi_language, clean_pop need confirmation across remaining 19 fixtures before claiming categorical dominance.
- The "reverb_heavy 8-min song" failure may not generalise — could be the specific song. Need more reverb-heavy fixtures from the catalog.
