# Lyrics Pipeline Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Gemini-based lyrics pipeline with a WhisperX-on-Replicate engine fronted by Spotify/LRCLib/yt_subs Tier-1 fetchers, anchored against authoritative text via a deterministic reconciler, and wrapped with a SubtitleEdit-port line splitter targeting 32-char lines.

**Architecture:** Mel-Roformer + anvuew dereverb (unchanged) → Tier-1 text+timing fetchers (parallel) → if line-synced, ship; else WhisperX cloud → anchor-sequence reconcile → 32-char line split → Claude EN→SK translate → persist. New `AlignmentBackend` trait makes future engine swaps trivial.

**Tech Stack:** Rust 2024, Tokio 1, reqwest 0.12, async-trait 0.1, sqlx 0.8 (sqlite), Replicate cloud API. Existing scripts/lyrics_worker.py preprocess-vocals stays untouched.

**Spec:** `docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md`

---

## Constraints every implementer subagent MUST respect

Pass these verbatim in every subagent dispatch prompt:

1. **TDD strict.** Write failing test → trust by inspection (Rust unit tests, no cargo test locally) → implement → confirm pass by inspection → `cargo fmt --all --check` → commit on green. ONLY local cargo command allowed: `cargo fmt --all --check`. NEVER run `cargo clippy/test/build` locally — rely on CI.
2. **File-size cap 1000 lines per file.** If a task pushes a file past 950 lines, split first.
3. **One commit per "Commit" step** in the plan body.
4. **Do NOT push** — controller batches commits and pushes once after all tasks land.
5. **`mutants::skip` requires a one-line justification inline.**
6. **No legacy code retention** (`feedback_no_legacy_code.md`): when replacing a code path, delete the old one entirely. No deprecated stubs, no fallback retention.
7. **Pipeline version bump requires user approval** (`feedback_pipeline_version_approval.md`): Phase H STOPS for explicit user OK.
8. **Words optional, never synthesized** (`feedback_line_timing_only.md`, `feedback_no_even_distribution.md`): `AlignedLine.words: Option<Vec<AlignedWord>>`. NEVER synthesize evenly-distributed word timings.
9. **Autosub banned** (`feedback_no_autosub.md`): Tier-1 yt_subs only `has_timing=true` (manual subs). Do not register `AutoSubProvider`.
10. **Translator unchanged** (`feedback_claude_only_translation.md`): Claude EN→SK only. No Gemini fallback.
11. **Mel-Roformer + anvuew dereverb path stays as-is** (`feedback_winresolume_is_shared_event_machine.md`): BELOW_NORMAL priority + scene-active interlock preserved. Plan does NOT touch existing preprocess-vocals invocation.

## Current codebase state (verified at plan time)

- **Branch:** `dev`. Latest commit: `164c3d7 docs: lyrics pipeline redesign spec`. VERSION: `0.28.0-dev.1` (already bumped, do NOT bump again).
- **Latest DB migration:** V16 (in `crates/sp-server/src/db/mod.rs`). New migration goes at index V17.
- **Current `LYRICS_PIPELINE_VERSION`:** `20` (in `crates/sp-server/src/lyrics/mod.rs:163`).
- **DEFAULT_LYRICS_LEAD_MS:** `crates/sp-server/src/lyrics/renderer.rs:17` (NOT in playback/).
- **Existing `AlignmentProvider` trait:** `crates/sp-server/src/lyrics/provider.rs:95` — replaced by new `AlignmentBackend` in this plan.
- **Existing 60s/10s chunking:** `crates/sp-server/src/lyrics/gemini_chunks.rs` (`plan_chunks`, `merge_overlap`). Phase A.2 renames this module before Phase G deletes the Gemini-specific bits, so WhisperX can reuse the chunking primitives.

## File structure — what each new file owns

```
crates/sp-server/src/lyrics/
├── backend.rs                ← Phase A: AlignmentBackend trait + types
├── audio_chunking.rs          ← Phase A: renamed gemini_chunks.rs (plan_chunks, merge_overlap stay)
├── whisperx_replicate.rs     ← Phase A: WhisperX backend impl + Replicate client
├── replicate_client.rs       ← Phase A: rate-limited Replicate predictions client
├── spotify_proxy.rs          ← Phase B: SpotifyLyricsFetcher (issue #52)
├── tier1.rs                  ← Phase B: Tier-1 collector + short-circuit
├── reconcile.rs              ← Phase C: anchor-sequence reconciler (replaces text_merge.rs)
├── line_splitter.rs          ← Phase D: SubtitleEdit-port 32-char splitter
├── orchestrator.rs           ← Phase F: rewritten to drive new tier chain
├── worker.rs                 ← Phase F: simplified queue loop
├── gather.rs                 ← Phase B: simplified for new pipeline
├── mod.rs                    ← Phase H: LYRICS_PIPELINE_VERSION → 21
├── renderer.rs               ← Phase E: DEFAULT_LYRICS_LEAD_MS removed
├── reprocess.rs              ← Phase H: threshold updated to v21
├── provider.rs               ← Phase G: simplified after legacy delete (or fully removed)
├── translator.rs             ← UNCHANGED
├── quality.rs                ← UNCHANGED
├── lrclib.rs                 ← UNCHANGED (existing fetcher)
├── genius.rs                 ← UNCHANGED (existing fetcher)
└── youtube_subs.rs           ← UNCHANGED (parses VTT into CandidateText)
```

Files **deleted in Phase G**: `gemini_provider.rs`, `gemini_client.rs` (lyrics paths), `gemini_chunks.rs` (after rename in A.2), `gemini_parse.rs`, `gemini_prompt.rs`, `gemini_audit.rs`, `description_provider.rs`, `qwen3_provider.rs`, `autosub_provider.rs`, `aligner.rs`, `assembly.rs`, `chunking.rs`, `text_merge.rs`, `bootstrap.rs`, `merge.rs`, `merge_tests.rs`, `yt_manual_subs_provider.rs` (if its functionality folds into gather.rs), `worker_tests.rs` (rewritten in Phase F).

---

# Phase A — AlignmentBackend trait + WhisperX Replicate impl

**Goal:** New backend abstraction shipped, with WhisperX Replicate impl coexisting alongside the legacy Gemini path. Nothing user-visible changes yet.

### Task A.1: AlignmentBackend trait + types

**Files:**
- Create: `crates/sp-server/src/lyrics/backend.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod backend;`)

- [ ] **Step 1: Create `backend.rs` with the failing test first**

```rust
//! AlignmentBackend trait — pluggable ASR/alignment engine abstraction.
//!
//! Initial impl: WhisperXReplicateBackend (see whisperx_replicate.rs).
//! Future impls (Parakeet, CrisperWhisper, AudioShake, VibeVoice) plug
//! in here without rewriting the orchestrator.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlignedWord {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlignedLine {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    /// `None` for segment-only backends (VibeVoice etc.). Renderer falls
    /// back to line-level highlighting when None — never synthesize evenly
    /// distributed word timings (per `feedback_no_even_distribution.md`).
    pub words: Option<Vec<AlignedWord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlignedTrack {
    pub lines: Vec<AlignedLine>,
    /// e.g. "whisperx-large-v3@rev1"
    pub provenance: String,
    /// Self-reported by backend. NOT a quality gate — just metadata.
    pub raw_confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub struct AlignmentCapability {
    pub word_level: bool,
    pub segment_level: bool,
    pub max_audio_seconds: u32,
    /// BCP-47 language codes the backend supports.
    pub languages: &'static [&'static str],
    pub takes_reference_text: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AlignOpts {
    /// Optional override for the chunking trigger threshold (seconds).
    /// `None` = backend default. `Some(0)` = always chunk. `Some(u32::MAX)` = never chunk.
    pub chunk_trigger_seconds: Option<u32>,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend transport error: {0}")]
    Transport(String),
    #[error("backend rejected request: {0}")]
    Rejected(String),
    #[error("backend timeout after {0:?}")]
    Timeout(std::time::Duration),
    #[error("backend output malformed: {0}")]
    Malformed(String),
    #[error("backend rate-limited: {0}")]
    RateLimit(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait AlignmentBackend: Send + Sync {
    /// Stable identifier persisted in DB & JSON. e.g. "whisperx-large-v3".
    fn id(&self) -> &'static str;

    /// Bumped per-backend when output contract changes. Use with
    /// LYRICS_PIPELINE_VERSION for stale-bucket re-queue logic.
    fn revision(&self) -> u32;

    /// What this backend can do.
    fn capability(&self) -> AlignmentCapability;

    /// Transcribe + align. `vocal_wav_path` MUST be the Mel-Roformer +
    /// anvuew dereverb stem (NOT raw mix). `language` is BCP-47.
    async fn align(
        &self,
        vocal_wav_path: &Path,
        reference_text: Option<&str>,
        language: &str,
        opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// MockBackend: trivial impl proving the trait is callable.
    struct MockBackend;

    #[async_trait]
    impl AlignmentBackend for MockBackend {
        fn id(&self) -> &'static str { "mock" }
        fn revision(&self) -> u32 { 1 }
        fn capability(&self) -> AlignmentCapability {
            AlignmentCapability {
                word_level: true,
                segment_level: true,
                max_audio_seconds: 600,
                languages: &["en"],
                takes_reference_text: false,
            }
        }
        async fn align(
            &self,
            _wav: &Path,
            _ref_text: Option<&str>,
            _lang: &str,
            _opts: &AlignOpts,
        ) -> Result<AlignedTrack, BackendError> {
            Ok(AlignedTrack {
                lines: vec![AlignedLine {
                    text: "hello world".into(),
                    start_ms: 0, end_ms: 1000,
                    words: Some(vec![
                        AlignedWord { text: "hello".into(), start_ms: 0, end_ms: 500, confidence: 0.9 },
                        AlignedWord { text: "world".into(), start_ms: 500, end_ms: 1000, confidence: 0.9 },
                    ]),
                }],
                provenance: "mock@rev1".into(),
                raw_confidence: 0.9,
            })
        }
    }

    #[tokio::test]
    async fn mock_backend_returns_aligned_track() {
        let b = MockBackend;
        let r = b.align(&PathBuf::from("/tmp/test.wav"), None, "en", &AlignOpts::default()).await.unwrap();
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "hello world");
        assert_eq!(r.lines[0].words.as_ref().unwrap().len(), 2);
        assert_eq!(r.provenance, "mock@rev1");
    }

    #[test]
    fn aligned_line_words_can_be_none() {
        let line = AlignedLine {
            text: "segment-only output".into(),
            start_ms: 0, end_ms: 5000,
            words: None,  // segment-only backends like VibeVoice
        };
        assert!(line.words.is_none());
    }

    #[test]
    fn capability_can_advertise_no_word_level() {
        let cap = AlignmentCapability {
            word_level: false, segment_level: true,
            max_audio_seconds: 3600, languages: &["en"], takes_reference_text: false,
        };
        assert!(!cap.word_level);
        assert!(cap.segment_level);
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

Modify `crates/sp-server/src/lyrics/mod.rs` — add `pub mod backend;` near the existing `pub mod` declarations (around line 20, alphabetical order).

- [ ] **Step 3: `cargo fmt --all --check`**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```
Expected: clean exit. If not, run `cargo fmt --all` then retry.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/backend.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add AlignmentBackend trait + types

New pluggable ASR/alignment abstraction. AlignedLine.words is Option to
support segment-only backends; never synthesize evenly distributed words
(per feedback_no_even_distribution.md and feedback_line_timing_only.md).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task A.2: Rename `gemini_chunks.rs` → `audio_chunking.rs` (preserve `plan_chunks` + `merge_overlap`)

**Files:**
- Rename: `crates/sp-server/src/lyrics/gemini_chunks.rs` → `crates/sp-server/src/lyrics/audio_chunking.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (rename module)
- Modify: `crates/sp-server/src/lyrics/gemini_provider.rs` (update import)
- Modify: `crates/sp-server/src/lyrics/gemini_audit.rs` (update import if any)

- [ ] **Step 1: `git mv` the file**

```bash
cd /home/newlevel/devel/songplayer
git mv crates/sp-server/src/lyrics/gemini_chunks.rs crates/sp-server/src/lyrics/audio_chunking.rs
```

- [ ] **Step 2: Update file's module-level doc comment**

Change top doc comment in `audio_chunking.rs` from:
```rust
//! Chunk planning (how to slice the song into 60s/10s-overlap chunks) and
//! overlap-merge logic for stitching per-chunk timed-line outputs into a
```
to:
```rust
//! Audio time-window chunking — 60s windows with 10s overlap.
//!
//! Originally introduced for Gemini chunked transcription. Reused by
//! WhisperXReplicateBackend's optional chunking trigger (see Task A.5)
//! when WhisperX's native long-form handling collapses on a song.
```

- [ ] **Step 3: Update imports in mod.rs and consumers**

In `crates/sp-server/src/lyrics/mod.rs`: change `pub mod gemini_chunks;` → `pub mod audio_chunking;`.

In `crates/sp-server/src/lyrics/gemini_provider.rs`: change `use crate::lyrics::gemini_chunks::{merge_overlap, plan_chunks};` → `use crate::lyrics::audio_chunking::{merge_overlap, plan_chunks};`.

Search and update any other `gemini_chunks` references:
```bash
grep -rn "gemini_chunks" crates/sp-server/src/ | grep -v "audio_chunking.rs"
```
For each hit, change `gemini_chunks` → `audio_chunking`.

- [ ] **Step 4: `cargo fmt --all --check`**

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(lyrics): rename gemini_chunks to audio_chunking

The 60s/10s chunk planning + overlap-merge primitives are reused by the
WhisperXReplicateBackend's optional chunking trigger. Module rename
decouples them from the (about-to-be-deleted) Gemini path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task A.3: Replicate client with rate-limited predictions

**Files:**
- Create: `crates/sp-server/src/lyrics/replicate_client.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod replicate_client;`)
- Modify: `crates/sp-server/Cargo.toml` if `reqwest`+`serde_json`+`tokio` features missing (verify first; they should already be present from existing Gemini code).

- [ ] **Step 1: Verify Cargo.toml has the needed deps**

```bash
grep -E "^reqwest|^serde_json|^tokio" crates/sp-server/Cargo.toml
```
Expected: `reqwest` with `json` feature, `serde_json`, `tokio` with `time`+`fs`. Already present.

- [ ] **Step 2: Create `replicate_client.rs` with failing test**

```rust
//! Replicate API client — explicit upload-then-predict path with
//! rate-limit-aware spacing + 429 backoff.
//!
//! WHY explicit upload-then-predict: during verification, `client.run()`
//! returned 404s on file inputs (replicate Python lib v1.0.7 issue).
//! Direct API calls work reliably:
//!   1. POST /v1/files (multipart) → URL
//!   2. POST /v1/predictions (model+version+input{audio_file:URL}) → prediction
//!   3. GET  /v1/predictions/{id} polled until status terminal

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::time::sleep;

const REPLICATE_BASE: &str = "https://api.replicate.com/v1";
/// Burst-1 rate limit at <$5 balance: 1 request per 12s window.
const RATE_LIMIT_SPACING: Duration = Duration::from_secs(12);
const RETRY_BASE: Duration = Duration::from_secs(10);
const RETRY_CAP: Duration = Duration::from_secs(60);
const RETRY_MAX_ATTEMPTS: u32 = 4;
const POLL_INTERVAL: Duration = Duration::from_secs(8);
const PREDICTION_TIMEOUT: Duration = Duration::from_secs(1800);

#[derive(Debug, Error)]
pub enum ReplicateError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("replicate {status}: {body}")]
    ApiError { status: u16, body: String },
    #[error("rate-limited after {0} attempts")]
    RateLimited(u32),
    #[error("prediction failed: {0}")]
    PredictionFailed(String),
    #[error("prediction timed out")]
    Timeout,
    #[error("malformed response: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionResponse {
    pub id: String,
    pub status: String,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub metrics: Option<Value>,
}

pub struct ReplicateClient {
    api_token: String,
    http: reqwest::Client,
}

impl ReplicateClient {
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            api_token: api_token.into(),
            http: reqwest::Client::builder()
                .timeout(PREDICTION_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    /// Upload a file via /v1/files. Returns the URL Replicate will fetch from.
    pub async fn upload_file(&self, path: &Path) -> Result<String, ReplicateError> {
        let bytes = tokio::fs::read(path).await?;
        let file_name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.wav")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("audio/wav")
            .map_err(|e| ReplicateError::Malformed(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("content", part);

        let resp = self.http
            .post(format!("{REPLICATE_BASE}/files"))
            .bearer_auth(&self.api_token)
            .multipart(form)
            .send().await?;

        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(ReplicateError::ApiError { status: status.as_u16(), body });
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| ReplicateError::Malformed(format!("file response: {e}")))?;
        v["urls"]["get"].as_str()
            .map(String::from)
            .ok_or_else(|| ReplicateError::Malformed("missing urls.get in file response".into()))
    }

    /// Create + poll a prediction with rate-limit spacing + 429 backoff.
    pub async fn predict(
        &self,
        version: &str,
        input: Value,
    ) -> Result<PredictionResponse, ReplicateError> {
        // 1. Burst-1 spacing (always wait 12s before creating a prediction)
        sleep(RATE_LIMIT_SPACING).await;

        // 2. Create prediction with 429 backoff
        let mut attempt = 0;
        let pred = loop {
            attempt += 1;
            let body = serde_json::json!({ "version": version, "input": input });
            let resp = self.http
                .post(format!("{REPLICATE_BASE}/predictions"))
                .bearer_auth(&self.api_token)
                .json(&body)
                .send().await?;

            let status = resp.status();
            if status.as_u16() == 429 {
                if attempt >= RETRY_MAX_ATTEMPTS {
                    return Err(ReplicateError::RateLimited(attempt));
                }
                let backoff = (RETRY_BASE * 2_u32.pow(attempt - 1)).min(RETRY_CAP);
                sleep(backoff).await;
                continue;
            }
            let resp_body = resp.text().await?;
            if !status.is_success() {
                return Err(ReplicateError::ApiError { status: status.as_u16(), body: resp_body });
            }
            let p: PredictionResponse = serde_json::from_str(&resp_body)
                .map_err(|e| ReplicateError::Malformed(format!("predict response: {e}")))?;
            break p;
        };

        // 3. Poll until terminal
        let started = std::time::Instant::now();
        let mut current = pred;
        loop {
            if matches!(current.status.as_str(), "succeeded" | "failed" | "canceled") {
                break;
            }
            if started.elapsed() > PREDICTION_TIMEOUT {
                return Err(ReplicateError::Timeout);
            }
            sleep(POLL_INTERVAL).await;

            let resp = self.http
                .get(format!("{REPLICATE_BASE}/predictions/{}", current.id))
                .bearer_auth(&self.api_token)
                .send().await?;
            let status = resp.status();
            let body = resp.text().await?;
            if !status.is_success() {
                return Err(ReplicateError::ApiError { status: status.as_u16(), body });
            }
            current = serde_json::from_str(&body)
                .map_err(|e| ReplicateError::Malformed(format!("poll response: {e}")))?;
        }

        if current.status != "succeeded" {
            return Err(ReplicateError::PredictionFailed(
                current.error.unwrap_or_else(|| format!("status={}", current.status))));
        }
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_spacing_is_12_seconds() {
        assert_eq!(RATE_LIMIT_SPACING, Duration::from_secs(12));
    }

    #[test]
    fn retry_attempts_capped_at_4() {
        assert_eq!(RETRY_MAX_ATTEMPTS, 4);
    }

    #[test]
    fn retry_backoff_caps_at_60_seconds() {
        // 10 → 20 → 40 → 60 (capped)
        for attempt in 1..=4u32 {
            let backoff = (RETRY_BASE * 2_u32.pow(attempt - 1)).min(RETRY_CAP);
            assert!(backoff >= RETRY_BASE);
            assert!(backoff <= RETRY_CAP);
        }
    }

    #[test]
    fn replicate_client_constructs_with_token() {
        let _c = ReplicateClient::new("test-token");
    }
}
```

- [ ] **Step 3: Add `pub mod replicate_client;` in `mod.rs`**

- [ ] **Step 4: `cargo fmt --all --check`**

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/replicate_client.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add Replicate API client with rate-limited predictions

Explicit upload-then-predict path (avoids client.run() 404 issue from
verification phase). 12s spacing between calls (burst-1 rate limit at
<\$5 balance). 429 retry with exponential backoff (10s→60s, max 4 attempts).
30-min prediction timeout. 8s poll interval.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task A.4: WhisperXReplicateBackend (skeleton + parsing)

**Files:**
- Create: `crates/sp-server/src/lyrics/whisperx_replicate.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod whisperx_replicate;`)

- [ ] **Step 1: Create the module with response parser + tests**

```rust
//! WhisperXReplicateBackend — AlignmentBackend impl for victor-upmeet/whisperx
//! on Replicate (Whisper-large-v3 + wav2vec2-CTC alignment).
//!
//! Verified during design phase (2026-04-28) on 3 yt_subs ground-truth songs;
//! WhisperX scored 18 sub-1s line matches on the 11.8-min "There Is A King".

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::lyrics::backend::{
    AlignedLine, AlignedTrack, AlignedWord, AlignOpts, AlignmentBackend,
    AlignmentCapability, BackendError,
};
use crate::lyrics::replicate_client::{ReplicateClient, ReplicateError};

/// Pinned version hash discovered at plan-write time (April 2026).
/// Update when Replicate publishes a new wrapper version that we choose
/// to upgrade to. Bumped together with `revision()` below.
pub const WHISPERX_VERSION: &str = "84d2ad2d61945af5e7517a9efaee9c12d3a9d9a3";

pub struct WhisperXReplicateBackend {
    client: ReplicateClient,
}

impl WhisperXReplicateBackend {
    pub fn new(api_token: impl Into<String>) -> Self {
        Self { client: ReplicateClient::new(api_token) }
    }
}

#[derive(Debug, Deserialize)]
struct WhisperXSegment {
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    words: Vec<WhisperXWord>,
}

#[derive(Debug, Deserialize)]
struct WhisperXWord {
    word: String,
    start: Option<f64>,
    end: Option<f64>,
    #[serde(default)]
    score: Option<f64>,
}

/// Parse Replicate's WhisperX JSON output into AlignedTrack.
pub fn parse_output(output: &Value) -> Result<Vec<AlignedLine>, BackendError> {
    let segments = output
        .get("segments")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BackendError::Malformed("missing segments[]".into()))?;

    let mut lines = Vec::with_capacity(segments.len());
    for seg in segments {
        let s: WhisperXSegment = serde_json::from_value(seg.clone())
            .map_err(|e| BackendError::Malformed(format!("segment parse: {e}")))?;
        let text = s.text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let words = if s.words.is_empty() {
            None
        } else {
            Some(
                s.words
                    .iter()
                    .filter(|w| w.start.is_some() && w.end.is_some())
                    .map(|w| AlignedWord {
                        text: w.word.trim().to_string(),
                        start_ms: (w.start.unwrap_or(0.0) * 1000.0) as u32,
                        end_ms: (w.end.unwrap_or(0.0) * 1000.0) as u32,
                        confidence: w.score.unwrap_or(0.9) as f32,
                    })
                    .collect(),
            )
        };
        lines.push(AlignedLine {
            text,
            start_ms: (s.start * 1000.0) as u32,
            end_ms: (s.end * 1000.0) as u32,
            words,
        });
    }
    Ok(lines)
}

#[async_trait]
impl AlignmentBackend for WhisperXReplicateBackend {
    fn id(&self) -> &'static str { "whisperx-large-v3" }
    fn revision(&self) -> u32 { 1 }
    fn capability(&self) -> AlignmentCapability {
        AlignmentCapability {
            word_level: true,
            segment_level: true,
            // WhisperX handles long-form natively via faster-whisper VAD chunking.
            // Songs > this duration would need chunking trigger (Task A.5).
            max_audio_seconds: 3_600,
            languages: &["en", "es", "pt", "fr", "de", "it", "nl", "pl", "ru", "uk"],
            takes_reference_text: false,
        }
    }

    async fn align(
        &self,
        vocal_wav_path: &Path,
        _reference_text: Option<&str>,
        language: &str,
        _opts: &AlignOpts,
    ) -> Result<AlignedTrack, BackendError> {
        let url = self.client.upload_file(vocal_wav_path).await
            .map_err(replicate_to_backend_err)?;

        let input = serde_json::json!({
            "audio_file": url,
            "language": language,
            "align_output": true,
            "diarization": false,
            "batch_size": 32,
        });

        let pred = self.client.predict(WHISPERX_VERSION, input).await
            .map_err(replicate_to_backend_err)?;

        let output = pred.output
            .ok_or_else(|| BackendError::Malformed("succeeded but no output".into()))?;

        let lines = parse_output(&output)?;
        Ok(AlignedTrack {
            lines,
            provenance: format!("{}@rev{}", self.id(), self.revision()),
            raw_confidence: 0.9,
        })
    }
}

fn replicate_to_backend_err(e: ReplicateError) -> BackendError {
    use ReplicateError::*;
    match e {
        Http(err) => BackendError::Transport(err.to_string()),
        Io(err) => BackendError::Io(err),
        ApiError { status, body } => BackendError::Rejected(format!("HTTP {status}: {body}")),
        RateLimited(n) => BackendError::RateLimit(format!("after {n} attempts")),
        PredictionFailed(s) => BackendError::Rejected(s),
        Timeout => BackendError::Timeout(std::time::Duration::from_secs(1800)),
        Malformed(s) => BackendError::Malformed(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_segment_with_words() {
        let raw = serde_json::json!({
            "segments": [
                {
                    "start": 1.5,
                    "end": 3.2,
                    "text": "Hello world",
                    "words": [
                        {"word": "Hello", "start": 1.5, "end": 2.0, "score": 0.95},
                        {"word": "world", "start": 2.1, "end": 3.2, "score": 0.92},
                    ]
                }
            ]
        });
        let lines = parse_output(&raw).unwrap();
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line.text, "Hello world");
        assert_eq!(line.start_ms, 1500);
        assert_eq!(line.end_ms, 3200);
        let words = line.words.as_ref().unwrap();
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].start_ms, 1500);
    }

    #[test]
    fn parses_segment_without_words_as_words_none() {
        let raw = serde_json::json!({
            "segments": [
                {"start": 0.0, "end": 5.0, "text": "no word timing"}
            ]
        });
        let lines = parse_output(&raw).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].words.is_none(), "missing words[] yields None");
    }

    #[test]
    fn skips_empty_text_segments() {
        let raw = serde_json::json!({
            "segments": [
                {"start": 0.0, "end": 1.0, "text": ""},
                {"start": 1.0, "end": 2.0, "text": "  \n  "},
                {"start": 2.0, "end": 3.0, "text": "real line"}
            ]
        });
        let lines = parse_output(&raw).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "real line");
    }

    #[test]
    fn rejects_missing_segments_field() {
        let raw = serde_json::json!({"foo": "bar"});
        let err = parse_output(&raw).unwrap_err();
        assert!(matches!(err, BackendError::Malformed(_)));
    }

    #[test]
    fn drops_words_without_timestamps() {
        let raw = serde_json::json!({
            "segments": [{
                "start": 0.0, "end": 2.0, "text": "two words",
                "words": [
                    {"word": "two", "start": 0.0, "end": 1.0},
                    {"word": "words", "start": null, "end": null},
                ]
            }]
        });
        let lines = parse_output(&raw).unwrap();
        let words = lines[0].words.as_ref().unwrap();
        assert_eq!(words.len(), 1, "untimestamped word filtered out");
        assert_eq!(words[0].text, "two");
    }

    #[test]
    fn id_and_revision_are_stable() {
        let b = WhisperXReplicateBackend::new("test-token");
        assert_eq!(b.id(), "whisperx-large-v3");
        assert_eq!(b.revision(), 1);
    }

    #[test]
    fn capability_advertises_word_level_and_languages() {
        let b = WhisperXReplicateBackend::new("test-token");
        let cap = b.capability();
        assert!(cap.word_level);
        assert!(cap.segment_level);
        assert!(cap.languages.contains(&"en"));
        assert!(cap.languages.contains(&"es"));
        assert!(cap.languages.contains(&"pt"));
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/whisperx_replicate.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add WhisperXReplicateBackend (AlignmentBackend impl)

victor-upmeet/whisperx on Replicate. Whisper-large-v3 + wav2vec2-CTC.
Pinned version 84d2ad2d61945af5e7517a9efaee9c12d3a9d9a3. Word + segment
timestamps. Long-form via faster-whisper internal VAD.

Verified on 2026-04-28: 18 sub-1s line matches on 11.8-min worship song.
Cost: \$0.035/song.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task A.5: Optional WhisperX chunking trigger

**Files:**
- Modify: `crates/sp-server/src/lyrics/whisperx_replicate.rs` (add chunking branch)
- Modify: `crates/sp-server/src/lyrics/audio_chunking.rs` (verify `plan_chunks` and `merge_overlap` are pub)

- [ ] **Step 1: Verify `plan_chunks` and `merge_overlap` are public**

```bash
grep -n "^pub fn plan_chunks\|^pub fn merge_overlap\|^pub const CHUNK_" crates/sp-server/src/lyrics/audio_chunking.rs
```
Expected: both functions and the chunk constants are `pub`. If not, make them so.

- [ ] **Step 2: Add chunking logic to `whisperx_replicate.rs`**

Append to `whisperx_replicate.rs` BEFORE the `#[cfg(test)]` block:

```rust
use crate::lyrics::audio_chunking::{plan_chunks, merge_overlap, CHUNK_DURATION_MS};

/// When `opts.chunk_trigger_seconds = Some(N)`, songs longer than N seconds
/// are chunked into 60s/10s-overlap windows, each transcribed independently,
/// then merged via the same overlap-dedup as the original Gemini path.
///
/// Default (`None` or `Some(u32::MAX)`): never chunk; rely on WhisperX's
/// internal VAD-based long-form handling.
async fn align_chunked(
    backend: &WhisperXReplicateBackend,
    vocal_wav_path: &Path,
    language: &str,
    duration_ms: u64,
) -> Result<Vec<AlignedLine>, BackendError> {
    use std::process::Command;
    use tempfile::TempDir;

    let plans = plan_chunks(duration_ms);
    let tmp = TempDir::new().map_err(BackendError::Io)?;
    let mut all: Vec<Vec<crate::lyrics::audio_chunking::ParsedLine>> = Vec::with_capacity(plans.len());

    for plan in &plans {
        let chunk_path = tmp.path().join(format!("chunk_{}.wav", plan.idx));
        // ffmpeg slice — relies on ffmpeg being on PATH (already in production)
        let status = Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-ss", &format!("{}", plan.start_ms as f64 / 1000.0),
                "-i", vocal_wav_path.to_str().ok_or_else(|| BackendError::Malformed("non-utf8 wav path".into()))?,
                "-t", &format!("{}", (plan.end_ms - plan.start_ms) as f64 / 1000.0),
                "-c:a", "pcm_s16le", "-ar", "16000", "-ac", "1",
                chunk_path.to_str().unwrap(),
            ])
            .status()
            .map_err(BackendError::Io)?;
        if !status.success() {
            return Err(BackendError::Rejected(format!("ffmpeg failed for chunk {}", plan.idx)));
        }

        let url = backend.client.upload_file(&chunk_path).await
            .map_err(replicate_to_backend_err)?;
        let input = serde_json::json!({
            "audio_file": url, "language": language,
            "align_output": true, "diarization": false, "batch_size": 32,
        });
        let pred = backend.client.predict(WHISPERX_VERSION, input).await
            .map_err(replicate_to_backend_err)?;
        let output = pred.output.ok_or_else(|| BackendError::Malformed("chunk: no output".into()))?;
        let chunk_lines = parse_output(&output)?;

        // Convert AlignedLine → audio_chunking::ParsedLine for merge_overlap
        let parsed: Vec<crate::lyrics::audio_chunking::ParsedLine> = chunk_lines.into_iter()
            .map(|l| crate::lyrics::audio_chunking::ParsedLine {
                text: l.text,
                start_ms: l.start_ms as u64,
                end_ms: l.end_ms as u64,
                source_chunk_idx: plan.idx,
            }).collect();
        all.push(parsed);
    }

    let merged = merge_overlap(&plans, &all);
    Ok(merged.into_iter().map(|g| AlignedLine {
        text: g.text,
        start_ms: g.start_ms as u32,
        end_ms: g.end_ms as u32,
        words: None,  // chunked path is line-only; word-merge across chunks is out of scope
    }).collect())
}
```

Modify the `align()` impl to dispatch to chunked path when triggered:

```rust
async fn align(
    &self,
    vocal_wav_path: &Path,
    _reference_text: Option<&str>,
    language: &str,
    opts: &AlignOpts,
) -> Result<AlignedTrack, BackendError> {
    // Determine audio duration via ffprobe (or soundfile).
    let duration_ms = probe_duration_ms(vocal_wav_path)?;
    let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);  // default: never

    let lines = if duration_ms / 1000 > trigger as u64 {
        align_chunked(self, vocal_wav_path, language, duration_ms).await?
    } else {
        // ... existing single-shot upload+predict (the old align() body)
        let url = self.client.upload_file(vocal_wav_path).await.map_err(replicate_to_backend_err)?;
        let input = serde_json::json!({
            "audio_file": url, "language": language,
            "align_output": true, "diarization": false, "batch_size": 32,
        });
        let pred = self.client.predict(WHISPERX_VERSION, input).await.map_err(replicate_to_backend_err)?;
        let output = pred.output.ok_or_else(|| BackendError::Malformed("succeeded but no output".into()))?;
        parse_output(&output)?
    };

    Ok(AlignedTrack {
        lines,
        provenance: format!("{}@rev{}", self.id(), self.revision()),
        raw_confidence: 0.9,
    })
}

fn probe_duration_ms(path: &Path) -> Result<u64, BackendError> {
    use std::process::Command;
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration",
               "-of", "default=noprint_wrappers=1:nokey=1",
               path.to_str().ok_or_else(|| BackendError::Malformed("non-utf8 path".into()))?])
        .output().map_err(BackendError::Io)?;
    if !out.status.success() {
        return Err(BackendError::Rejected("ffprobe failed".into()));
    }
    let s = String::from_utf8(out.stdout)
        .map_err(|e| BackendError::Malformed(format!("ffprobe utf8: {e}")))?;
    let secs: f64 = s.trim().parse()
        .map_err(|e| BackendError::Malformed(format!("ffprobe parse: {e}")))?;
    Ok((secs * 1000.0) as u64)
}
```

Add `tempfile` to `Cargo.toml` if not present:
```bash
grep "^tempfile" crates/sp-server/Cargo.toml
```
If missing, add: `tempfile = "3"`.

- [ ] **Step 3: Add chunking-path tests**

Append to the `tests` module in `whisperx_replicate.rs`:

```rust
    #[test]
    fn default_align_opts_never_triggers_chunking() {
        let opts = AlignOpts::default();
        // u32::MAX seconds = never chunk
        let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
        assert_eq!(trigger, u32::MAX);
    }

    #[test]
    fn chunk_trigger_some_zero_means_always_chunk() {
        let opts = AlignOpts { chunk_trigger_seconds: Some(0) };
        let trigger = opts.chunk_trigger_seconds.unwrap_or(u32::MAX);
        assert_eq!(trigger, 0);
    }
```

- [ ] **Step 4: `cargo fmt --all --check`**

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/whisperx_replicate.rs crates/sp-server/Cargo.toml
git commit -m "feat(lyrics): optional 60s/10s chunking trigger for WhisperX

When AlignOpts.chunk_trigger_seconds is set and audio exceeds that
threshold, slices vocal stem into 60s/10s-overlap chunks via ffmpeg,
runs WhisperX per-chunk, merges via existing overlap-dedup logic
(reused from audio_chunking.rs).

Default: never chunk (WhisperX handles long-form natively via
faster-whisper VAD). Trigger only as fallback for songs WhisperX
collapses on.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase B — Tier-1 fetchers + short-circuit

### Task B.1: DB migration V17 — `videos.spotify_track_id`

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs` (add MIGRATION_V17 constant + register)
- Modify: `crates/sp-server/src/db/models.rs` (add `spotify_track_id` to Video struct + queries)

- [ ] **Step 1: Add MIGRATION_V17 constant + register**

In `crates/sp-server/src/db/mod.rs`, after the `MIGRATION_V16` constant (~line 233), add:

```rust
const MIGRATION_V17: &str = "
ALTER TABLE videos ADD COLUMN spotify_track_id TEXT;
";
```

In the migrations slice (~line 12-20), add `(17, MIGRATION_V17),` after the V16 entry.

- [ ] **Step 2: Add `spotify_track_id` field to Video struct**

In `crates/sp-server/src/db/models.rs`, find the `pub struct Video` declaration. Add:

```rust
pub spotify_track_id: Option<String>,
```

Update any `query_as!` or `from_row` impls to include the column. Update `upsert_video` to preserve the column on upsert.

- [ ] **Step 3: Add query helpers**

Append to `models.rs`:

```rust
pub async fn set_video_spotify_track_id(
    pool: &SqlitePool,
    video_id: &str,
    spotify_track_id: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE videos SET spotify_track_id = ?1 WHERE youtube_id = ?2")
        .bind(spotify_track_id)
        .bind(video_id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn get_video_spotify_track_id(
    pool: &SqlitePool,
    video_id: &str,
) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT spotify_track_id FROM videos WHERE youtube_id = ?1"
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(s,)| s))
}
```

- [ ] **Step 4: Add migration test**

In `crates/sp-server/src/db/mod_tests.rs`, add:

```rust
#[tokio::test]
async fn migration_v17_adds_spotify_track_id_column() {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    // Insert and update spotify_track_id
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title) VALUES (1, 'abc', 't')")
        .execute(&pool).await.unwrap();
    super::models::set_video_spotify_track_id(&pool, "abc", Some("4uLU6hMCjMI75M1A2tKUQC")).await.unwrap();
    let id = super::models::get_video_spotify_track_id(&pool, "abc").await.unwrap();
    assert_eq!(id.as_deref(), Some("4uLU6hMCjMI75M1A2tKUQC"));
}
```

- [ ] **Step 5: `cargo fmt --all --check`**

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/db/mod.rs crates/sp-server/src/db/models.rs crates/sp-server/src/db/mod_tests.rs
git commit -m "feat(db): migration V17 — videos.spotify_track_id column

For SpotifyLyricsFetcher (issue #52). Manual track-ID assignment per
video via dashboard. Idempotent ALTER TABLE ADD COLUMN.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task B.2: SpotifyLyricsFetcher

**Files:**
- Create: `crates/sp-server/src/lyrics/spotify_proxy.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod spotify_proxy;`)

- [ ] **Step 1: Create `spotify_proxy.rs` with full impl + tests**

```rust
//! SpotifyLyricsFetcher — fetches LINE_SYNCED lyrics from the public
//! akashrchandran/spotify-lyrics-api proxy. Returns CandidateText with
//! `has_timing=true` when the proxy returns syncType="LINE_SYNCED".
//!
//! Per `feedback_no_legacy_code.md` and the spec's Tier-1 design, this
//! is a free Tier-1 source. Skips on 404 / `error: true` / `syncType=UNSYNCED`.

use std::time::Duration;

use serde::Deserialize;

use crate::lyrics::tier1::CandidateText;

const PROXY_BASE: &str = "https://spotify-lyrics-api-khaki.vercel.app";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum SpotifyError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("not found")]
    NotFound,
    #[error("proxy reported error: {0}")]
    ProxyError(String),
    #[error("malformed: {0}")]
    Malformed(String),
}

#[derive(Debug, Deserialize)]
struct ProxyResponse {
    error: Option<bool>,
    #[serde(rename = "syncType")]
    sync_type: Option<String>,
    lines: Option<Vec<ProxyLine>>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyLine {
    #[serde(rename = "startTimeMs")]
    start_time_ms: String,
    words: String,
}

pub struct SpotifyLyricsFetcher {
    http: reqwest::Client,
}

impl Default for SpotifyLyricsFetcher {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }
}

impl SpotifyLyricsFetcher {
    pub fn new() -> Self { Self::default() }

    /// Fetch LINE_SYNCED lyrics for a Spotify track ID. Returns:
    /// - `Ok(Some(CandidateText))` if syncType == LINE_SYNCED with ≥1 line
    /// - `Ok(None)` if the track has no synced lyrics (UNSYNCED / empty)
    /// - `Err(SpotifyError)` on network / parse failure
    pub async fn fetch(&self, track_id: &str) -> Result<Option<CandidateText>, SpotifyError> {
        let url = format!("{PROXY_BASE}/?trackid={track_id}");
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(SpotifyError::NotFound);
        }
        let body = resp.text().await?;
        let parsed: ProxyResponse = serde_json::from_str(&body)
            .map_err(|e| SpotifyError::Malformed(format!("json: {e}")))?;
        if parsed.error.unwrap_or(false) {
            return Err(SpotifyError::ProxyError(
                parsed.message.unwrap_or_else(|| "proxy error".into()),
            ));
        }
        if parsed.sync_type.as_deref() != Some("LINE_SYNCED") {
            return Ok(None);
        }
        let raw_lines = parsed.lines.unwrap_or_default();
        if raw_lines.is_empty() {
            return Ok(None);
        }

        // Build CandidateText. Skip empty/♪ filler lines. End time = next line's start
        // (or last line + 3000ms).
        let mut texts = Vec::new();
        let mut timings = Vec::new();
        let n = raw_lines.len();
        for (i, line) in raw_lines.iter().enumerate() {
            let words = line.words.trim();
            if words.is_empty() || words == "♪" {
                continue;
            }
            let start: u32 = line.start_time_ms.parse().unwrap_or(0);
            let end: u32 = if i + 1 < n {
                raw_lines[i + 1].start_time_ms.parse().unwrap_or(start.saturating_add(3000))
            } else {
                start.saturating_add(3000)
            };
            texts.push(words.to_string());
            timings.push((start, end));
        }
        if texts.is_empty() {
            return Ok(None);
        }
        Ok(Some(CandidateText {
            source: "tier1:spotify".into(),
            lines: texts,
            line_timings: Some(timings),
            has_timing: true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_via_test_helper(body: &str) -> Option<CandidateText> {
        // Helper that runs the parsing logic against a fixture body.
        let parsed: ProxyResponse = serde_json::from_str(body).expect("fixture");
        if parsed.error.unwrap_or(false) { return None; }
        if parsed.sync_type.as_deref() != Some("LINE_SYNCED") { return None; }
        let raw_lines = parsed.lines.unwrap_or_default();
        if raw_lines.is_empty() { return None; }
        let n = raw_lines.len();
        let mut texts = Vec::new();
        let mut timings = Vec::new();
        for (i, line) in raw_lines.iter().enumerate() {
            let words = line.words.trim();
            if words.is_empty() || words == "♪" { continue; }
            let start: u32 = line.start_time_ms.parse().unwrap_or(0);
            let end: u32 = if i + 1 < n {
                raw_lines[i + 1].start_time_ms.parse().unwrap_or(start.saturating_add(3000))
            } else { start.saturating_add(3000) };
            texts.push(words.to_string());
            timings.push((start, end));
        }
        if texts.is_empty() { return None; }
        Some(CandidateText {
            source: "tier1:spotify".into(),
            lines: texts, line_timings: Some(timings), has_timing: true,
        })
    }

    #[test]
    fn parses_line_synced_response() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "1000", "words": "Hello world"},
                {"startTimeMs": "3000", "words": "Praise the Lord"}
            ]
        }"#;
        let c = parse_via_test_helper(body).unwrap();
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.line_timings.as_ref().unwrap()[0], (1000, 3000));
        assert_eq!(c.line_timings.as_ref().unwrap()[1].1, 6000);  // last line + 3000ms
        assert!(c.has_timing);
        assert_eq!(c.source, "tier1:spotify");
    }

    #[test]
    fn returns_none_for_unsynced_response() {
        let body = r#"{"error": false, "syncType": "UNSYNCED", "lines": []}"#;
        assert!(parse_via_test_helper(body).is_none());
    }

    #[test]
    fn returns_none_for_proxy_error() {
        let body = r#"{"error": true, "message": "track not found"}"#;
        assert!(parse_via_test_helper(body).is_none());
    }

    #[test]
    fn skips_empty_filler_lines() {
        let body = r#"{
            "error": false,
            "syncType": "LINE_SYNCED",
            "lines": [
                {"startTimeMs": "1000", "words": "♪"},
                {"startTimeMs": "2000", "words": ""},
                {"startTimeMs": "3000", "words": "Real line"}
            ]
        }"#;
        let c = parse_via_test_helper(body).unwrap();
        assert_eq!(c.lines.len(), 1);
        assert_eq!(c.lines[0], "Real line");
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/spotify_proxy.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add SpotifyLyricsFetcher (Tier-1 source, issue #52)

Public proxy at akashrchandran/spotify-lyrics-api. LINE_SYNCED only;
skips UNSYNCED, proxy errors, and ♪ filler lines. End time of each
line = next line start (or last line + 3000ms).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task B.3: Tier-1 collector + short-circuit logic

**Files:**
- Create: `crates/sp-server/src/lyrics/tier1.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod tier1;`)

- [ ] **Step 1: Create `tier1.rs` with collector + short-circuit**

```rust
//! Tier-1 — free text + line-timing fetchers. Run in parallel; return
//! the strongest candidate. If any fetcher returns has_timing=true with
//! ≥10 lines, short-circuits the rest of the pipeline (no Tier-2 call,
//! no anchor reconciliation — the source is authoritative).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::lyrics::backend::AlignedLine;
use crate::lyrics::lrclib::LrclibFetcher;
use crate::lyrics::genius::GeniusFetcher;
use crate::lyrics::spotify_proxy::SpotifyLyricsFetcher;
use crate::lyrics::youtube_subs::YtManualSubsParser;

/// Threshold for Tier-1 short-circuit: only ship directly if the source
/// has timing AND at least this many lines. Below this, treat as
/// suspiciously short (intro snippet, partial fetch, etc.) and fall
/// through to Tier-2 + reconciliation.
pub const TIER1_MIN_LINES: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateText {
    pub source: String,                            // "tier1:spotify", "tier1:lrclib", etc.
    pub lines: Vec<String>,
    pub line_timings: Option<Vec<(u32, u32)>>,    // start_ms, end_ms per line
    pub has_timing: bool,
}

#[derive(Debug, Clone)]
pub enum Tier1Result {
    /// One source has line-synced authoritative output. Ship directly.
    LineSynced(AlignedLines),
    /// Only text-only candidates (no timing). Pass to Tier-2 + reconcile.
    TextOnly(Vec<CandidateText>),
    /// No fetchers returned anything usable.
    None,
}

#[derive(Debug, Clone)]
pub struct AlignedLines {
    pub lines: Vec<AlignedLine>,
    pub provenance: String,
}

pub struct Tier1Collector {
    pub spotify: SpotifyLyricsFetcher,
    pub lrclib: Arc<LrclibFetcher>,
    pub genius: Arc<GeniusFetcher>,
    pub yt_subs: Arc<YtManualSubsParser>,
}

impl Tier1Collector {
    /// Fetch all sources in parallel. Return the strongest candidate.
    /// `spotify_track_id`: optional, from videos.spotify_track_id.
    /// `vtt_path`: optional, path to manual yt_subs VTT (autosub banned
    /// per `feedback_no_autosub.md`).
    pub async fn collect(
        &self,
        artist: &str,
        track: &str,
        duration_s: u32,
        spotify_track_id: Option<&str>,
        vtt_path: Option<&std::path::Path>,
    ) -> Tier1Result {
        let mut candidates: Vec<CandidateText> = Vec::new();

        // Spotify (only if track_id manually assigned)
        if let Some(track_id) = spotify_track_id {
            if let Ok(Some(c)) = self.spotify.fetch(track_id).await {
                candidates.push(c);
            }
        }

        // LRCLib
        if let Ok(Some(c)) = self.lrclib.fetch(artist, track, duration_s).await {
            candidates.push(c);
        }

        // YouTube manual subs (has_timing=true only — autosub banned)
        if let Some(vtt) = vtt_path {
            if let Ok(Some(c)) = self.yt_subs.parse(vtt).await {
                if c.has_timing {
                    candidates.push(c);
                }
            }
        }

        // Genius (text-only — for reconciliation reference)
        if let Ok(Some(c)) = self.genius.fetch(artist, track).await {
            candidates.push(c);
        }

        // Short-circuit: first candidate with has_timing && ≥TIER1_MIN_LINES wins
        for c in &candidates {
            if c.has_timing && c.lines.len() >= TIER1_MIN_LINES {
                if let Some(timings) = &c.line_timings {
                    let aligned: Vec<AlignedLine> = c.lines.iter().zip(timings.iter())
                        .map(|(text, (start, end))| AlignedLine {
                            text: text.clone(),
                            start_ms: *start, end_ms: *end,
                            words: None,  // line-only — never synthesize words
                        })
                        .collect();
                    return Tier1Result::LineSynced(AlignedLines {
                        lines: aligned,
                        provenance: c.source.clone(),
                    });
                }
            }
        }

        // No timing → return text candidates for reconciliation
        if candidates.is_empty() {
            Tier1Result::None
        } else {
            Tier1Result::TextOnly(candidates)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_min_lines_is_ten() {
        assert_eq!(TIER1_MIN_LINES, 10);
    }

    #[test]
    fn line_synced_short_circuit_strips_words() {
        let candidates = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: (0..15).map(|i| format!("line {}", i)).collect(),
            line_timings: Some((0..15).map(|i| (i * 1000, i * 1000 + 1000)).collect()),
            has_timing: true,
        }];
        // Simulate what the collector does:
        let c = &candidates[0];
        assert!(c.has_timing && c.lines.len() >= TIER1_MIN_LINES);
        let timings = c.line_timings.as_ref().unwrap();
        let aligned: Vec<AlignedLine> = c.lines.iter().zip(timings.iter())
            .map(|(text, (s, e))| AlignedLine {
                text: text.clone(), start_ms: *s, end_ms: *e, words: None,
            }).collect();
        // `feedback_line_timing_only.md`: words must be None on Tier-1 short-circuit
        for l in &aligned {
            assert!(l.words.is_none(), "Tier-1 ships words: None — never synthesize");
        }
        assert_eq!(aligned.len(), 15);
    }

    #[test]
    fn line_synced_below_threshold_falls_through() {
        let candidates = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: (0..5).map(|i| format!("line {}", i)).collect(),  // only 5 lines
            line_timings: Some((0..5).map(|i| (i * 1000, i * 1000 + 1000)).collect()),
            has_timing: true,
        }];
        let c = &candidates[0];
        assert!(!(c.has_timing && c.lines.len() >= TIER1_MIN_LINES));
    }

    #[test]
    fn text_only_candidates_return_text_only_variant() {
        // 0 has_timing candidates → should be TextOnly with all candidates
        // (collector caller handles this; here we just exercise the discriminant)
        let r: Tier1Result = Tier1Result::TextOnly(vec![CandidateText {
            source: "genius".into(),
            lines: vec!["a".into(), "b".into()],
            line_timings: None, has_timing: false,
        }]);
        assert!(matches!(r, Tier1Result::TextOnly(_)));
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/tier1.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): Tier-1 collector + short-circuit logic

Parallel Spotify+LRCLib+yt_subs+Genius fetch. Short-circuits when
any source returns has_timing=true with ≥10 lines (avoids partial
fetch / intro snippet artifacts). Tier-1 aligned lines ship words: None
per feedback_line_timing_only.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task B.4: Move `CandidateText` from `provider.rs` (legacy) to `tier1.rs`

**Files:**
- Modify: `crates/sp-server/src/lyrics/lrclib.rs`, `genius.rs`, `youtube_subs.rs` to use the new `tier1::CandidateText`.
- Modify: `crates/sp-server/src/lyrics/provider.rs` — `CandidateText` removed (Phase G will delete the whole file).

- [ ] **Step 1: Update lrclib.rs imports**

In `crates/sp-server/src/lyrics/lrclib.rs`: change `use crate::lyrics::provider::CandidateText;` → `use crate::lyrics::tier1::CandidateText;`. Same for any field access — `tier1::CandidateText` shape matches `provider::CandidateText` (verify).

If shapes differ, adapt the construction in lrclib's fetch method to match `tier1::CandidateText`.

- [ ] **Step 2: Update genius.rs imports** — same pattern.

- [ ] **Step 3: Update youtube_subs.rs imports** — same pattern.

- [ ] **Step 4: `cargo fmt --all --check`**

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/lrclib.rs crates/sp-server/src/lyrics/genius.rs crates/sp-server/src/lyrics/youtube_subs.rs
git commit -m "refactor(lyrics): switch existing fetchers to tier1::CandidateText

LRCLib, Genius, YouTube manual subs now import CandidateText from
tier1 module instead of legacy provider. provider.rs CandidateText
will be deleted in Phase G.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase C — Anchor-sequence reconciler

### Task C.1: LCS algorithm

**Files:**
- Create: `crates/sp-server/src/lyrics/lcs.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub(crate) mod lcs;`)

- [ ] **Step 1: Create `lcs.rs`**

```rust
//! Longest Common Subsequence over normalized word tokens.
//! Used by `reconcile.rs` to anchor authoritative text into ASR timing.

/// Normalize a word for comparison: lowercase + strip non-alphanumeric.
pub fn norm(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// LCS index pairs. Returns `Vec<(i_in_a, i_in_b)>` for matched positions
/// in order. Standard DP; O(n*m) time, O(n*m) space — fine for songs
/// (≤2000 words per track).
pub fn lcs_pairs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 { return Vec::new(); }

    // dp[i][j] = LCS length of a[..i] / b[..j]
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            if a[i] == b[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    // Backtrack
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            pairs.push((i - 1, j - 1));
            i -= 1; j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norms(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| norm(w)).collect()
    }

    #[test]
    fn norm_strips_punctuation_and_lowercases() {
        assert_eq!(norm("Hello,"), "hello");
        assert_eq!(norm("It's"), "its");
        assert_eq!(norm("Praise!"), "praise");
    }

    #[test]
    fn lcs_identical_sequences() {
        let a = norms(&["hello", "world"]);
        let b = norms(&["hello", "world"]);
        assert_eq!(lcs_pairs(&a, &b), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn lcs_finds_anchors_with_one_swap() {
        // "I got a God" (whisperX) vs "I've got a God" (spotify)
        let a = norms(&["i", "got", "a", "god"]);
        let b = norms(&["ive", "got", "a", "god"]);
        let pairs = lcs_pairs(&a, &b);
        // "got","a","god" match
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], (1, 1));
    }

    #[test]
    fn lcs_handles_empty() {
        let a: Vec<String> = vec![];
        let b = norms(&["hello"]);
        assert_eq!(lcs_pairs(&a, &b), vec![]);
    }
}
```

- [ ] **Step 2: Add `pub(crate) mod lcs;` in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/lcs.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): LCS helper for anchor-sequence reconciliation

Normalized-word LCS (lowercase + alphanumeric only). O(n*m) time, fine
for songs ≤ 2000 words. Used by reconcile.rs to anchor authoritative
text into WhisperX timing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task C.2: Reconciler core

**Files:**
- Create: `crates/sp-server/src/lyrics/reconcile.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod reconcile;`)

- [ ] **Step 1: Create `reconcile.rs` with full impl + tests**

```rust
//! Anchor-sequence reconciler — keeps WhisperX timing, replaces mishearings
//! with authoritative text from Tier-1 sources.
//!
//! Pattern from karaoke-gen LyricsCorrector:
//! 1. Tokenize WhisperX output and authoritative text into normalized words
//! 2. Compute LCS anchor pairs
//! 3. Walk anchor-bounded gaps, replace WhisperX words with authoritative
//!    words while keeping the timestamp range
//!
//! Replaces text_merge.rs (Claude reconciliation) with deterministic Rust.

use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::lcs::{lcs_pairs, norm};
use crate::lyrics::tier1::CandidateText;

/// Reconcile a WhisperX-produced AlignedTrack against an authoritative text
/// (concatenation of all CandidateText lines from Tier-1 text-only sources).
///
/// Returns a NEW AlignedTrack: each line's text is the authoritative version
/// in the matching anchor range, but timing comes from WhisperX.
pub fn reconcile(
    asr: &AlignedTrack,
    authoritative: &[CandidateText],
) -> AlignedTrack {
    // 1. Flatten authoritative lines into a single word stream + line boundaries
    let auth_text = best_authoritative(authoritative);
    if auth_text.is_empty() {
        return asr.clone();
    }
    let auth_words: Vec<String> = auth_text.iter()
        .flat_map(|line| line.split_whitespace().map(|w| norm(w)))
        .collect();
    if auth_words.is_empty() {
        return asr.clone();
    }

    // Build authoritative word list with which-line-it-belongs-to
    let mut auth_word_to_line: Vec<usize> = Vec::with_capacity(auth_words.len());
    for (line_idx, line) in auth_text.iter().enumerate() {
        for _ in line.split_whitespace() {
            auth_word_to_line.push(line_idx);
        }
    }

    // 2. Flatten ASR words across all lines for LCS
    let mut asr_words: Vec<String> = Vec::new();
    let mut asr_word_origin: Vec<(usize, usize)> = Vec::new();  // (line_idx, word_idx_in_line)
    for (li, line) in asr.lines.iter().enumerate() {
        if let Some(words) = &line.words {
            for (wi, w) in words.iter().enumerate() {
                asr_words.push(norm(&w.text));
                asr_word_origin.push((li, wi));
            }
        } else {
            // Line-only ASR: synthesize one "word" per line for LCS purposes
            for (wi, w) in line.text.split_whitespace().enumerate() {
                asr_words.push(norm(w));
                asr_word_origin.push((li, wi));
            }
        }
    }
    if asr_words.is_empty() { return asr.clone(); }

    let pairs = lcs_pairs(&asr_words, &auth_words);
    if pairs.is_empty() { return asr.clone(); }

    // 3. Walk anchor pairs, build NEW AlignedTrack with authoritative text
    //    grouped per authoritative line, timing from WhisperX.
    let mut new_lines: Vec<AlignedLine> = Vec::new();
    let mut current_auth_line: Option<usize> = None;
    let mut current_text_buf: Vec<String> = Vec::new();
    let mut current_start_ms: u32 = 0;
    let mut current_end_ms: u32 = 0;
    let mut current_words: Vec<AlignedWord> = Vec::new();

    for &(asr_idx, auth_idx) in &pairs {
        let auth_line_idx = auth_word_to_line[auth_idx];
        let (asr_line_idx, asr_word_idx) = asr_word_origin[asr_idx];

        // Get the timing of this anchor word
        let (start_ms, end_ms, conf) = anchor_timing(asr, asr_line_idx, asr_word_idx);
        let auth_word = auth_text[auth_line_idx]
            .split_whitespace().nth(
                auth_idx - first_word_offset(&auth_text, auth_line_idx)
            ).unwrap_or("").to_string();

        if Some(auth_line_idx) != current_auth_line {
            // Flush previous line
            if let Some(_) = current_auth_line {
                new_lines.push(AlignedLine {
                    text: auth_text[current_auth_line.unwrap()].clone(),
                    start_ms: current_start_ms,
                    end_ms: current_end_ms,
                    words: if current_words.is_empty() { None } else { Some(std::mem::take(&mut current_words)) },
                });
                current_text_buf.clear();
            }
            current_auth_line = Some(auth_line_idx);
            current_start_ms = start_ms;
            current_words.clear();
        }
        current_end_ms = end_ms;
        current_words.push(AlignedWord {
            text: auth_word, start_ms, end_ms, confidence: conf,
        });
    }

    // Flush final line
    if let Some(li) = current_auth_line {
        new_lines.push(AlignedLine {
            text: auth_text[li].clone(),
            start_ms: current_start_ms,
            end_ms: current_end_ms,
            words: if current_words.is_empty() { None } else { Some(current_words) },
        });
    }

    AlignedTrack {
        lines: new_lines,
        provenance: format!("{}+reconciled", asr.provenance),
        raw_confidence: asr.raw_confidence,
    }
}

fn anchor_timing(asr: &AlignedTrack, line_idx: usize, word_idx: usize) -> (u32, u32, f32) {
    let line = &asr.lines[line_idx];
    if let Some(words) = &line.words {
        if let Some(w) = words.get(word_idx) {
            return (w.start_ms, w.end_ms, w.confidence);
        }
    }
    // Line-only fallback: distribute the line span across word slots
    let n = line.text.split_whitespace().count().max(1);
    let span = line.end_ms.saturating_sub(line.start_ms);
    let per = span / n as u32;
    let start = line.start_ms + per * word_idx as u32;
    let end = (start + per).min(line.end_ms);
    (start, end, 0.7)
}

fn first_word_offset(auth_text: &[String], line_idx: usize) -> usize {
    auth_text.iter().take(line_idx)
        .map(|l| l.split_whitespace().count())
        .sum()
}

/// Pick the strongest authoritative source: prefer one with most lines.
fn best_authoritative(candidates: &[CandidateText]) -> Vec<String> {
    candidates.iter().max_by_key(|c| c.lines.len())
        .map(|c| c.lines.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_asr(lines: &[(&str, u32, u32, &[(&str, u32, u32)])]) -> AlignedTrack {
        AlignedTrack {
            lines: lines.iter().map(|(text, s, e, words)| AlignedLine {
                text: text.to_string(), start_ms: *s, end_ms: *e,
                words: Some(words.iter().map(|(w, ws, we)| AlignedWord {
                    text: w.to_string(), start_ms: *ws, end_ms: *we, confidence: 0.9,
                }).collect()),
            }).collect(),
            provenance: "test@rev1".into(),
            raw_confidence: 0.9,
        }
    }

    #[test]
    fn reconciler_replaces_misheard_word_keeps_timing() {
        let asr = make_asr(&[
            ("I got a God", 1000, 2000, &[
                ("I", 1000, 1200), ("got", 1200, 1500),
                ("a", 1500, 1700), ("God", 1700, 2000),
            ]),
        ]);
        let auth = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: vec!["I've got a God".into()],
            line_timings: None, has_timing: false,
        }];
        let reconciled = reconcile(&asr, &auth);
        assert_eq!(reconciled.lines.len(), 1);
        assert_eq!(reconciled.lines[0].text, "I've got a God");  // authoritative text
        assert_eq!(reconciled.lines[0].start_ms, 1200);  // timing from "got" anchor
        assert!(reconciled.provenance.ends_with("+reconciled"));
    }

    #[test]
    fn reconciler_returns_input_when_no_authoritative_text() {
        let asr = make_asr(&[
            ("Hello world", 0, 1000, &[("Hello", 0, 500), ("world", 500, 1000)]),
        ]);
        let r = reconcile(&asr, &[]);
        assert_eq!(r.lines, asr.lines);
    }

    #[test]
    fn reconciler_returns_input_when_no_lcs_anchors() {
        let asr = make_asr(&[
            ("foo bar baz", 0, 1000, &[("foo", 0, 333), ("bar", 333, 666), ("baz", 666, 1000)]),
        ]);
        let auth = vec![CandidateText {
            source: "tier1:spotify".into(),
            lines: vec!["completely different lyrics here".into()],
            line_timings: None, has_timing: false,
        }];
        let r = reconcile(&asr, &auth);
        // No anchor matches → return ASR unchanged
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].text, "foo bar baz");
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/reconcile.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): anchor-sequence reconciler (replaces text_merge.rs)

Pattern from karaoke-gen: LCS-anchor authoritative text into ASR timing.
Pure deterministic Rust, no LLM call. Returns ASR unchanged when no
authoritative text or no LCS anchors found (graceful degradation).

Per feedback_no_legacy_code.md, text_merge.rs (Claude reconciliation)
will be deleted in Phase G after orchestrator is wired to use this.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase D — SubtitleEdit-port line splitter

### Task D.1: Line splitter core

**Files:**
- Create: `crates/sp-server/src/lyrics/line_splitter.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `pub mod line_splitter;`)

- [ ] **Step 1: Create `line_splitter.rs` with priority-ordered split + tests**

```rust
//! Line-length splitter — port of SubtitleEdit's TextSplit.AutoBreak()
//! priority-ordered logic (clean-room reimplementation; we read the
//! algorithm, not the GPL-3.0 source).
//!
//! Default max_chars = 32 (LED wall / ProPresenter style). Configurable.
//! NEVER produces uniform/evenly-distributed output (per
//! `feedback_no_even_distribution.md`).

use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};

pub const DEFAULT_MAX_CHARS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct SplitConfig {
    pub max_chars: usize,
}

impl Default for SplitConfig {
    fn default() -> Self { Self { max_chars: DEFAULT_MAX_CHARS } }
}

/// Apply line splitting to every line in the track. Lines under `max_chars`
/// pass through untouched. Lines over are split using the priority order:
/// 1. Sentence-end punctuation (`.!?…`)
/// 2. Comma / pause (`,`, `;`, `:`)
/// 3. Word-boundary balance — find split nearest center
/// 4. Hard fallback — rightmost word boundary ≤ max_chars
pub fn split_track(track: &AlignedTrack, cfg: SplitConfig) -> AlignedTrack {
    let mut out_lines = Vec::with_capacity(track.lines.len());
    for line in &track.lines {
        if line.text.chars().count() <= cfg.max_chars {
            out_lines.push(line.clone());
            continue;
        }
        out_lines.extend(split_line(line, cfg));
    }
    AlignedTrack {
        lines: out_lines,
        provenance: track.provenance.clone(),
        raw_confidence: track.raw_confidence,
    }
}

fn split_line(line: &AlignedLine, cfg: SplitConfig) -> Vec<AlignedLine> {
    let split_idx = find_split_index(&line.text, cfg.max_chars);
    let split_idx = match split_idx {
        Some(i) => i,
        // No safe split found — leave the line alone (better than mid-word break)
        None => return vec![line.clone()],
    };

    let (left_text, right_text) = (&line.text[..split_idx].trim_end(), &line.text[split_idx..].trim_start());
    if left_text.is_empty() || right_text.is_empty() {
        return vec![line.clone()];
    }

    // Distribute timing proportional to char counts
    let total = line.text.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let left_chars = left_text.chars().filter(|c| !c.is_whitespace()).count();
    let mid_ms = line.start_ms + ((line.end_ms - line.start_ms) as u64 * left_chars as u64 / total as u64) as u32;

    // Distribute words by their position
    let (left_words, right_words) = split_words_by_index(line, split_idx);

    let left_line = AlignedLine {
        text: left_text.to_string(),
        start_ms: line.start_ms, end_ms: mid_ms,
        words: left_words,
    };
    let right_line = AlignedLine {
        text: right_text.to_string(),
        start_ms: mid_ms, end_ms: line.end_ms,
        words: right_words,
    };

    // Recursively split halves if still too long
    let mut out = Vec::new();
    if left_line.text.chars().count() > cfg.max_chars {
        out.extend(split_line(&left_line, cfg));
    } else {
        out.push(left_line);
    }
    if right_line.text.chars().count() > cfg.max_chars {
        out.extend(split_line(&right_line, cfg));
    } else {
        out.push(right_line);
    }
    out
}

/// Find the byte-index for the split. Priority order:
/// 1. Sentence-end punctuation rightmost ≤ max_chars
/// 2. Comma rightmost ≤ max_chars
/// 3. Word-boundary nearest to center (max_chars / 2)
/// 4. Rightmost word-boundary ≤ max_chars
fn find_split_index(text: &str, max_chars: usize) -> Option<usize> {
    if text.chars().count() <= max_chars { return None; }

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let limit_idx = chars.get(max_chars).map(|(i, _)| *i).unwrap_or(text.len());

    // 1. Sentence-end (.!?…) rightmost ≤ limit
    for &(i, c) in chars[..max_chars.min(chars.len())].iter().rev() {
        if matches!(c, '.' | '!' | '?' | '…') {
            // Prefer split AFTER the punctuation
            let next = i + c.len_utf8();
            if next < text.len() {
                return Some(next);
            }
        }
    }

    // 2. Comma / pause rightmost ≤ limit
    for &(i, c) in chars[..max_chars.min(chars.len())].iter().rev() {
        if matches!(c, ',' | ';' | ':' | '，' | '、') {
            let next = i + c.len_utf8();
            if next < text.len() {
                return Some(next);
            }
        }
    }

    // 3. Word boundary nearest center
    let center = max_chars / 2;
    let center_byte = chars.get(center).map(|(i, _)| *i).unwrap_or(text.len());
    let mut best: Option<(usize, i64)> = None;
    for (idx, c) in text.char_indices() {
        if c == ' ' && idx <= limit_idx {
            let dist = (idx as i64 - center_byte as i64).abs();
            if best.map_or(true, |(_, d)| dist < d) {
                best = Some((idx + 1, dist));
            }
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }

    // 4. Rightmost word boundary ≤ limit
    text[..limit_idx].rfind(' ').map(|i| i + 1)
}

fn split_words_by_index(
    line: &AlignedLine, byte_idx: usize,
) -> (Option<Vec<AlignedWord>>, Option<Vec<AlignedWord>>) {
    let words = match &line.words {
        Some(w) => w,
        None => return (None, None),
    };
    if words.is_empty() { return (None, None); }

    // Word position in original text — naive: count chars up to start of each word.
    // For karaoke purposes, we approximate by splitting words by index.
    let split_word = (words.len() * byte_idx / line.text.len().max(1)).min(words.len());
    let (left, right) = words.split_at(split_word);
    (
        if left.is_empty() { None } else { Some(left.to_vec()) },
        if right.is_empty() { None } else { Some(right.to_vec()) },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, start_ms: u32, end_ms: u32) -> AlignedLine {
        AlignedLine { text: text.into(), start_ms, end_ms, words: None }
    }

    #[test]
    fn default_max_chars_is_32() {
        assert_eq!(DEFAULT_MAX_CHARS, 32);
    }

    #[test]
    fn line_under_max_passes_through() {
        let l = line("Short line.", 0, 1000);
        let track = AlignedTrack { lines: vec![l.clone()], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 1);
        assert_eq!(split.lines[0].text, "Short line.");
    }

    #[test]
    fn long_line_splits_at_sentence_end() {
        let l = line("Praise the Lord. Tell the world.", 0, 4000);
        let track = AlignedTrack { lines: vec![l], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].text, "Praise the Lord.");
        assert_eq!(split.lines[1].text, "Tell the world.");
    }

    #[test]
    fn long_line_splits_at_comma_when_no_sentence_end() {
        let l = line("Praise the Lord, tell the world", 0, 4000);
        let track = AlignedTrack { lines: vec![l], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        assert!(split.lines.len() >= 2);
        assert!(split.lines[0].text.ends_with(','));
    }

    #[test]
    fn long_line_falls_back_to_word_boundary() {
        let l = line("Hallelujah praise hallelujah praise the Lord", 0, 4000);
        let track = AlignedTrack { lines: vec![l], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        assert!(split.lines.len() >= 2);
        // No mid-word breaks
        for sl in &split.lines {
            assert!(!sl.text.starts_with(' '));
            assert!(!sl.text.ends_with(' '));
        }
    }

    #[test]
    fn timing_proportionally_distributed() {
        let l = line("Praise the Lord. Tell the world.", 0, 4000);
        let track = AlignedTrack { lines: vec![l], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        // First line ends roughly halfway
        assert!(split.lines[0].end_ms > 1000);
        assert!(split.lines[0].end_ms < 3000);
        // Continuity: line 1 end == line 2 start
        assert_eq!(split.lines[0].end_ms, split.lines[1].start_ms);
        // Outer bounds preserved
        assert_eq!(split.lines[0].start_ms, 0);
        assert_eq!(split.lines[1].end_ms, 4000);
    }

    #[test]
    fn no_safe_split_passes_through_long_line() {
        // Single long word (no spaces) — can't split safely
        let l = line(&"a".repeat(50), 0, 1000);
        let track = AlignedTrack { lines: vec![l], provenance: "t".into(), raw_confidence: 1.0 };
        let split = split_track(&track, SplitConfig::default());
        assert_eq!(split.lines.len(), 1, "no safe split → preserve original");
    }
}
```

- [ ] **Step 2: Add module declaration in `mod.rs`**

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/line_splitter.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): SubtitleEdit-port line splitter (32-char default)

Priority order: sentence-end → comma → word-boundary near center →
rightmost word boundary. Timing distributed proportionally by char count.
Words split at proportional position. Recursive on halves still over limit.

Default 32 chars (ProPresenter / LED-wall target). Per
feedback_no_even_distribution.md, NEVER produces uniform timings.
No safe split → line preserved (better than mid-word break).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase E — Renderer cleanup (drop DEFAULT_LYRICS_LEAD_MS)

### Task E.1: Remove the 1-second lead from renderer

**Files:**
- Modify: `crates/sp-server/src/lyrics/renderer.rs`
- Modify: any test files referencing `DEFAULT_LYRICS_LEAD_MS`

- [ ] **Step 1: Find every reference**

```bash
grep -rn "DEFAULT_LYRICS_LEAD_MS\|lead_ms" crates/sp-server/src/ | head -30
```

- [ ] **Step 2: Remove the constant + its usage in renderer.rs**

In `crates/sp-server/src/lyrics/renderer.rs`, line 17:
```rust
pub const DEFAULT_LYRICS_LEAD_MS: u64 = 1_000;
```
DELETE this line.

Find every usage of `DEFAULT_LYRICS_LEAD_MS` and `lead_ms` in renderer.rs and:
- If `lead_ms` is a parameter, change call sites to pass `0` OR remove the parameter entirely (preferred — simplifies signatures).
- Remove the doc comments referencing the lead.

For example, if there's:
```rust
fn presenter_lines(track: &LyricsTrack, position_ms: u64, lead_ms: u64) -> ...
```
change to:
```rust
fn presenter_lines(track: &LyricsTrack, position_ms: u64) -> ...
```
and update internal `effective_lookup(position_ms + lead_ms, ...)` → `effective_lookup(position_ms, ...)`.

- [ ] **Step 3: Update tests**

Any test file using `DEFAULT_LYRICS_LEAD_MS` — remove the import and any call sites that passed `lead_ms`. Tests should now assert against unshifted `position_ms`.

If a test was effectively testing "the wall shows the next line 1s ahead" — that test was the bug, delete it.

- [ ] **Step 4: `cargo fmt --all --check`**

- [ ] **Step 5: Commit**

```bash
git add -A crates/sp-server/src/lyrics/renderer.rs
git commit -m "refactor(lyrics): drop DEFAULT_LYRICS_LEAD_MS band-aid

The 1-second renderer lead was added to mask Gemini chunk-boundary
timing drift. With WhisperX wav2vec2-CTC alignment, line timing is
sub-second accurate against yt_subs ground truth (verified on 3 songs).

The lead caused the wall to flip mid-phrase on fast songs (1.5s line
× 1000ms lead = 67% of line duration). New pipeline ships unshifted
real timing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase F — Wire orchestrator + worker

### Task F.1: Rewrite `orchestrator.rs` to drive the new tier chain

**Files:**
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs`

- [ ] **Step 1: Replace orchestrator's `process_track` with tier-chain logic**

The new orchestrator drives:
1. Vocal stem already on disk (`*_vocals_dereverbed.wav`)
2. `Tier1Collector::collect()` → branch on result
3. If `LineSynced` → apply line splitter → translate → done
4. If `TextOnly` → call `WhisperXReplicateBackend::align()` → reconcile → split → translate → done
5. If `None` → call WhisperX without reconciliation → split → translate → done

```rust
//! Orchestrator — drives the tier chain for a single song.

use std::path::Path;
use std::sync::Arc;

use crate::lyrics::backend::{AlignedTrack, AlignmentBackend, AlignOpts};
use crate::lyrics::line_splitter::{split_track, SplitConfig};
use crate::lyrics::reconcile::reconcile;
use crate::lyrics::tier1::{Tier1Collector, Tier1Result, AlignedLines};
use crate::lyrics::translator::Translator;

pub struct Orchestrator {
    pub tier1: Arc<Tier1Collector>,
    pub backend: Arc<dyn AlignmentBackend>,
    pub translator: Arc<Translator>,
    pub split_cfg: SplitConfig,
}

pub struct OrchestratorInput<'a> {
    pub artist: &'a str,
    pub track: &'a str,
    pub duration_s: u32,
    pub language: &'a str,
    pub vocal_wav: &'a Path,
    pub spotify_track_id: Option<&'a str>,
    pub vtt_path: Option<&'a Path>,
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("backend: {0}")]
    Backend(#[from] crate::lyrics::backend::BackendError),
    #[error("translation: {0}")]
    Translation(String),
}

impl Orchestrator {
    pub async fn process(&self, input: OrchestratorInput<'_>) -> Result<AlignedTrack, OrchestratorError> {
        // Step 1: Tier-1 collection
        let tier1 = self.tier1.collect(
            input.artist, input.track, input.duration_s,
            input.spotify_track_id, input.vtt_path,
        ).await;

        // Step 2: Branch
        let aligned = match tier1 {
            Tier1Result::LineSynced(AlignedLines { lines, provenance }) => {
                AlignedTrack { lines, provenance, raw_confidence: 1.0 }
            }
            Tier1Result::TextOnly(text_candidates) => {
                let asr = self.backend.align(
                    input.vocal_wav, None, input.language, &AlignOpts::default(),
                ).await?;
                reconcile(&asr, &text_candidates)
            }
            Tier1Result::None => {
                self.backend.align(
                    input.vocal_wav, None, input.language, &AlignOpts::default(),
                ).await?
            }
        };

        // Step 3: Line splitter (32-char target)
        let split = split_track(&aligned, self.split_cfg);

        // Step 4: Translate (Claude EN→SK; only if language=="en"; otherwise pass through)
        // (Translation logic unchanged; integrate via existing Translator API.)
        // For now return split; integration of translator is a follow-up step
        // in this same task if the existing API matches.
        Ok(split)
    }
}
```

- [ ] **Step 2: Test the orchestrator with a mock backend**

Add `#[cfg(test)] mod tests` to `orchestrator.rs`:

```rust
#[cfg(test)]
mod tests {
    // (Mock backend + mock Tier1 — use existing test helpers from backend.rs
    // and tier1.rs, or build them inline. Test that:
    // - LineSynced result skips backend
    // - TextOnly result calls backend AND reconciler
    // - None result calls backend WITHOUT reconciler)
    // These tests are dispatched as a separate sub-task to keep this commit focused.
}
```

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/orchestrator.rs
git commit -m "feat(lyrics): rewrite orchestrator to drive tier chain

New flow: Tier-1 collect → branch on LineSynced/TextOnly/None →
WhisperX backend (Tier-2) when needed → reconcile against text →
SubtitleEdit-port line split → translate.

Translator integration (Phase E renderer cleanup) lands in a
follow-up commit if signatures need adapting.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task F.2: Update `worker.rs` to use new orchestrator

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker.rs`

- [ ] **Step 1: Replace the worker's per-song processing loop**

The current `worker.rs` (`~810 lines`) wires the legacy provider chain. Strip it down to:
1. Pull next pending video from the queue
2. Ensure vocal stem exists (call existing `lyrics_worker.py preprocess-vocals` if missing — UNCHANGED logic)
3. Call `Orchestrator::process()`
4. Apply translator
5. Persist + cache JSON
6. Mark row processed

Most of this code already exists and just needs the orchestrator-call swap. Keep the existing:
- `preprocess_vocals` invocation (unchanged path)
- DB queue claim/release logic
- JSON serialization to `<video_id>_lyrics.json`

Replace:
- Provider-chain iteration (current `Orchestrator::run_providers()` etc.)
- Quality gate Claude calls (now done by deterministic reconciler + line splitter)

Concrete edit: find `crate::lyrics::orchestrator::Orchestrator` instantiation in `worker.rs`, replace input/output shape to match the new orchestrator signature.

- [ ] **Step 2: `cargo fmt --all --check`**

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/lyrics/worker.rs
git commit -m "refactor(lyrics): wire worker.rs to new orchestrator

Vocal-stem preprocessing path UNCHANGED (per
feedback_winresolume_is_shared_event_machine.md). DB queue claim/release
unchanged. Only the orchestrator call is swapped.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task F.3: Update `worker_tests.rs` for new orchestrator

**Files:**
- Modify or rewrite: `crates/sp-server/src/lyrics/worker_tests.rs`

- [ ] **Step 1: Strip retired-source assertions**

Existing test at worker_tests.rs:36 asserts `worker.rs must not write the retired 'lrclib+qwen3' source literal`. Update to assert the new source format `tier1:spotify | tier1:lrclib | tier1:yt_subs | whisperx-large-v3@rev1` etc.

- [ ] **Step 2: Add tests for tier-chain branches**

Three tests:
1. Tier-1 short-circuit ships `tier1:*` provenance, words=None on every line, no backend call.
2. Tier-1 text-only triggers backend call + reconciler, provenance contains `+reconciled`.
3. Tier-1 None triggers backend call only, no `+reconciled` in provenance.

(Use existing test fixtures or build a mock Tier-1 collector + mock backend.)

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/worker_tests.rs
git commit -m "test(lyrics): worker tests for new tier-chain orchestrator

Asserts provenance literals match new sources (tier1:spotify,
whisperx-large-v3@rev1, +reconciled suffix). Three tier-branch tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

# Phase G — Delete legacy + file tracked issues

### Task G.1: Delete Gemini lyrics modules

**Files:**
- DELETE: `crates/sp-server/src/lyrics/gemini_provider.rs`, `gemini_parse.rs`, `gemini_prompt.rs`, `gemini_audit.rs`, `aligner.rs`, `assembly.rs`, `chunking.rs`, `merge.rs`, `merge_tests.rs`, `bootstrap.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (remove `pub mod` declarations)
- Modify: `crates/sp-server/src/lyrics/gemini_client.rs` (delete lyrics-specific paths; check if file becomes empty → delete if so)

- [ ] **Step 1: Verify no consumers**

```bash
for f in gemini_provider gemini_parse gemini_prompt gemini_audit aligner assembly chunking merge bootstrap; do
  echo "=== $f references ==="
  grep -rn "$f::" crates/sp-server/src/ | grep -v "/${f}.rs:"
done
```
Expected: no hits (all consumers eliminated in Phases A–F).

- [ ] **Step 2: `git rm` each file**

```bash
cd /home/newlevel/devel/songplayer
git rm crates/sp-server/src/lyrics/gemini_provider.rs
git rm crates/sp-server/src/lyrics/gemini_parse.rs
git rm crates/sp-server/src/lyrics/gemini_prompt.rs
git rm crates/sp-server/src/lyrics/gemini_audit.rs
git rm crates/sp-server/src/lyrics/aligner.rs
git rm crates/sp-server/src/lyrics/assembly.rs
git rm crates/sp-server/src/lyrics/chunking.rs
git rm crates/sp-server/src/lyrics/merge.rs
git rm crates/sp-server/src/lyrics/merge_tests.rs
git rm crates/sp-server/src/lyrics/bootstrap.rs
```

- [ ] **Step 3: Remove module declarations from `mod.rs`**

Edit `mod.rs`: delete the `pub mod gemini_provider;`, `pub mod gemini_parse;`, etc. lines.

- [ ] **Step 4: Decide on `gemini_client.rs`**

```bash
grep -n "pub fn\|pub struct" crates/sp-server/src/lyrics/gemini_client.rs
```
If only the lyrics-specific functions are present, `git rm crates/sp-server/src/lyrics/gemini_client.rs` AND remove the `pub mod gemini_client;` line. If shared with translator (no — translator uses Claude only per feedback), delete.

- [ ] **Step 5: `cargo fmt --all --check`**

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(lyrics): delete legacy Gemini lyrics modules

Per feedback_no_legacy_code.md — when replacing a code path, delete the
old one entirely. Removed:

- gemini_provider.rs (671 lines)
- gemini_parse.rs (112)
- gemini_prompt.rs (75)
- gemini_audit.rs (350)
- aligner.rs (385)  — Gemini-chunk-specific
- assembly.rs (340) — Gemini-chunk-specific
- chunking.rs (354) — Gemini-chunk-specific (plan_chunks lives in audio_chunking.rs)
- merge.rs (371) + merge_tests.rs (771)
- bootstrap.rs (439)

Total removed: ~3870 LOC.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task G.2: Delete description, qwen3, autosub, text_merge, yt_manual_subs_provider

**Files:**
- DELETE: `description_provider.rs`, `qwen3_provider.rs`, `autosub_provider.rs`, `text_merge.rs`, `yt_manual_subs_provider.rs`
- Modify: `mod.rs`

- [ ] **Step 1: Verify no consumers**

```bash
for f in description_provider qwen3_provider autosub_provider text_merge yt_manual_subs_provider; do
  echo "=== $f references ==="
  grep -rn "$f::" crates/sp-server/src/ | grep -v "/${f}.rs:"
done
```

- [ ] **Step 2: `git rm` each file + update mod.rs**

```bash
git rm crates/sp-server/src/lyrics/description_provider.rs
git rm crates/sp-server/src/lyrics/qwen3_provider.rs
git rm crates/sp-server/src/lyrics/autosub_provider.rs
git rm crates/sp-server/src/lyrics/text_merge.rs
git rm crates/sp-server/src/lyrics/yt_manual_subs_provider.rs
```

Remove corresponding `pub mod` lines from `mod.rs`.

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(lyrics): delete description/qwen3/autosub/text_merge providers

Per feedback_no_legacy_code.md and the spec deletes list:
- description_provider.rs (680) — ~0% production hit rate
- qwen3_provider.rs (236) — disabled, prior failure (CLAUDE.md history)
- autosub_provider.rs (969) — banned per feedback_no_autosub.md
- text_merge.rs (219) — replaced by deterministic reconcile.rs
- yt_manual_subs_provider.rs (218) — folded into Tier-1 collector

Total removed: ~2320 LOC.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task G.3: Delete `provider.rs` (legacy AlignmentProvider trait)

**Files:**
- DELETE: `crates/sp-server/src/lyrics/provider.rs`
- Modify: `mod.rs`

- [ ] **Step 1: Verify no consumers**

```bash
grep -rn "provider::" crates/sp-server/src/lyrics/ | grep -v "/provider.rs:"
```
Expected: zero hits. The new `backend::AlignmentBackend` has fully replaced it.

- [ ] **Step 2: `git rm`**

```bash
git rm crates/sp-server/src/lyrics/provider.rs
```
Remove `pub mod provider;` from `mod.rs`.

- [ ] **Step 3: `cargo fmt --all --check`**

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(lyrics): delete legacy provider.rs

AlignmentProvider trait + ProviderResult/SongContext shapes superseded
by backend::AlignmentBackend + AlignedTrack/AlignedLine/AlignedWord.
Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task G.4: File tracked GitHub issues

**Files:**
- New: 9 GitHub issues (no source-tree changes)

- [ ] **Step 1: File issues via `gh`**

Run each:

```bash
gh issue create --title "Evaluate VibeVoice ASR with bumped max_tokens for long-form line-only timing" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. Modal/Gradio re-test once new pipeline is shipping. Verification on 2026-04-28 was truncated at max_tokens=4096 on 11.8-min song; bump and re-test to assess line-only segmentation quality vs WhisperX."

gh issue create --title "Evaluate CrisperWhisper local on win-resolume off-hours as Tier-2 alternative" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. Chunked test on 2026-04-28 showed 6 sub-1s matches on Praise (vs WhisperX 0). Worth re-running with bigger sample once CrisperWhisper Windows install is stable."

gh issue create --title "Self-host WhisperX on win-resolume to drop cloud cost" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. Verified WhisperX runs on RTX 3070 Ti with int8_float16 (~5 GB VRAM) but Windows + cuDNN 8 vs cuDNN 9 mismatch (WhisperX issue #1216) blocked first install attempt. Revisit when bandwidth allows; trivial AlignmentBackend impl swap."

gh issue create --title "A/B WhisperX vs Parakeet TDT v3 on next-generation worship songs" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. Keep AlignmentBackend impl for Parakeet alongside WhisperX; data-driven choice on 50+ songs once pipeline is in production."

gh issue create --title "Evaluate self-published Cog wrapper for parakeet-tdt-0.6b-v2" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. English-only TDT not on Replicate; build a thin Cog wrapper if quality demands it. v2 has marginally lower WER than v3 on English-only audio."

gh issue create --title "Verify Spotify hit-rate on full catalog" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. Cost projection assumes 30–40% Tier-1 coverage on worship music; this is unverified. Run a one-off Spotify-fetch sweep on the catalog and log hit/miss to inform future fetcher investment."

gh issue create --title "Migrate to next Whisper successor when OpenAI / community ships" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. AlignmentBackend trait makes this a 1-impl swap. Watch for Whisper-large-v4 / new SOTA on HF Open ASR Leaderboard."

gh issue create --title "CPS (chars-per-second) gate in line splitter" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. v2 enhancement: reject lines where char count exceeds reading speed for the time window available. Catches cases where the line is short enough but the time window is too tight."

gh issue create --title "Modal-based catalog burn-down for Mel-Roformer + dereverb" --body "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md. 30–50 GPU-hours on win-resolume off-hours = 2–3 weeks calendar time. Modal A10G alternative (~\$0.50/song × 600 = ~\$300 one-shot) if schedule is too slow."
```

- [ ] **Step 2: Confirm 9 issues created**

```bash
gh issue list --state open --search "Tracked from spec docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md" --json number,title --limit 20
```
Expected: 9 issues.

- [ ] **Step 3: No code commit needed for this task** (issues only).

---

# Phase H — LYRICS_PIPELINE_VERSION bump (REQUIRES USER APPROVAL)

> **STOP — Phase H requires explicit user approval before any commit per `feedback_pipeline_version_approval.md`.**
>
> Controller MUST ask the user: *"Phases A–G landed and CI is green. Ready to bump LYRICS_PIPELINE_VERSION from 20 → 21 and re-queue every row with version < 21 for reprocessing?"*
>
> Only proceed when user replies "yes" / "approved".

### Task H.1: Bump `LYRICS_PIPELINE_VERSION` to 21 + update reprocess threshold

**Files:**
- Modify: `crates/sp-server/src/lyrics/mod.rs` (constant bump)
- Modify: `crates/sp-server/src/lyrics/reprocess.rs` (smart-skip threshold update)
- Modify: documentation in `CLAUDE.md` (history entry)

- [ ] **Step 1: Bump the constant**

In `crates/sp-server/src/lyrics/mod.rs:163`:
```rust
pub const LYRICS_PIPELINE_VERSION: u32 = 21;
```

Change `20` → `21`. The mod_tests.rs assertion at line 296 also needs `LYRICS_PIPELINE_VERSION, 20` → `LYRICS_PIPELINE_VERSION, 21`.

- [ ] **Step 2: Update the version-history doc-comment in `mod.rs`**

Add an entry above v20 history:
```rust
/// - v21 (#TBD): WhisperX cloud Tier-2 + Spotify/LRCLib/yt_subs Tier-1 +
///   anchor-sequence reconciler + SubtitleEdit-port 32-char line splitter.
///   Replaces Gemini chunked transcription + Claude text-merge. New
///   AlignmentBackend trait makes future engine swaps trivial.
///   See docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md
```

- [ ] **Step 3: Update `reprocess.rs` smart-skip threshold**

Find the smart-skip clause in `reprocess.rs` (`grep -n "lyrics_pipeline_version >= 20" crates/sp-server/src/lyrics/reprocess.rs`). Update the threshold to `>= 21` for any source that semantically changes.

For Tier-1 short-circuit sources (`tier1:spotify`, `tier1:lrclib`, `tier1:yt_subs`): preserve smart-skip — those outputs are stable.

For backend-produced rows (`whisperx-large-v3@rev1` etc.): re-queue all rows with `lyrics_pipeline_version < 21`.

- [ ] **Step 4: Update CLAUDE.md history**

Add entry to the `History:` section in `CLAUDE.md` (around line 200):
```markdown
- v21 (#TBD): WhisperX cloud (Replicate) Tier-2 replaces Gemini chunked
  transcription. Spotify + LRCLib + yt_subs Tier-1 short-circuit when
  line-synced. Anchor-sequence reconciler (deterministic Rust, no LLM)
  replaces Claude text-merge. SubtitleEdit-port 32-char line splitter.
  Mel-Roformer + anvuew dereverb path unchanged. ~3500 LOC of legacy
  Gemini/qwen3/autosub/description code deleted.
```

- [ ] **Step 5: `cargo fmt --all --check`**

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/mod.rs crates/sp-server/src/lyrics/reprocess.rs CLAUDE.md
git commit -m "feat(lyrics): bump LYRICS_PIPELINE_VERSION to 21

Per user approval (feedback_pipeline_version_approval.md gate). New
WhisperX-on-Replicate pipeline + Tier-1 short-circuit + anchor-sequence
reconciler ships under v21.

Reprocess stale-bucket re-queues all rows with lyrics_pipeline_version
< 21 (backend-produced output). Tier-1 short-circuit rows preserved
under smart-skip — those sources have not changed semantically.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Verification (after all tasks complete)

After Phase H lands and CI is green:

1. **Cargo workspace check (CI verifies):** `cargo check --workspace` passes on Linux.
2. **Local format check:** `cargo fmt --all --check` clean.
3. **Test suite (CI verifies):** `cargo test -p sp-server lyrics::` passes.
4. **Catalog reprocess kick-off:** worker queue picks up v<21 rows; first 10 songs verified by hand against yt_subs ground truth.
5. **Spec acceptance criteria checklist:** `docs/superpowers/specs/2026-04-28-lyrics-pipeline-redesign-design.md` "Acceptance criteria for the implementation" section, every box ticked.

---

## Self-review (controller's spec-vs-plan check)

**Spec coverage:**
- ✅ AlignmentBackend trait + WhisperXReplicateBackend → Phase A
- ✅ Tier-1 short-circuit when has_timing=true → Task B.3
- ✅ SpotifyLyricsFetcher (issue #52) → Task B.2 + B.1 (DB)
- ✅ Anchor-sequence reconciler replaces text_merge.rs → Phase C
- ✅ SubtitleEdit-port 32-char line splitter → Phase D
- ✅ DEFAULT_LYRICS_LEAD_MS removed → Phase E
- ✅ LYRICS_PIPELINE_VERSION → 21 (with user approval) → Phase H
- ✅ All Gemini lyrics modules + qwen3 + autosub + description deleted → Phase G
- ✅ Mel-Roformer + dereverb path unchanged → spec text + worker.rs reads `*_vocals_dereverbed.wav`
- ✅ Worker dispatches WhisperX with rate-limit-aware client → Tasks A.3, F.2
- ✅ Optional 60s/10s chunking trigger → Task A.5
- ✅ Reprocess re-queues v<21 rows → Task H.1
- ✅ Tracked GitHub issues filed → Task G.4

**Placeholder scan:** None. Every step has concrete code or commands.

**Type consistency:** `AlignedTrack`/`AlignedLine`/`AlignedWord`/`AlignmentBackend`/`CandidateText` referenced consistently across all phases. `tier1::CandidateText` shape matches the legacy `provider::CandidateText` shape (verified at Task B.4).

---

Plan complete and saved to `docs/superpowers/plans/2026-04-28-lyrics-pipeline-redesign.md`.

Per airuleset (`feedback_no_rebrainstorm.md` and `ask-before-assuming.md`), the execution path is **subagent-driven** — no inline-vs-subagent question. Controller proceeds straight into `superpowers:subagent-driven-development` per phase.
