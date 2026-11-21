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

- Aligners: `w2v-bert-2.0-music-lyrics-ctc` 32.3 % and its fine-tune
  `sylla-600m-v1` 31.7 % vs mtl 32.0 % on the same audio and text — within
  0.6 pts, less than the 0.8 pts mtl itself moved between two copies of the
  same audio. Hypothesis (to be confirmed by #129's hand-verified gold): the
  gold's own timing noise caps this metric, so it cannot rank aligners.
  w2v-bert / sylla are 5.4–5.8× faster (mean 11–12 s vs 65.5 s per song on
  dev1's RTX 2070 SUPER) — an operational, not a quality, argument.
- No-text tier: w2v-bert reads 23.0 % vs g35t 21.3 % on the headline but
  13.2 % vs 15.6 % on the order-respecting (monotonic) view, and it
  over-produces lines 1.7×; g35t's number comes from different audio
  (no key to re-run it). Not a clear margin; re-check on the same audio
  next month.
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
| 2026-11-21 | (monthly review, #349) | **no change** | open-weight aligners tie mtl; w2v-bert no-text +1.7 headline / −2.4 monotonic vs g35t, not a clear margin; the paid candidates had no key; `2026-11-21-review.md` |
