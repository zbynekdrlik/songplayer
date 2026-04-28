# Lyrics Pipeline Redesign — WhisperX + Spotify Tier-1 + Anchor-Sequence Reconciliation

**Date:** 2026-04-28
**Status:** Design — awaiting user approval before plan
**Replaces:** Gemini chunked transcription pipeline (v11–v19)

## Goal

Replace the current Gemini-based lyrics pipeline with a quality-first architecture that solves three production pain points the user has articulated repeatedly:

1. **Unreliable line timing**, especially on fast songs. Current 1-second renderer lead (`DEFAULT_LYRICS_LEAD_MS = 1_000`) is a band-aid for chunk-boundary timing drift; on a 1.5s line it advances 67% of line duration, causing the wall to flip mid-phrase.
2. **Lines too long for LED wall** rendering. Current pipeline ships whatever line lengths the source provides (long verses, sometimes whole stanzas). Visual target is 32 chars/line (ProPresenter-style).
3. **Cost.** ~€50 spent on Gemini API tokens with weak results. ~€0.30–0.75/song with chunk-boundary artifacts and quality oscillation.

## Verification basis

A 3-week empirical investigation tested 5 ASR engines against manual `yt_subs` ground truth on 3 English worship songs covering short, medium, and long-form (3.7 min, 6.8 min, 11.8 min):

| Engine | Praise <1s matches | Anointed One <1s | There Is A King <1s | Notes |
|---|---|---|---|---|
| **WhisperX cloud (winner)** | 0 | 3 | **18** | Best long-form; native VAD chunking |
| Chunked CrisperWhisper local | 6 | 1 | 5 | Mixed; medical-speech bias; OOM-prone |
| Parakeet TDT v3 chunked | 1 | 2 | 2 | Comparable on first lines; needs chunking |
| Whisper-Turbo (thomasmol) | 0 | 2 | 2 | Coarse segments |
| VibeVoice ASR (Gradio) | 1 | 1 | 3 (truncated) | Line-only; max_tokens caps long-form |

WhisperX (`victor-upmeet/whisperx` on Replicate, Whisper-large-v3 + wav2vec2-CTC forced alignment) won decisively on long-form line timing — the hardest case for our worship catalog. Sub-second timing on 6 consecutive verses of "There Is A King." Total verification spend: ~$0.20.

## Architecture

```
YouTube → FLAC + video.mp4 (already cached)
              ↓
Vocal isolation + dereverb  ← UNCHANGED FROM CURRENT
  Mel-Roformer (vocal stem) + anvuew (dereverb)
  scripts/lyrics_worker.py preprocess-vocals
  Outputs *_vocals_dereverbed.wav (16 kHz mono float32)
  Runs on win-resolume RTX 3070 Ti, off-hours, BELOW_NORMAL
              ↓
Tier 1 — Text + line-timing (free, parallel fetch)
  • Spotify LINE_SYNCED proxy (akashrchandran/spotify-lyrics-api)
  • LRCLib (exact match by artist+title+duration)
  • YouTube manual subs (yt_subs, has_timing only — autosub banned)
  • Genius (text-only — no timing, used for anchor-reconciliation reference)
  If Tier 1 has line timing → ship directly, skip Tier 2
              ↓ (when Tier 1 has only text or misses entirely)
Tier 2 — WhisperX on Replicate
  victor-upmeet/whisperx (Whisper-large-v3 + wav2vec2-CTC alignment)
  INPUT: *_vocals_dereverbed.wav (NOT raw mix)
  Native VAD chunking handles long-form internally
  Optional 60s/10s chunking trigger for songs > N minutes
              ↓
Anchor-sequence reconciliation (karaoke-gen pattern)
  Match WhisperX transcript ↔ authoritative text from Tier 1
  Replace WhisperX mishearings with correct lyrics
  KEEP WhisperX word/line timing
              ↓
Line-length splitter — port of SubtitleEdit TextSplit.AutoBreak
  32-char target, priority: dialog dash → sentence-end → comma → word boundary balance
              ↓
Translation — Claude EN→SK (unchanged from current)
              ↓
Persist to DB + JSON cache
```

## Components

### `AlignmentBackend` trait

Pluggable abstraction over the ASR/alignment engine. Lets us swap WhisperX for a future SOTA model (or A/B against Parakeet/CrisperWhisper later) without rewriting the pipeline.

```rust
#[async_trait]
pub trait AlignmentBackend: Send + Sync {
    fn id(&self) -> &'static str;             // "whisperx-large-v3", etc.
    fn revision(&self) -> u32;                // bumped per-backend on contract change
    fn capability(&self) -> AlignmentCapability;

    async fn align(
        &self,
        vocal_wav_path: &Path,
        reference_text: Option<&str>,
        language: &str,                  // BCP-47, e.g. "en"
        opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError>;
}

pub struct AlignmentCapability {
    pub word_level: bool,           // WhisperX = true
    pub segment_level: bool,        // every backend
    pub max_audio_seconds: u32,     // WhisperX native handles long-form
    pub languages: &'static [&'static str],  // BCP-47 codes
    pub takes_reference_text: bool, // future: WhisperX `initial_prompt`
}

pub struct AlignedTrack {
    pub lines: Vec<AlignedLine>,
    pub provenance: String,         // e.g. "whisperx-large-v3@rev1"
    pub raw_confidence: f32,        // self-reported, NOT a quality gate
}

pub struct AlignedLine {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub words: Option<Vec<AlignedWord>>,  // None for segment-only backends
}
```

Initial impl shipped: `WhisperXReplicateBackend`. Future impls (Parakeet, CrisperWhisper, etc.) added per tracked issues.

### Tier 1 — Text + line-timing fetchers

Existing `gather.rs` simplified. Each fetcher returns `CandidateText { source, lines: Vec<String>, line_timings: Option<Vec<(u32, u32)>>, has_timing: bool }`. Run in parallel; collect all candidates.

- **`SpotifyLyricsFetcher`** — issue #52 implementation. Manual `spotify_track_id` per video (DB column `videos.spotify_track_id TEXT NULL`). Hits `https://spotify-lyrics-api-khaki.vercel.app/?trackid=<id>`. Skips on 404 / `error: true`. Returns `LINE_SYNCED` when available.
- **`LrclibFetcher`** — exact-match endpoint `https://lrclib.net/api/get?artist_name=&track_name=&duration=`. Returns synced or plain lyrics.
- **`YtManualSubsFetcher`** — current `YtManualSubsProvider`, but only `has_timing=true` (manual subs). Autosub remains banned (`feedback_no_autosub.md`).
- **`GeniusFetcher`** — text only, no timing. Used as additional anchor for the reconciler.

**Tier 1 short-circuit:** If any fetcher returns `has_timing=true` AND the line count looks plausible (≥10 lines), ship directly. Skip Tier 2. Provenance: `tier1:spotify` / `tier1:lrclib` / `tier1:yt_subs`.

**No anchor reconciliation on Tier 1 short-circuit** — these sources are authoritative for both text and timing.

### Tier 2 — WhisperX on Replicate

Implementation: `WhisperXReplicateBackend` impl of `AlignmentBackend`.

- Reference template: `victor-upmeet/whisperx` on Replicate
- Input: `*_vocals_dereverbed.wav` (already produced by Mel-Roformer + dereverb)
- API: explicit upload via `client.files.create()` then `client.predictions.create()` with `audio_file: <url>` + `language: "en"` + `align_output: true` + `diarization: false`
- Output: `segments[]` with `words[]` per segment (word-level word-aligned via wav2vec2-CTC)
- Cost: $0.035/song. ~5–15s wall on cloud A100. Native long-form via faster-whisper internal VAD.

**Optional chunking trigger.** Configurable threshold (default: never trigger). When audio > N minutes AND `WhisperX` segment count is < expected ratio (e.g. < `audio_minutes` segments), retry with 60s/10s chunking pattern from existing `gemini_chunks.rs::plan_chunks`. Mirror the overlap-merge dedup logic.

**Rate-limit + retry.** Replicate burst limit is 1 req with <$5 balance. Wrap predictions with: 12s spacing between calls; explicit retry on 429 with exponential backoff (10s → 60s, max 4 attempts).

### Anchor-sequence reconciliation

When Tier 1 has authoritative TEXT (Spotify, LRCLib, Genius) but no timing, AND Tier 2 has timing, we reconcile: walk WhisperX's word sequence in order, match against authoritative text, replace mishearings while keeping the timestamps.

Pattern from `karaoke-gen` `LyricsCorrector`:
1. Tokenize both transcripts to lowercased word lists
2. Compute longest common subsequence (LCS) anchor pairs
3. For each anchor-bounded gap, replace WhisperX words with authoritative words; keep WhisperX's timing range
4. Output: `lines: Vec<{text: <authoritative>, start_ms: <whisperX>, end_ms: <whisperX>, words: <whisperX timed positions>}>`

Implementation: new `crates/sp-server/src/lyrics/reconcile.rs` (~300 lines). Pure deterministic Rust, no LLM call (replaces current `text_merge.rs` Claude call entirely).

### Line splitter — SubtitleEdit port

New `crates/sp-server/src/lyrics/line_splitter.rs`. Port the priority-ordered split logic from SubtitleEdit's `TextSplit.AutoBreak()` and `Utilities.AutoBreakLine()` (GPL-3.0 — read the algorithm, reimplement clean-room in Rust):

1. **Dialog dash** split first
2. **Sentence-ending punctuation** (`.!?…؟。？！`) — break early if the prefix fits
3. **Comma / pause chars** (`,，、`)
4. **Word-boundary balance** — minimize length difference between the two resulting lines
5. **Hard char-count fallback** — split at the rightmost word boundary ≤ max_chars

Configurable `max_chars` (default 32). Applied as a post-processing step on every shipped line, regardless of Tier 1 vs Tier 2 origin. CPS (chars-per-second) gate optional for v2.

Tests: SubtitleEdit-style fixtures with English worship lyrics; assert the priority order is preserved.

### Translation

Unchanged. Existing `translator.rs` (Claude EN→SK via CLIProxyAPI) stays. Per `feedback_claude_only_translation.md`, no Gemini fallback for translation. Fix the prompt when Claude refuses.

### Persistence + reprocess

- DB columns unchanged except: ADD `videos.spotify_track_id TEXT NULL` (issue #52)
- `LYRICS_PIPELINE_VERSION` bumps from current v19/v20 to **v21**. Per `feedback_pipeline_version_approval.md`, this requires explicit user approval at plan-execution time.
- `reprocess.rs` stale-bucket: re-queue all rows with `lyrics_pipeline_version < 21`. Smart-skip preserved for sources that don't change semantics.
- Cache JSON path unchanged: `<video_id>_lyrics.json`. Schema gains `provenance` and `reconciled_against` fields.

## Code deletions (~3500 LOC)

- `crates/sp-server/src/lyrics/gemini_provider.rs` (671)
- `crates/sp-server/src/lyrics/gemini_client.rs` lyrics paths (the translator-Gemini-fallback was removed per `feedback_claude_only_translation.md`)
- `crates/sp-server/src/lyrics/gemini_chunks.rs` (392)
- `crates/sp-server/src/lyrics/gemini_parse.rs` (112)
- `crates/sp-server/src/lyrics/gemini_prompt.rs` (75)
- `crates/sp-server/src/lyrics/gemini_audit.rs`
- `crates/sp-server/src/lyrics/description_provider.rs` (680) — ~0% production hit rate
- `crates/sp-server/src/lyrics/autosub_provider.rs` — already disabled
- `crates/sp-server/src/lyrics/qwen3_provider.rs` (236) — disabled, prior failure
- `crates/sp-server/src/lyrics/aligner.rs` — Gemini-chunk-specific
- `crates/sp-server/src/lyrics/assembly.rs` — Gemini-chunk-specific
- `crates/sp-server/src/lyrics/chunking.rs` — Gemini-chunk-specific (separate from gemini_chunks.rs which keeps `plan_chunks` for optional WhisperX chunking trigger)
- `crates/sp-server/src/lyrics/text_merge.rs` (219) — replaced by deterministic `reconcile.rs`
- `crates/sp-server/src/lyrics/bootstrap.rs` if Gemini-only
- `DEFAULT_LYRICS_LEAD_MS = 1_000` constant in `crates/sp-server/src/playback/renderer.rs:17` — band-aid removed, real timing from WhisperX/Tier-1
- `scripts/lyrics_worker.py align-chunks` and `preload` Qwen3 paths — `preprocess-vocals` stays

Rationale: per `feedback_no_legacy_code.md`, when replacing a code path delete the old one entirely; no deprecated stubs.

## Code that stays unchanged

- `provider.rs` — trait, simplified to match new `AlignmentBackend` shape
- `worker.rs` — queue loop simplified; new tier-stack orchestration
- `orchestrator.rs` — slimmed, runs the tier chain
- `gather.rs` — Tier 1 fetchers (LRCLib, Genius, yt_subs) unchanged; SpotifyLyricsFetcher added
- `quality.rs` — histogram gate kept as sanity check
- `renderer.rs` — drop the 1s lead, keep the rest (highlighter logic, etc.)
- `reprocess.rs` — stale-bucket logic unchanged; threshold updates to v21
- `translator.rs` — completely unchanged
- `mod.rs` — `LYRICS_PIPELINE_VERSION` constant bumped
- `scripts/lyrics_worker.py preprocess-vocals` — Mel-Roformer + anvuew dereverb stays

## Cost projection

- **600-song catalog reprocess (one-shot):** ~**$13–21** on WhisperX cloud, depending on Tier-1 hit rate. Each Tier-1-covered track costs $0 (no Tier-2 call); remaining tracks at $0.035 each. Estimated 30–40% Tier-1 coverage on worship music is unverified — see tracked issue "Verify Spotify hit-rate on full catalog".
- **Mel-Roformer + dereverb (existing):** ~30–50 GPU-hours one-shot on win-resolume RTX 3070 Ti, off-hours scheduling, takes 2–3 weeks calendar time to drain
- **Ongoing 50 new songs/month:** ~$1.05–1.75/mo cloud + ~3–5 GPU-hours/month local
- **Realistic monthly steady state: under $3/mo cloud spend**

The cost numbers depend on actual Tier-1 hit rate. If Tier-1 covers fewer tracks than estimated, cloud cost approaches $21 one-shot. If coverage is higher (Spotify's catalog is broad on commercial worship), cost approaches $13.

Verification spend during this design phase: ~$0.20 total.

## Constraints honored

- `feedback_no_legacy_code.md` — all replaced code deleted, no stubs
- `feedback_no_autosub.md` — autosub stays banned in Tier 1 (only `has_timing=true` yt_subs)
- `feedback_claude_only_translation.md` — translator unchanged, Claude only
- `feedback_line_timing_only.md` — words optional in `AlignedLine`; renderer falls back to line-level highlighting when `words: None`
- `feedback_pipeline_version_approval.md` — `LYRICS_PIPELINE_VERSION` bump requires user approval at plan-execution
- `feedback_winresolume_is_shared_event_machine.md` — Mel-Roformer + dereverb stays off-hours, BELOW_NORMAL, scene-active interlock
- `feedback_no_even_distribution.md` — no synthesized word timings; Tier 1 line-only outputs ship `words: None`
- `feedback_no_model_downgrade.md` — N/A (we're switching architecture, not downgrading within Gemini family)

## Tracked future-issue list (file as GitHub issues at plan-execution time)

These are explicitly out of scope for this design but worth tracking:

- **"Evaluate VibeVoice ASR with bumped max_tokens for long-form line-only timing"** — Modal/Gradio re-test once pipeline is shipping; potentially better line-only segmentation than WhisperX
- **"Evaluate CrisperWhisper local on win-resolume off-hours as Tier-2 alternative"** — chunked test showed 6 sub-1s on Praise; worth re-running with bigger sample
- **"Self-host WhisperX on win-resolume to drop cloud cost"** — cuDNN install issue (#1216) blocked first attempt; revisit when bandwidth allows
- **"A/B WhisperX vs Parakeet TDT v3"** — keep `AlignmentBackend` impl for Parakeet alongside WhisperX; data-driven choice
- **"Evaluate self-published Cog wrapper for parakeet-tdt-0.6b-v2"** — English-only TDT not on Replicate; build wrapper if quality demands it
- **"Verify Spotify hit-rate on full catalog"** — measure how many tracks Tier-1 covers in practice; informs whether to invest more in Tier-1 sources
- **"Migrate to next Whisper successor when OpenAI / community ships"** — `AlignmentBackend` trait makes this 1-impl swap
- **"CPS (chars-per-second) gate in line splitter"** — v2 enhancement; reject lines too long for the time available
- **"Modal-based catalog burn-down for Mel-Roformer + dereverb"** — paid alternative if 30–50 GPU-hour win-resolume schedule is too slow

## Non-goals

- Word-level synthesized timings — banned per `feedback_no_even_distribution.md`. Tier 1 line-only output ships `words: None`; renderer falls back to line-level highlighting
- AudioShake LyricSync — blocked by user cost filter (>$3/song)
- Cohere Transcribe / GPT-4o-transcribe — no word/line timestamps, unusable for karaoke
- VibeVoice ASR as primary — segment-only output, max_tokens truncation on long-form, defer to tracked issue
- Replacing translator (Claude EN→SK) — out of scope per user direction
- Re-introducing Demucs subprocess for any purpose other than Mel-Roformer vocal isolation
- Live event scheduling logic for Mel-Roformer — preserved as-is, not re-designed

## Open questions resolved during this design phase

- **Cloud vs local Tier 2:** Cloud (Replicate). Local WhisperX install hit cuDNN issue; tracked for later. Quality is identical model.
- **Dropping Mel-Roformer:** No. All verification ran on dereverbed vocals; dropping it is unverified.
- **Dropping Demucs:** Yes for the lyrics path specifically. Mel-Roformer + anvuew is the actual pipeline; "Demucs" was loose terminology earlier in the discussion.
- **Chunking always vs trigger:** Trigger only. WhisperX handles long-form natively; explicit chunking is fallback for songs WhisperX collapses on.
- **AlignmentBackend trait first or after?** First. Even with one impl shipping, the trait makes future swaps trivial.
- **VibeVoice future testing:** Yes, tracked issue. Not blocking this design.
- **CrisperWhisper future testing:** Yes, tracked issue. Not blocking.

## Acceptance criteria for the implementation

- [ ] `AlignmentBackend` trait + `WhisperXReplicateBackend` impl
- [ ] Tier-1 short-circuit when any fetcher has `has_timing=true`
- [ ] `SpotifyLyricsFetcher` (issue #52) integrated as Tier-1
- [ ] Anchor-sequence reconciler replaces `text_merge.rs`
- [ ] SubtitleEdit-port line splitter (32-char default)
- [ ] `DEFAULT_LYRICS_LEAD_MS` removed from renderer
- [ ] `LYRICS_PIPELINE_VERSION` bumped to v21 (with explicit user approval at plan-execution)
- [ ] All Gemini lyrics modules + qwen3 + autosub + description provider deleted
- [ ] Mel-Roformer + anvuew dereverb path unchanged; new pipeline reads `*_vocals_dereverbed.wav`
- [ ] Worker dispatches WhisperX with rate-limit-aware Replicate client
- [ ] Optional 60s/10s chunking trigger (configurable threshold; default never)
- [ ] Reprocess stale-bucket re-queues all v<21 rows
- [ ] Tracked GitHub issues filed for the future-issue list
- [ ] Spec-defined acceptance: WhisperX line-timing on the 3 verified yt_subs songs reproduces the verification numbers (≥18 sub-1s matches on "There Is A King")

---

**Approval status:** awaiting user spec-review.
