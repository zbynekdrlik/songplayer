# ASR Path: AssemblyAI U3-Pro + Claude-Merge for `unsupported_source` Bucket

**Status:** Approved 2026-05-19
**Closes:** Issue #115 (Phase 2 — but reframed as EXTENSION not SWAP)
**Pipeline version:** No bump. New rows only; whisperx-era rows untouched.

---

## Problem

The current lyrics pipeline gates expensive alignment (Demucs + whisperx forced-alignment) behind `is_allowed_text_source`. Only candidates with timed text (`yt_subs` / `lrclib` / `spotify` with `has_timing=true`) or curated `description` reach alignment. Songs whose `gather_sources` returns ONLY untimed text candidates (`genius`, `lrclib`-untimed) — or any combination the gate rejects — are marked `lyrics_source = 'unsupported_source'` and never get karaoke output.

This is the bulk of the ~200 maintained songs that today have no lyrics on the wall. The whisperx forced-alignment approach structurally cannot serve them: it needs a timed text reference.

Eval (PR #114, report `2026-05-19-assemblyai-universal-3-pro-r2.md`) confirmed AssemblyAI Universal-3 Pro as a true ASR backend that transcribes from audio without a text reference, with mean 7.6/10 quality and 5-of-5 wall-pass on the 5-fixture pilot.

This spec adds AAI as an **additional alignment path** for the `unsupported_source` bucket. Whisperx flow on bucket 2 (songs with timed text source) stays untouched.

---

## Scope

**In scope:**

- New `crates/sp-server/src/lyrics/asr_path/` sub-tree:
  - `mod.rs` — orchestrator entry point
  - `aai_backend.rs` — AssemblyAI HTTP client (Rust port of `eval/lyrics/backends/assemblyai_universal_3_pro.py`)
  - `claude_merge.rs` — CLIProxyAPI client + JSON parser (NOT the existing `claude_merge.rs`, which is whisperx-era; new module under sub-tree)
  - `merge_prompt.rs` — testable prompt builder
  - `resolver.rs` — word-index → ms resolver, output sanitization
  - `tests.rs` — unit tests
- One new branch in `worker.rs::process_song`: when `is_allowed_text_source == false` AND `candidate_texts` non-empty → call `asr_path::run`.
- New settings key: `assemblyai_api_key` (already used in eval).
- Extend `canonical_source_regression_tests.rs` to pin 3–5 bucket-1 songs to expected `asr:*` labels.
- Two new lyrics-source labels: `asr:aai-u3-pro+claude-merge` (Claude used the untimed source as reference) and `asr:aai-u3-pro` (Claude disagreed and fell back to raw AAI silence-gap split).
- VERSION bump on dev (per `version-bumping.md`).

**Out of scope:**

- Any change to existing whisperx code path. `whisperx_replicate.rs`, all `text_reference_merge*.rs`, `timed_reference_merge.rs`, and the existing whisperx-era `claude_merge.rs` stay byte-for-byte identical.
- `LYRICS_PIPELINE_VERSION` bump. Per `feedback_no_bump_until_proven` and `feedback_pipeline_version_approval`: only the user calls a bump after wall-verification. This spec only writes new rows for songs that currently have `lyrics_source IN ('unsupported_source', NULL)`.
- Bucket 2 / 3 / 4 changes (songs with timed source, songs in asr_gap quarantine, songs with zero candidates). All unchanged.
- Retiring whisperx. Whisperx remains the primary backend for timed-text bucket 2.
- Word-level karaoke output. All asr_path lines ship with `words: None` per `feedback_line_timing_only`.

---

## Architecture

```
crates/sp-server/src/lyrics/
├── asr_path/                  # NEW sub-tree
│   ├── mod.rs                 # pub async fn run(ctx, audio_path) -> Result<AsrOutput>
│   ├── aai_backend.rs         # AAI HTTP client + AaiTranscript types
│   ├── claude_merge.rs        # CLIProxyAPI client + ClaudeMergeResult parser
│   ├── merge_prompt.rs        # build_prompt(aai, untimed_text, source, lang) -> String
│   ├── resolver.rs            # resolve(merged, aai) -> Vec<LyricsLine>
│   └── tests.rs               # unit tests
├── worker.rs                  # MODIFIED: one new branch in process_song
├── canonical_source_regression_tests.rs  # MODIFIED: add bucket-1 pins
└── (everything else)          # UNTOUCHED
```

**Reused from existing flow (zero rebuild):**

| Existing module | Role in asr_path |
|---|---|
| `bootstrap.rs` + `audio_chunking.rs` | Demucs/anvuew dereverb to 16k vocal WAV. Same preprocess as whisperx. |
| `gather.rs` | Source-gathering. asr_path consumes the same `Vec<CandidateText>` whisperx consumes. |
| `tier1::CandidateText`, `provider::*`, `renderer::LyricsLine` | Same output types. |
| `translator.rs` | EN→SK translation runs as a separate post-step. Inherited free. |
| `audit_ctx.rs` | Per-song audit log. Extended with `asr_path` events; same writer. |

---

## Components

### `aai_backend.rs`

```rust
pub struct AaiWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
}

pub struct AaiTranscript {
    pub words: Vec<AaiWord>,
    pub raw_text: String,
}

pub async fn transcribe(api_key: &str, audio_path: &Path) -> Result<AaiTranscript, AaiError>
```

POST `/v2/upload` with audio binary → POST `/v2/transcript` with `speech_model: "universal-3"`, `word_timestamps: true`, `format_text: true` → poll `/v2/transcript/{id}` until `status == completed`. Parse `words[]` into `Vec<AaiWord>`. Mirrors `eval/lyrics/backends/assemblyai_universal_3_pro.py` defaults exactly.

Retry policy: 3 attempts on 5xx / network, exponential backoff (5s → 20s → 60s). 429 → log `aai_quota_exhausted`, surface on dashboard, no row write. AAI `status: error` or empty word list → return error → orchestrator quarantines as `asr_gap`.

### `claude_merge.rs`

```rust
pub struct ClaudeMergeInput<'a> {
    pub aai_words: &'a [AaiWord],
    pub untimed_text: &'a str,
    pub untimed_source: &'a str,   // "genius" | "lrclib" | ...
    pub language: Option<&'a str>,
}

pub struct MergedLine {
    pub text: String,
    pub start_word_idx: usize,
    pub end_word_idx: usize,        // inclusive
}

pub struct ClaudeMergeResult {
    pub lines: Vec<MergedLine>,
    pub disagreement: bool,
    pub notes: String,
}

pub async fn merge(
    proxy_url: &str,
    input: &ClaudeMergeInput<'_>,
) -> Result<ClaudeMergeResult, MergeError>
```

POSTs to CLIProxyAPI (same channel as `translator.rs`). Schema-validates Claude's JSON response. Schema **rejects** `start_ms` / `end_ms` fields — only word indices are accepted. Malformed → retry once with stricter prompt, second fail → return `disagreement: true`.

Refusal / "copyrighted" policy response → return `disagreement: true`. No Gemini fallback (per `feedback_claude_only_translation` — translation is Claude-only; same channel discipline applies here).

### `merge_prompt.rs`

System prompt (constant):

```
You are a karaoke-lyrics editor.

INPUT
- ASR transcript with per-word timings (the audio truth).
- Reference lyrics text from {source} (untimed, may have errors, may be wrong version).

GOAL
- Produce singable line-level karaoke lyrics that match what the singer actually sings.
- Use ASR timings as the timing source; use reference text to correct mishears and
  pick natural line breaks.

RULES
1. Output JSON only. No prose. Schema below.
2. Each output line MUST reference contiguous ASR word indices
   [start_word_idx..end_word_idx] (inclusive).
3. NEVER invent words not present in the ASR transcript. Reference text can correct
   spelling/word-choice ONLY where ASR clearly mis-heard a word that the reference
   disambiguates.
4. NEVER emit ms values. Only word indices.
5. Line splits chosen for vocal phrasing — group what a singer sings as one breath
   / phrase, not where silence falls.
6. If reference text disagrees too much with ASR (different song / different version
   / wrong language), set "disagreement": true and return empty lines[].
7. Drop ASR ad-libs / "yeah" / "hey" / repeated filler that are clearly not in the
   reference text. Skip them — do NOT include in any line.

SCHEMA
{
  "disagreement": bool,
  "notes": string,
  "lines": [
    { "text": string, "start_word_idx": int, "end_word_idx": int }
  ]
}
```

User prompt template (per-song):

```
SOURCE: {untimed_source}
LANGUAGE: {language or "unknown"}

REFERENCE TEXT:
{untimed_text}

ASR TRANSCRIPT (word_idx: text @ start_ms..end_ms):
0: "no" @ 0..500
1: "pulse" @ 500..900
...
```

### `resolver.rs`

```rust
pub fn resolve(
    merged: &ClaudeMergeResult,
    aai: &AaiTranscript,
) -> Result<Vec<LyricsLine>, ResolverError>
```

For each `MergedLine`:

1. Validate `start_word_idx <= end_word_idx < aai.words.len()`. Malformed → `ResolverError::OutOfRange` → orchestrator treats as disagreement.
2. `line.text = merged.text`.
3. `line.start_ms = aai.words[start_idx].start_ms`.
4. `line.end_ms = aai.words[end_idx].end_ms`.
5. `line.words = None`.

After all lines built: apply line-only sanitizer (monotonic `start_ms`, no overlap, minimum 200ms duration). Same invariants as existing `sanitize_word_timings` but at line granularity.

### Orchestrator (`asr_path/mod.rs`)

```rust
pub enum AsrOutput {
    Merged { lines: Vec<LyricsLine>, source: &'static str },  // "asr:aai-u3-pro+claude-merge"
    Fallback { lines: Vec<LyricsLine>, source: &'static str }, // "asr:aai-u3-pro"
    Quarantine { reason: &'static str },                       // empty AAI / hard failure
}

pub async fn run(
    ctx: &SongCtx,
    audio_path: &Path,
) -> Result<AsrOutput, AsrError>
```

Flow:

1. Vocal preprocess (reuse `bootstrap` + `audio_chunking`) → 16k vocal WAV.
2. `aai_backend::transcribe(api_key, wav)` → `AaiTranscript`. Empty words → `Quarantine`.
3. Pick best untimed text candidate from `ctx.candidate_texts` (priority: `genius` > `lrclib` > others).
4. `claude_merge::merge(...)` → `ClaudeMergeResult`.
5. If `result.disagreement == true` OR `result.lines.is_empty()`: build fallback lines from AAI silence-gap split (helper using `LINE_GAP_MS=400` — same constant as eval Python). Return `Fallback`.
6. Else `resolver::resolve(...)` → `Vec<LyricsLine>`. Return `Merged`.

---

## Data flow

```
ctx.audio_path (FLAC sidecar)
  → demucs/dereverb (existing preprocess) → vocal.wav (16k mono)
  → aai_backend::transcribe → AaiTranscript
  → claude_merge::merge(aai, untimed_text) → ClaudeMergeResult
       ├─ disagreement=false, lines non-empty
       │     → resolver::resolve → Vec<LyricsLine>
       │     → persist as source="asr:aai-u3-pro+claude-merge"
       └─ disagreement=true OR lines empty
             → silence-gap fallback split from AAI words
             → persist as source="asr:aai-u3-pro"
  → DB row updated, lyrics JSON written
  → dashboard refresh event
```

---

## Worker integration (`worker.rs`)

Current shape (simplified):

```rust
if !is_allowed_text_source(&ctx.candidate_texts) {
    mark_unsupported_source(&row);
    return Ok(());
}
// ... whisperx alignment ...
```

After change:

```rust
if !is_allowed_text_source(&ctx.candidate_texts) {
    if has_any_text_candidate(&ctx.candidate_texts) {
        match asr_path::run(&ctx, audio_path).await {
            Ok(AsrOutput::Merged { lines, source }) => persist_lyrics(&row, lines, source),
            Ok(AsrOutput::Fallback { lines, source }) => persist_lyrics(&row, lines, source),
            Ok(AsrOutput::Quarantine { reason }) => mark_asr_gap(&row, reason),
            Err(e) => {
                tracing::error!("asr_path failed: {e}");
                // leave row unprocessed for next tick
            }
        }
    } else {
        mark_no_text_source(&row);
    }
    return Ok(());
}
// whisperx alignment — UNTOUCHED
```

`has_any_text_candidate` = `!ctx.candidate_texts.is_empty()`. Guard ensures asr_path never runs on songs with zero text candidates (bucket 4) and never runs on songs with timed source (bucket 2).

---

## Error handling

| Stage | Failure | Response |
|---|---|---|
| Demucs/dereverb | preprocess fails | reuse existing whisperx-path error label, mark row, no new error type |
| AAI upload | HTTP 5xx / timeout / network | 3-attempt exponential backoff (5s → 20s → 60s); after 3, leave row unprocessed |
| AAI transcribe | HTTP 429 (quota) | log `aai_quota_exhausted`, surface on dashboard, no row write, no silent retry |
| AAI transcribe | `status: error` or empty word list | quarantine row with `source = 'asr_gap'` |
| Claude merge | network / 5xx | 3-attempt exponential; after 3, fallback path |
| Claude merge | policy refusal | fallback path |
| Claude merge | malformed JSON / schema fail | retry once with stricter prompt; second fail → fallback path |
| Claude merge | `disagreement: true` | fallback path |
| Resolver | word_idx out-of-range | fallback path (treat result as malformed) |
| Persist | DB error | bubble up — worker handles same as today |

**Never** writes a row with synthesized / fake content. **Never** silently retries on quota — user is told.

---

## Lessons-locked guards (regression hard stops)

| Past disaster | Version | Guard in asr_path |
|---|---|---|
| LLM can't emit exact-length ms arrays | v15 | Claude returns word INDICES only. Schema rejects ms fields. |
| Word-timing duplicates / backward starts | v8–v10 | Output `words: None` (line-only). |
| Synthesized even-distribution word timings | v17/v18 | Resolver assigns `line.start_ms` = first matched word; `line.end_ms` = last matched word. No interpolation. |
| Catalog-wide reprocess from version bump | every bump | NO `LYRICS_PIPELINE_VERSION` bump. asr_path writes only `unsupported_source` / `NULL` rows. |
| Self-reported confidence as quality | v14 era | No confidence field in output. Wall-verify is the only quality gate. |
| Merge-layer heuristics ("absorb dropped", "phantom") | v12–v15 | No heuristic code. Claude judges + emits indices; server resolves. `disagreement` is a single bool. |

Three pre-flight unit assertions in `asr_path/tests.rs`:

1. `asr_path_never_emits_word_timings` — every `LyricsLine.words == None`.
2. `asr_path_never_synthesizes_ms` — `line.start_ms` and `line.end_ms` are sourced from `aai_words[idx]` exactly.
3. `asr_path_rejects_claude_ms_output` — schema deserialization fails if Claude response contains `start_ms` / `end_ms`.

Plus regression in `worker_tests.rs`:

- `process_song_routes_to_whisperx_when_yt_subs_timed` — timed yt_subs MUST trigger whisperx, NOT asr_path.

---

## Testing

**Unit (no network):**

- `aai_backend_tests.rs` — wiremock for HTTP shape + golden response → AaiTranscript.
- `claude_merge_tests.rs` (new under `asr_path/`) — golden responses, malformed JSON, out-of-range indices, disagreement.
- `resolver_tests.rs` — given (aai, merged), assert LyricsLine timings + words: None + sanitizer applied.
- `merge_prompt_tests.rs` — assert prompt contains all words, source label, language hint.

**Integration (CI-gated by env, real API):**

- `asr_path_end_to_end_test.rs` — feature-gated by `ASR_TEST_AUDIO_PATH`. Uses one cached vocal WAV, real AAI + real CLIProxyAPI. Not in default `cargo test`.

**Worker structural:**

- `process_song_routes_to_whisperx_when_yt_subs_timed` (regression).
- `process_song_routes_to_asr_path_when_only_genius` (new positive).
- `process_song_marks_no_text_source_when_no_candidates` (preserved).

**Canonical-source regression CI:**

- Pin 3–5 currently-`unsupported_source` songs to expected post-asr-path label.

**Wall-verification (manual, post-deploy):**

- `/lyrics-verify` on 5 bucket-1 songs after first deploy.
- One song at a time. If broken, fix the code, never propose batch. Per `feedback_song_by_song_iteration`.

---

## Settings

New: `assemblyai_api_key` (read from `settings` table, same pattern as `replicate_api_token` and `gemini_api_key`).

Existing: `replicate_api_token` stays (whisperx still uses it).

---

## Source-label vocabulary

After this change, the `lyrics_source` column gains two new values:

- `asr:aai-u3-pro+claude-merge` — AAI transcribed + Claude merged with untimed source.
- `asr:aai-u3-pro` — Claude disagreed or merge failed; raw AAI silence-gap split shipped.

Existing values (`yt_subs`, `lrclib`, `genius`, `description`, `ensemble:*`, `whisperx`, `unsupported_source`, `asr_gap`, `no_text_source`) unchanged.

---

## Out of scope (do not include in implementation PR)

- Bumping `LYRICS_PIPELINE_VERSION`.
- Touching whisperx code.
- Catalog-wide reprocess of bucket 2.
- Dashboard UI for asr_path-specific diagnostics (existing /lyrics-verify suffices).
- Provider-monitoring CRON.
- Per-language tuning of the merge prompt (one prompt, EN-first; revisit only after wall data).

---

## Acceptance

- Whisperx flow unchanged on bucket 2 (canonical-source regression CI green).
- 5 bucket-1 fixtures wall-verify on the singing wall before any bulk reprocess.
- Quota / Claude refusal failure modes visible to user via dashboard / logs, never silent.
- No new `LYRICS_PIPELINE_VERSION` value.
- No row carries synthesized word timings or even-distribution line timings.
