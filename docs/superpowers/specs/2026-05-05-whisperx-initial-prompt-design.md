# WhisperX initial_prompt: Description Lyrics as LM Bias

## Goal

Pass each song's extracted description lyrics to WhisperX's `initial_prompt` parameter so the underlying Whisper LM is biased toward expected phrases. Eliminates phantom-token tokenization during sustained vowels (id=132 2:57) without introducing the segment concatenation seen in chunked mode (id=132 chorus under-counts).

## Background

Two prior wall-verify findings on id=132 "Holy Forever":

1. **Full-audio mode (no chunking, no prompt)**: WhisperX correctly captures all 9 "Holy forever" repeats. But during the sustained "Holy forever" at 2:55, the LM hallucinated "your", "name" tokens (whisperx idx 156-157) for the sustained-vowel acoustic content. `description_merge` matched those phantom tokens to the next ref line "Your name is the highest", so the wall switched to that line at 2:57.7 — mid-sustained-forever. User wall-verified the issue.

2. **Chunked mode (60s/10s overlap, no prompt)**: 2:57 phantom-token issue gone, but WhisperX's per-chunk LM (60s context only) concatenated multiple sung phrases into single segments (e.g. "Holy, holy forever You will always be Holy, holy forever" as ONE segment). `description_merge` matches one ref line per segment → other lines lost. Chorus counts under-emit: 7/9 "Holy forever", 5/6 "stands above", 2/3 "thrones".

Neither mode is correct. The root cause is WhisperX's LM lacking context about what the song actually contains. Description lyrics provide that context.

## Approach: Approach 1 (single-pass full-audio + initial_prompt)

Stay in full-audio mode (no chunking). Pass the song's description lyrics as `initial_prompt` to the Replicate `victor-upmeet/whisperx` model. The LM biases its predictions toward the prompt's vocabulary and phrasing. Sustained-vowel hallucinations become unlikely because the LM knows the real next phrase. Chorus repeat detection stays correct because full-audio context is preserved.

This is the simplest viable change: one Replicate inference per song, no chunking complexity, no two-pass merge logic.

## Components

### 1. Backend trait — `takes_reference_text` capability

`crates/sp-server/src/lyrics/backend.rs`. The `AlignmentCapability` struct already has the `takes_reference_text: bool` field. Set to `true` for `WhisperXReplicateBackend`.

The `align()` trait method already accepts `_reference_text: Option<&str>`. Drop the underscore prefix in the implementation; pass through to the Replicate input.

### 2. Replicate input — pass `initial_prompt`

`crates/sp-server/src/lyrics/whisperx_replicate.rs`. Current input JSON:

```rust
{
    "audio_file": url,
    "language": language,
    "align_output": true,
    "diarization": false,
    "batch_size": 32
}
```

Add `"initial_prompt": <text>` when `reference_text` is `Some`. Whisper's prompt token budget is 224 tokens — for a 25-line worship song description (~150-200 tokens), the full description fits.

### 3. Orchestrator — supply description text

`crates/sp-server/src/lyrics/orchestrator.rs`. Two `backend.align()` call sites (Tier-1 TextOnly and Tier-1 None paths). Both currently pass `None` for reference text.

For TextOnly path: assemble description lines from `text_candidates[best].lines` joined with `\n`. Pass as the second argument.

For None path: no description available, leave `None`.

### 4. Worker — disable chunking

`crates/sp-server/src/lyrics/orchestrator.rs`. Currently sets `chunk_trigger_seconds: Some(90)` (chunking on for any song > 90s). Change to `None` (or remove). Full-audio mode handles long-form natively when the LM has good context (which initial_prompt provides).

The `align_chunked` code path stays — keep as a future option. Just not used by default.

## Data Flow

1. Worker fetches song, gathers candidate texts (description, lrclib, genius, yt_subs).
2. `claude_merge::merge` picks best authoritative text → description path → `description_merge::process`.
3. Before that, the `Orchestrator::process` calls `backend.align(wav, Some(&description_lines_joined), language, AlignOpts::default())`.
4. `WhisperXReplicateBackend::align` includes `initial_prompt` in the Replicate JSON.
5. WhisperX returns segments biased by the prompt.
6. Returned `AlignedTrack` flows into `description_merge::process` as before.

No change to merge / Phase 1-5 / extension / absorption. The bias takes effect at WhisperX level only.

## Error Handling

If Replicate rejects the `initial_prompt` parameter (API change, malformed text), the call returns `BackendError::Rejected`. Existing fallback in `claude_merge::merge` covers this — caller sees an error, falls through to raw asr or alternate path.

If description text is empty (rare — most songs have description), pass `None` (skip the prompt) so behavior is identical to current code.

If description text is too long (>1000 chars approximating ~250 tokens), truncate to first ~800 chars and warn. Whisper silently truncates beyond 224 tokens but logging surfaces the issue for inspection.

## Testing

- **Unit test**: `align()` with `Some(text)` produces JSON containing `initial_prompt: <text>`. Without it, no `initial_prompt` field. Mock the Replicate client; assert request shape.
- **Wall-verify**: reprocess id=132. Check `descmerge_audit.json`:
  - Whisperx idx 154 ("forever") duration ≥ 200 ms (vs current 141 ms) — sustained vowel correctly captured.
  - No idx 155-157 ("cause", "your", "name") in 175-180 s region — phantom tokens gone.
  - Chorus counts: "Holy forever" 9, "stands above" 6, "thrones" 3.
  - L36 "Your name is the highest" starts at the actual sung "your" (≥ 184000 ms or wherever real phrase begins per audio).

If id=132 passes wall-verify, the change is correct. Apply to all 240 songs (clear `lyrics_source` for stale rows; worker auto-reprocesses).

## Rollout

1. Implement code changes in single PR.
2. Push, wait for CI green.
3. Reprocess id=132 (clear DB row, worker picks up).
4. User wall-verifies on sp-live.
5. If pass: clear `lyrics_source` for entire catalog (60 currently stuck, plus any that produce different output). Worker drains over time.
6. If fail: revert orchestrator change to `None` reference text; keep backend support for future.

## Cost

1 Replicate inference per song. Same as pre-chunking. Catalog reprocess: 240 inferences. No increase vs current (chunking was 7× per song; this drops back to 1×).

## Out of Scope

- Approach 2 (chunked + prompt) and Approach 3 (two-pass) deferred until Approach 1 is wall-verified. If insufficient, escalate.
- WhisperX model upgrade (large-v3 → turbo, etc.).
- Multi-model ensemble.
- Custom worship-corpus training.
