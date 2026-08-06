# Lyrics Eval Run History

Append-only one-line summary of every `/lyrics-eval` run. Newest at the bottom.

## Corrections (2026-08-06)

The 2026-08-05 sweeps (one-call north-star, combine experiment, aligner
shootout, ElevenLabs) were never appended to this table. They are added
below **with the 2026-08-06-corrected figures**, not the numbers as first
published — an adversarial review found that the combine experiment pooled
the poisoned fixture `Xvm4_fWkXe8` into its rows while the aligner reports
excluded it from theirs, so the "baseline" every aligner was measured
against had a different denominator than the aligners. Each report carries
its own `## Corrections (2026-08-06)` table listing exactly what moved.
Nothing above this line was altered.

**Second-pass correction (2026-08-06, later the same day).** The one-call
sweep's own CLI path (`score_one_call.py`) did not exclude the poisoned
fixture `Xvm4_fWkXe8` the way the other two scorers already did —
`2026-08-05-scores.json` was regenerated to match, and the two rows below
are updated accordingly.

| # | Was | Is now | Cause |
|---|---|---|---|
| 1 | `gemini36-flash` (one-call) row: 22 fixtures / 1772 gold (poisoned fixture included, "different denominator from every row below"); 16.0% cond. / 7.1% gold-norm., median 2160ms, coverage 44.2% | **21 fixtures / 1702 gold — the SAME denominator as every row below.** 16.1% cond. / **7.3% gold-norm.**, median 2145ms, coverage 45.6%. | `score_one_call.py`'s CLI path now excludes `POISONED_FIXTURE_VIDEO_ID` like the other two scorers; `2026-08-05-scores.json` regenerated. |
| 2 | `aai-u35-translate` (one-call) row: 31.1% cond. over 206 matched → 3.6% gold-norm., coverage 11.6%, "collapses 4/22 songs into one line" | 31.1% cond. over 206 matched → **3.8% gold-norm.**, coverage **12.1%**, collapses **3/21** songs into one line (the 4th was the now-excluded poisoned fixture, `Xvm4_fWkXe8`). | Same regeneration. |

**Reading the 2026-08-05 rows:** those runs use the algorithmic scorer
(`score_one_call.py` / `score_aligner.py`), not the Claude-judge harness,
so "Mean Score" and "Wall Pass" do not apply — the comparable figure is
`%≤400ms (gold-norm.)`, i.e. within-400ms divided by the full gold-line
count, identical for every backend in a given sweep. The **conditional**
`%≤400ms` those reports lead with divides by matched-and-timed lines only
and is **not** comparable across backends.

| Date | Backend | Revision | Mean Score | Wall Pass | Note |
|------|---------|----------|------------|-----------|------|
| 2026-05-19 | whisperx-large-v3 | r1 | 4.4 | 1/5 | initial baseline; pre-shift gold. `2026-05-19-whisperx-large-v3.md` |
| 2026-05-19 | gemini-3-1-flash-lite | r1 | 5.0 | 0/5 | already-known-tech, kept for reference. `2026-05-19-gemini-3-1-flash-lite.md` |
| 2026-05-19 | nemotron-3-nano-omni | r1 | DROPPED | n/a | audio_tokens=0 every call. Wrapper deleted. |
| 2026-05-19 | mimo-v2-5 | r1 | DROPPED | n/a | reasoning model; 2 fixtures emitted empty content. Wrapper deleted. |
| 2026-05-19 | mimo-v2-omni | r1 | DROPPED | 0/5 | systematic +400-4030ms late-timing; prompt-tune shot failed. Wrapper deleted. |
| 2026-05-19 | (manifest re-anchor) | - | - | - | 3 of 5 LRClib gold timings shifted via multi-backend consensus. `feedback_verify_gold_reference.md`. |
| 2026-05-19 | assemblyai-universal-3-pro | r1 | 6.6 | 4/5 | first wall-gate-passing backend; coverage 37-70% (line-splitter merged adjacent gold lines). `2026-05-19-assemblyai-universal-3-pro.md` |
| 2026-05-19 | assemblyai-universal-3-pro | r2 | **7.6** | **5/5** | LINE_GAP_MS 800→400; coverage jumped +11..+34pp every category, timing held inside gate. Multi_language 9/10 (whisperx 3, Gemini 4). Zero hallucinations. New front-runner for production champion. `2026-05-19-assemblyai-universal-3-pro-r2.md` |
| 2026-08-05 | gemini36-flash (one-call) | r1 | n/a | n/a | Algorithmic scorer, 21 fixtures / 1702 gold (**same denominator as every row below** — corrected 2026-08-06, see the second-pass correction above). 16.1% ≤400ms conditional but only **7.3% gold-norm.**, median 2145ms, coverage 45.6%. Right structure, unreliable timestamps. `2026-08-05-one-call-northstar.md` |
| 2026-08-05 | aai-u35-translate (one-call) | r2 | n/a | n/a | Same sweep/denominator. 31.1% ≤400ms conditional over just 206 matched lines → **3.8% gold-norm.**, median 1575.5ms, coverage 12.1%. Accurate when it segments; `match_original_utterance` collapses 3/21 songs into one line. `2026-08-05-one-call-northstar.md` |
| 2026-08-05 | qwen35-omni × aai-u35-translate (combine) | r1 | n/a | n/a | **Best recombination, and the "baseline" the aligner shootout was measured against.** 21 fixtures / 1702 gold, poisoned excluded. **42.2% ≤400ms cond. / 29.6% gold-norm., median 570ms, 6.2% untimed, 70.0% coverage** — corrected 2026-08-06 from the published 41.4% / 573ms / 20.2% / 70.1%, which pooled the poisoned fixture (297 of its 394 untimed lines came from that one fixture). `2026-08-05-combine-experiment.md` |
| 2026-08-05 | ctc-forced-aligner (no star) | r1 | n/a | n/a | Same 21/1702 denominator. 29.4% cond. / **19.7% gold-norm.**, median 900ms, 10.4% untimed, 66.9% coverage, 1.5s/song. Worst of the field; crashes on literal digits in the reference text. `2026-08-05-aligner-shootout.md` |
| 2026-08-05 | ctc-forced-aligner-star (`--star_frequency segment`) | r1 | n/a | n/a | 41.7% cond. / **27.8% gold-norm.**, median 650ms, 10.4% untimed, 66.9% coverage, **1.4s/song — fastest of the field**. `star` is worth +12.3 cond. pts over the default. **Does NOT beat the combine baseline** — the original claim that it did was the poisoned-fixture artifact. `2026-08-05-aligner-shootout.md` |
| 2026-08-05 | elevenlabs-fa (hosted API) | r1 | n/a | n/a | 42.2% cond. (**ties** the baseline) / **30.7% gold-norm.** (beats it), median 625ms (**loses** to the baseline's 570ms), **0.0% untimed**, 72.6% coverage, 6.9s/song (partially verified — #125). No install, no GPU. Best `clean_pop` result of any backend (79.4%). `2026-08-05-elevenlabs-fa.md` |
| 2026-08-05 | **lyrics-alignment-mtl (MTL+BDR)** | r1 | n/a | n/a | **Winner of the shootout, on every accuracy view, by a small margin.** 43.5% cond. / **31.6% gold-norm.**, median 528ms, **0.0% untimed**, 72.6% coverage. 1st-vs-2nd is 0.9 gold-norm. pts (0.2 on the order-respecting view), not the 2.1 originally published. **Last on runtime: 106.0s/song GPU-normalized** (n=19; 2 fixtures fell back to CPU at 575.4s — the published "150s" was device-blended). `2026-08-05-aligner-shootout.md` |
| 2026-08-06 | (rescore, no new runs) | - | - | - | All five 2026-08-05 rows re-scored from committed artifacts after an adversarial review found 14 harness defects; `2026-08-05-combine-raw/` regenerated byte-identically, confirming the recombination step is deterministic. Every affected report carries a `## Corrections (2026-08-06)` table. No `LYRICS_PIPELINE_VERSION` change; no API calls. |
