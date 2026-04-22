# Gemini Chunked Lyrics Provider — Design

**Status:** design-phase, prototype-first
**Owner:** Zbyněk Drlík
**Scope:** Line-level timing only. Word-level timing deferred to a later PR.

## Background

The current lyrics pipeline (`crates/sp-server/src/lyrics/`) combines qwen3 forced
alignment, YouTube auto-subs, and a deterministic merge layer. A live event on
2026-04-19 showed the output was unusable for many songs:

- On songs without lrclib/yt_subs timing anchors, qwen3 receives untimed reference
  text, produces per-chunk timestamps that are silently collapsed into a monotonic
  80 ms-floor sequence by the sanitizer. Result: song 230 "Known By You" compressed
  all 20 description lines into the first 10.5 s of 11 min of audio.
- The ensemble merge picks qwen3 as primary (higher `base_confidence`) so other
  providers' timings never reach the output; autosub is confidence-booster only.
- Quality gate `avg_confidence_mean` reads qwen3's self-reported confidence, not
  measured alignment validity, so a garbage output scores 0.63 and ships green.

Testing on 2026-04-20 against a single real song (video `Avi4sMPQqzI`) showed
**Gemini 3 Pro transcribing the Demucs-dereverbed vocal WAV produces cleaner text
and more accurate line timings than any of the above approaches**. The catch is
Gemini drifts 2–4 s later in the song when fed the full 11 min at once; chunking
with overlap eliminates that drift.

## Goal

Line-level timings in `_lyrics.json` should match what the singer actually sings,
for every song, every repeat of every chorus, including instrumental breaks.

## Success criteria

1. On a set of 5 representative songs (worship ballad, fast tempo, instrumental
   bridge, non-English, short track), the first-word start and every line start
   are within 500 ms of the true sung onset as heard in Resolume.
2. No "phantom lines" during instrumental sections.
3. No missed chorus repeats — if a refrain is sung 5 times, the file contains
   5 separately-timed entries.
4. Coverage from first vocal to last vocal of each song.

## Non-goals (for this PR)

- Word-level timing (defers to a later PR; qwen3 code stays in the repo, dormant)
- Replacing qwen3 in its well-functioning cases (it stays as an optional provider
  that can be reactivated when word-level work resumes)
- Changing the Resolume push-text architecture
- Changing the translator (EN→SK stays as-is)

## Prototype-first methodology

The Rust lyrics worker has historically been expensive to iterate in — CI cycles
of 30+ min and mutation testing gate every change. Given this feature will
change the core pipeline, we prototype in Python on live data before touching
Rust.

### Phase 0 — Prototype

Location: `scripts/experiments/gemini_lyrics.py` (one script, no Rust changes)

Input: video_id (the normalized FLAC + Demucs-dereverbed WAV must already exist
on disk; we reuse existing `preprocess-vocals` output).

Steps:
1. Load or generate the Demucs-dereverbed vocal WAV for the song.
2. Split the WAV into 60 s chunks with 10 s overlap (stride 50 s). Each chunk
   records its global start offset.
3. For each chunk, call Gemini 3 Pro via the CLIProxyAPI Google-account endpoint
   with the audio + the refined prompt (see Appendix A).
4. Parse each chunk's response into a list of `(local_start_ms, local_end_ms, text)`
   tuples, then offset each tuple by the chunk's global start to produce global
   timings.
5. Merge overlapping regions (see Appendix B for the merge algorithm).
6. Write the resulting line list to `{cache}/{youtube_id}_lyrics.json` using the
   existing `LyricsTrack` schema. `sk` is left `null`; the background translator
   fills it in on its next pass.

Validation: run the script on a shortlist of 5 songs representing different
styles. Listen in Resolume. Record per-song:
- first-line start offset vs sung onset (ms)
- lines displayed vs lines sung (coverage)
- any phantom/hallucinated text
- drift by end of song (ms)

Iterate on prompt and chunking params until all 5 pass the success criteria.

### Phase 1 — Rust port

Only after Phase 0 validates on 5 songs.

A new alignment provider `GeminiLyricsProvider` in
`crates/sp-server/src/lyrics/gemini_provider.rs` implementing `AlignmentProvider`.

Key wiring:
- Reuses the existing Demucs preprocessing pipeline output (`clean_vocal_path`
  on `SongContext`, same field qwen3 uses).
- Produces a `ProviderResult` with `LineTiming` entries only (no `WordTiming`
  entries in the MVP — word vectors empty).
- Goes behind a feature flag `LYRICS_GEMINI_ENABLED` defaulted to `true` once
  Phase 0 succeeds; qwen3 provider stays registered behind
  `LYRICS_QWEN3_ENABLED` defaulted to `false`.
- Orchestrator path: when Gemini produces lines, merge layer passes them through
  unchanged (no interpolation, no sanitization besides monotonicity enforcement).
- Pipeline version bump from v10 → v11, triggering auto-reprocess of the catalog.
- Cache the raw Gemini chunk responses as `{youtube_id}_gemini_chunks.json` so
  re-parsing or re-merge doesn't require re-calling the API.

Failure modes:
- CLIProxy unreachable / Google auth expired → worker logs error, leaves song
  unprocessed (no fallback text written). Same shape as current Claude errors.
- Gemini returns malformed output (doesn't match the `(MM:SS.x --> MM:SS.x) text`
  regex) → chunk skipped, warn-logged; other chunks still contribute.
- No vocals in a chunk → chunk returns `# no vocals`; merge treats as empty.

## Data flow

```
YouTube video → yt-dlp → normalized FLAC (existing)
                              │
                              ▼
                  Demucs Mel-Roformer + anvuew dereverb (existing)
                              │
                              ▼
                         vocal WAV (16 kHz mono)
                              │
                              ▼
            ┌─ split 60 s / stride 50 s (10 s overlap) ─┐
            │                                           │
          chunk 0    chunk 1    chunk 2   …   chunk N
            │         │          │              │
            ▼         ▼          ▼              ▼
       ┌───────── Gemini 3 Pro via CLIProxy ─────────┐
       │  prompt + audio → timed lines (local ms)    │
       └─────────────────────────────────────────────┘
                              │
                              ▼
         per-chunk results, each shifted by chunk offset
                              │
                              ▼
                 merge across overlap regions (Appendix B)
                              │
                              ▼
                    single ordered line list
                              │
                              ▼
                 write {youtube_id}_lyrics.json
```

## Appendix A — Gemini prompt

```
System:
You are a precise sung-lyrics transcription assistant. Your only output format
is timed lines in this exact schema, one per line, nothing else:
(MM:SS.x --> MM:SS.x) text

User:
Transcribe the sung vocals in the attached audio.

Rules:

1. Timestamps are LOCAL to this audio chunk, starting at 00:00. Do NOT offset.

2. COVERAGE — Output a timed line for EVERY sung phrase. Do NOT skip or
   collapse repeated choruses or refrains. If a phrase is sung 5 times, output
   5 separate lines. Do not summarize.

3. SHORT LINES — Break long phrases into short, separately timed lines.
   - Break at every comma, semicolon, or breath pause.
   - Example: "To know Your heart, oh it's the goal of my life, it's the aim
     of my life" MUST be 3 separate lines:
       (07:23.0 --> 07:25.5) To know Your heart
       (07:26.0 --> 07:30.0) Oh it's the goal of my life
       (07:31.0 --> 07:34.0) It's the aim of my life
   - Aim for ≤ 8 words per output line where the phrasing allows.

4. PRECISION — Line start_time = the exact moment the first syllable BEGINS
   being sung (not the breath before, not a preceding beat). Line end_time =
   the last syllable finishes, before the next silence.

5. SILENCE — If the chunk has no vocals (instrumental only, or pre-roll
   silence), output exactly: # no vocals

6. OUTPUT FORMAT — Output ONLY timed lines. No intro text, no commentary,
   no markdown fences, no summary at the end.

7. DO NOT HALLUCINATE — Only transcribe what you actually hear. If you hear
   a word not matching the reference lyrics below, still write what you hear.
   If the reference has a line that doesn't appear in this audio chunk, do
   NOT include it.

Reference lyrics for this song (extracted from YouTube description — may be
out of order, missing chorus repeats, or contain extra phrases not in this
chunk):
<description_lyrics here>

This chunk covers audio from {chunk_start_s} to {chunk_end_s} of the full
song ({full_duration_s} total). The chunk may start or end mid-phrase.
```

## Appendix B — Overlap merge algorithm

Each overlap region is 10 s wide. Within a 10 s window two chunks both
transcribed the same audio and produced line entries for the same sung content.
The merge needs to emit each sung line exactly once with the most precise
available timing.

Algorithm per overlap region:
1. Collect all lines from chunk N ending after the overlap-region start.
2. Collect all lines from chunk N+1 starting before the overlap-region end.
3. For each pair `(line_a, line_b)` where `line_a.text` and `line_b.text`
   normalize to the same word sequence AND their start times are within 1500 ms:
   - Treat as duplicate. Keep the entry whose start_time is further from the
     chunk boundary (less boundary-effect).
4. Any unpaired lines in the overlap region from either chunk are kept as-is
   (may represent lines one chunk missed).

Text normalization for dedup: lowercase, strip punctuation, collapse whitespace,
no stemming.

## Risks & open questions

- **Gemini 3 Pro availability through CLIProxy Google OAuth free tier**: the
  proxy currently has only Claude logged in. User needs to run
  `CLIProxyAPI.exe -login` once to add the Google account. If the free tier
  rate-limits too aggressively on bulk catalog reprocessing (200+ songs × ~10
  chunks each ≈ 2000 calls), we may need a cool-down or queue pacing.
- **Latency**: ~13 chunks × 5 s per call ≈ 1 minute per 11-min song to produce
  lyrics. Acceptable for the background worker; would be slow if someone plays
  a newly-downloaded song before worker finishes.
- **Prompt drift across Gemini model updates**: the Google OAuth path may get
  a newer model silently. Worth caching full raw responses (step B of Phase 1
  above) so we can re-merge without re-calling when the API behaviour changes.
- **Instrumental silence handling**: `# no vocals` from a chunk should map to
  "panel clears during this range", not "panel stays stale on last line". The
  existing `LyricsState::update` already clears between entries, so leaving
  gaps in the line list produces the right behavior.
- **Cost if the free tier fails us**: Gemini 2.5 Pro paid is ~$0.002 per chunk
  at 60 s; full catalog reprocess ≈ $4 one-shot. Fine as a fallback.

## Out of scope

- Porting the chunking Python logic into Rust (the prototype stays Python;
  the Rust port reads the same audio + issues same API calls but via reqwest).
- Any changes to OBS, NDI, or Resolume push architecture.
- Any changes to the dashboard karaoke panel (it will render line-level only
  until word-level work resumes).
