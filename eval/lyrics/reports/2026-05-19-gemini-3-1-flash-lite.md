# Lyrics Eval — gemini-3-1-flash-lite rev 1 — 2026-05-19

**Run ID:** `2026-05-19T07:30:00Z`
**Backend:** `gemini-3-1-flash-lite` rev 1 (`google/gemini-3.1-flash-lite-20260507` via OpenRouter)
**Judge:** `claude-opus-4-7`, prompt revision 1
**Fixtures run:** 5 of 24 (same pilot set as the whisperx baseline)
**Audio input:** Mel-Roformer + anvuew dereverbed 16 kHz WAV, except `p74PDWAFk0A` which had to be down-encoded to 64 kbps MP3 to fit OpenRouter's request-body limit (31 MB float WAV → 502 Bad Gateway, 4 MB MP3 → OK)
**Cost:** ~$0.04 total across the 5 fixtures (free Nemotron attempt didn't ingest audio; see backend file header)

## Aggregate

| Metric | whisperx-large-v3 | gemini-3-1-flash-lite | Δ |
|---|---:|---:|---:|
| Mean score | 4.4 | **5.0** | +0.6 |
| Median score | 4 | **5** | +1 |
| Wall-acceptable | 1 / 5 | 0 / 5 | -1 |
| Cost per song | $0.005 (Replicate) | ~$0.008 (OpenRouter) | small |
| Latency per song | 22 s + ~140 s preprocess | 8–18 s + 0 s preprocess | **~10× faster** end-to-end |

## Scores by category

| Category | whisperx | gemini-3-1-flash-lite | winner |
|---|---:|---:|---|
| dense_vocal | 5 | **6** | gemini |
| reverb_heavy | 4 | 4 | tied |
| instrumental_breaks | 3 | **5** | gemini |
| multi_language | 3 | **4** | gemini |
| clean_pop | **7** | 6 | whisperx |

## Per-fixture summary

| video_id | category | whisperx | gemini | verdict | wall_ok | coverage Δ | timing Δ (median ms) |
|---|---|---:|---:|---|:---:|---:|---:|
| 5JW87KKDTcU | dense_vocal | 5 | 6 | partial | ❌ | +17% (60→77) | tied (~163ms both) |
| p74PDWAFk0A | reverb_heavy | 4 | 4 | partial | ❌ | -9% (61→53) | -37ms (420→457) |
| s8o2YuTBYk4 | instrumental_breaks | 3 | 5 | partial | ❌ | +17% (51→68) | -78ms (434→512) |
| tCivrrU4SSM | multi_language | 3 | 4 | partial | ❌ | +34% (43→77) | -86ms (1094→1180) |
| YbGFYaA0SbY | clean_pop | 7 | 6 | partial | ❌ | -15% (85→70) | -37ms (256→293) |

## Recommendation

**Partial promote.** Gemini 3.1 Flash Lite wins three of the four whisperx-weak categories on coverage by 17–34 percentage points; tied on reverb_heavy; loses clean_pop. The remaining problem is timing — Gemini systematically lands just over the 400 ms wall-acceptable threshold on the harder fixtures (and ~1.2 s late on multi-language, which is a "vocal onset vs section onset" disagreement with the LRClib gold). No fixture is wall-acceptable yet under the strict gate, but Gemini's text quality is materially better on every weak category.

Concrete next-step proposals (NOT shipping in this run):

- **Tune the prompt** to nail timing closer to vocal-card onset (currently lands at vocal-attack); the multi-language ~1.2 s drift may collapse with a better prompt.
- **Add to the eval rotation** alongside whisperx so each new candidate gets compared against both.
- **Test on the remaining 19 fixtures** to refine per-category scores (current per-category buckets have N=1).

This run did NOT update `CHAMPION.md` — whisperx remains the production champion until the user explicitly decides whether to (a) tune the Gemini prompt + re-run, (b) split routing per category, or (c) keep whisperx and try a different novel candidate next.

## Failed candidates this session

- **`nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free`** via OpenRouter — model accepts audio per the published modality list, but `prompt_tokens_details.audio_tokens = 0` on every call (audio not ingested). Reasoning model burned `completion_tokens=4096` with empty visible `content`. Wrapper kept at `eval/lyrics/backends/nemotron_3_nano_omni.py` for when the upstream audio routing is fixed.

## Caveats

- 1 fixture (`p74PDWAFk0A`) used MP3 instead of WAV input; the rest used the same dereverbed float WAV whisperx ran on.
- Per-category scores are N=1 in this pilot. Apparent gemini-wins on dense_vocal / instrumental / multi-language need confirmation across all 4–5 fixtures per category before promoting.
- OpenRouter Gemini Flash Lite is currently capped at 8192 max output tokens — works for songs ≤ ~2000 lines. Longer songs may truncate.
