# Lyrics Eval — whisperx-large-v3 rev 1 — 2026-05-19

**Run ID:** `2026-05-19T06:06:09Z`
**Backend:** `whisperx-large-v3` rev 1 (pinned model version `84d2ad2d…afcb`)
**Judge:** `claude-opus-4-7`, prompt revision 1
**Fixtures run:** 5 of the committed 24-fixture manifest (first-pass baseline; remaining 19 fixtures already in `manifest.json` for the full-baseline follow-up pass)
**Vocal-stem source:** Mel-Roformer + anvuew dereverb via `scripts/lyrics_worker.py preprocess-vocals` on each song's cached `_audio.flac`

## Aggregate

| Metric | Value |
|---|---|
| Mean score (0–10) | **4.4** |
| Median score | **4** |
| Fixtures passing wall-acceptable | **1 / 5** |
| Mean elapsed per song | ~19 s (whisperx) + ~140 s (preprocess) |

## Scores by category

| Category | Score |
|---|---:|
| clean_pop | 7.0 |
| dense_vocal | 5.0 |
| reverb_heavy | 4.0 |
| instrumental_breaks | 3.0 |
| multi_language | 3.0 |

## Per-fixture summary

| video_id | category | score | verdict | wall_ok | coverage | median_off_ms | hallucinations |
|---|---|---:|---|:---:|---:|---:|---:|
| YbGFYaA0SbY | clean_pop | 7 | match | OK | 0.852 | 256 | 0 |
| 5JW87KKDTcU | dense_vocal | 5 | partial | NO | 0.597 | 163 | 0 |
| p74PDWAFk0A | reverb_heavy | 4 | partial | NO | 0.613 | 420 | 1 cluster |
| s8o2YuTBYk4 | instrumental_breaks | 3 | hallucination | NO | 0.512 | 434 | 1 cluster (x8) |
| tCivrrU4SSM | multi_language | 3 | miss | NO | 0.426 | 1094 | 0 |

## Per-fixture reasoning

### YbGFYaA0SbY — God I'm Just Grateful (Elevation Worship x Chandler Moore)

Clean-pop track with isolated vocals (post-dereverb) — whisperx's sweet spot. 85% coverage, 56 candidate lines vs 54 gold (close 1:1), no hallucinations, timing within tolerance. This is the only fixture in this baseline that would actually ship to the wall.

### 5JW87KKDTcU — COME RIGHT NOW (Planetshakers)

WhisperX produced 42 lines against 62 gold (coverage 60%, 25 gold lines missing). Timing on matched lines is acceptable for a wall display. But losing 40% of the gold lines means significant chunks of the song would have no on-screen lyrics. Not wall-acceptable for production.

### p74PDWAFk0A — Can't Take My Worship (Maverick City Music x Travis Greene)

Long song (8 min, reverb-heavy). Heavy over-segmentation (65 extra candidate lines past the matched 84), median timing offset just over the wall-acceptable threshold, plus a `let's go.` x4 hallucination cluster. Coverage of gold is 61% but the 65 extras would clutter the wall.

### s8o2YuTBYk4 — Free Indeed (Planetshakers)

Instrumental-break category exposes the failure mode: when the vocal track is sparse, whisperx hallucinates by repeating a recently-heard phrase. The 8-line repetition cluster `i'm gonna praise you with my hands raise` would be very visible on the wall. Combined with 49% missing coverage, this is unusable for production.

### tCivrrU4SSM — GREATER (Planetshakers)

Multi-language category. WhisperX produced only 24 lines against 47 gold (49% loss) and the lines it did produce drift over a second behind. No hallucination clusters but the timing alone makes this unusable on the wall.

## Interpretation

This baseline establishes the bar that any candidate backend from issue [#111](https://github.com/zbynekdrlik/songplayer/issues/111) must clear:

- **Clean-pop** is whisperx's strong category (score 7 / wall-acceptable). Any replacement must hold >= this.
- **Dense vocal**, **reverb-heavy**, **instrumental-breaks**, and **multi-language** are the weak categories where whisperx loses 40–60% of gold lines and/or hallucinates. These are the target categories for [#112](https://github.com/zbynekdrlik/songplayer/issues/112)'s no-text-source ASR path improvements.

## Caveats

- Run on first 5 of the 24 pinned fixtures. The remaining 19 are in the same `manifest.json` and will be evaluated in a follow-up full-baseline pass. Per-category score precision will improve once each category has 4–5 fixtures (currently each category has exactly 1 in this 5-fixture pass).
- Replicate occasionally exhibited cold-start hangs on individual predictions (twice during the run; retries cleared them). Real wall-clock per-fixture varied 17 s → 240 s when a cold start hit. Worth tracking if it becomes a stable issue.
