# Current Lyrics Champion — the production regime

**Regime:** lyrics pipeline v22 (#159) — one forced aligner, one ASR vendor.

| Tier | Model | Role | Gold-norm ≤ 400 ms (current manifest, of all 1414 gold lines) |
|------|-------|------|------|
| ★ tier | **`lyrics-alignment-mtl`** (LyricsAlignment-MTL, MTL+BDR, MIT, open-weight) | forced-aligns a text candidate; the line timings shown on the wall | **31.2 %** (2026-08-05 raw, re-scored 2026-11-21); 32.0 % re-run on dev1's rebuilt audio |
| gate + base tier | **`gemini-3.5-transcribe`** (Google, `gemini-3.5-transcribe`, Stable) | the `reference_gate` transcript that verifies the ★ text; when no text passes, its words grouped into lines are the base tier | **21.3 %** as the no-text tier (2026-09-12 raw, re-scored 2026-11-21) |

**Last review:** 2026-11-21 (#349) — `reports/2026-11-21-review.md`, scores
`reports/2026-11-21-scores.json`. **Next review due:** 2026-12-21 (monthly,
`.claude/rules/lyrics-eval-backends.md`).

**Verdict 2026-11-21: production stays.** No candidate beats either tier by a
clear margin on the comparable figure:

- Aligners: `w2v-bert-2.0-music-lyrics-ctc` 32.0 % and `sylla-600m-v1` 31.8 %
  vs mtl 32.0 % on the same audio and text. Three independent aligners agree
  within 0.2 pts — the gold's own timing noise now caps this metric, so no
  aligner swap can be justified by it. w2v-bert is ~6× faster (≈ 10 s vs 65 s
  per song on dev1's RTX 2070 SUPER); that is an operational, not a quality,
  argument.
- No-text tier: the best open-weight result, w2v-bert on the full mix
  (21.9 %), edges g35t (21.3 %) by 0.6 pts on the headline but loses by
  3.4 pts on the order-respecting (monotonic) view and over-produces lines
  1.6×. Not a win.
- Every new paid model (AssemblyAI STT 3.7, Soniox v6, ElevenLabs Scribe v3,
  Deepgram Nova-3, AudioShake) was **not measured: no key** in dev1's secret
  channel this month. OpenAI `gpt-transcribe` returns no word or segment
  times, so it cannot serve either tier.

Every number is a floor, not a verdict: the gold (LRCLIB / Spotify / YouTube
line sync) has its own unmeasured timing noise (#129).

## Promotion log

| Date | Backend | Decision | Reason |
|------|---------|----------|--------|
| 2026-05-19 | whisperx-large-v3 r1 | initial baseline measured | first /lyrics-eval run (5 fixtures); retired with the v20 route in #159, wrapper deleted in #286 |
| 2026-08-05 | lyrics-alignment-mtl | won the aligner shootout | 31.6 % gold-norm (old 21-fixture manifest), first on every view; `2026-08-05-aligner-shootout.md` |
| 2026-09-12 | gemini-3.5-transcribe | best dedicated ASR, became the gate's transcript + base tier (v22, #159) | 19.7 % gold-norm own lines (old manifest) vs Soniox v5 16.0 %; `2026-09-12-gemini-3-5-transcribe.md` |
| 2026-11-21 | (monthly review, #349) | **no change** | open-weight aligners tie mtl, no open-weight ASR clearly beats g35t, the paid candidates had no key; `2026-11-21-review.md` |
