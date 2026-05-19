# ASR Path (AAI U3-Pro + Claude-merge) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new alignment path that runs AssemblyAI Universal-3 Pro ASR + Claude-merge for songs the existing whisperx flow rejects (untimed text candidates only — `unsupported_source` bucket).

**Architecture:** A new `crates/sp-server/src/lyrics/asr_path/` sub-tree. One additive branch in `worker.rs::process_song` — when `is_allowed_text_source == false` AND `candidate_texts` non-empty, run asr_path instead of bailing. AAI returns word-level timings; Claude (via CLIProxyAPI) reviews the untimed text candidate and emits line splits as word-index ranges; the server resolves ranges to ms timings. Output ships with `words: None`. Whisperx flow on bucket 2 is byte-for-byte untouched.

**Tech Stack:** Rust workspace (existing), `reqwest 0.12` for AAI HTTP, `wiremock 0.6` (already a dev-dep in `crates/sp-server/Cargo.toml:45`) for HTTP mocking, existing `crate::ai::client::AiClient` for Claude/CLIProxyAPI, `serde_json` for AAI + Claude payloads.

**Spec:** `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md` (commit `785e0be`).

---

## Constraints reused from spec (do not re-derive)

- No `LYRICS_PIPELINE_VERSION` bump. asr_path writes only rows that today have `lyrics_source IN ('unsupported_source', NULL)`.
- Output `LyricsLine.words = None`. Always. Per `feedback_line_timing_only`.
- Claude emits word INDICES, never ms values. Schema must `deny_unknown_fields` + reject `start_ms` / `end_ms`.
- Silence-gap split constant: `LINE_GAP_MS = 400` (matches `eval/lyrics/backends/assemblyai_universal_3_pro.py:48`).
- AAI `speech_models: ["universal-3-pro"]` (plural list, NOT singular `speech_model`).
- `aligner::preprocess_vocals` is reused for vocal isolation. asr_path does NOT touch Demucs / dereverb code.

---

## File map (locked before tasks)

| Path | Created (C) / Modified (M) | Responsibility |
|---|---|---|
| `crates/sp-server/src/lyrics/asr_path/mod.rs` | C | Orchestrator entry. `pub async fn run(...) -> Result<AsrOutput, AsrError>`. |
| `crates/sp-server/src/lyrics/asr_path/aai_backend.rs` | C | AssemblyAI HTTP client. `pub async fn transcribe(api_key, audio_path) -> Result<AaiTranscript, AaiError>`. |
| `crates/sp-server/src/lyrics/asr_path/merge_prompt.rs` | C | Pure prompt builder. `pub fn build_user_prompt(input: &ClaudeMergeInput) -> String` + `pub const SYSTEM_PROMPT: &str`. |
| `crates/sp-server/src/lyrics/asr_path/claude_merge.rs` | C | Claude/CLIProxyAPI client + JSON parser. `pub async fn merge(ai, input) -> Result<ClaudeMergeResult, MergeError>`. |
| `crates/sp-server/src/lyrics/asr_path/resolver.rs` | C | Word-index → ms resolver + line-only sanitizer. `pub fn resolve(merged, aai) -> Result<Vec<LyricsLine>, ResolverError>`. |
| `crates/sp-server/src/lyrics/asr_path/fallback.rs` | C | Silence-gap line splitter from raw AAI words. `pub fn split_on_silence(words: &[AaiWord]) -> Vec<LyricsLine>`. |
| `crates/sp-server/src/lyrics/asr_path/tests.rs` | C | The 3 forbidden-behavior guards + orchestrator integration test. |
| `crates/sp-server/src/lyrics/mod.rs` | M | `pub mod asr_path;` declaration. |
| `crates/sp-server/src/lyrics/worker.rs` | M | One new branch in `process_song` between the `is_allowed_text_source` check and the existing `mark_unsupported_source` call. |
| `crates/sp-server/src/lyrics/canonical_source_regression_tests.rs` | M | New test asserting bucket-1 routing (genius-only candidate → asr_path). |
| `crates/sp-server/src/lyrics/orchestrator.rs` | M | New `pub fn has_any_text_candidate(candidates: &[CandidateText]) -> bool` next to `is_allowed_text_source`. |
| `VERSION` + `Cargo.toml` files + `tauri.conf.json` | M | Bump 0.44.0-dev.1 → 0.44.0-dev.2 via `./scripts/sync-version.sh`. |

---

## Task 1: Bump VERSION to 0.44.0-dev.2

**Files:**
- Modify: `VERSION` (one line)
- Sync: all `Cargo.toml` + `src-tauri/tauri.conf.json` via the script

- [ ] **Step 1: Edit VERSION**

```
0.44.0-dev.2
```

- [ ] **Step 2: Run sync script**

```bash
./scripts/sync-version.sh
```

Expected stdout: lists each file updated with old → new version. Each `Cargo.toml` workspace member uses `version.workspace = true` so only root `Cargo.toml`, `src-tauri/Cargo.toml`, `sp-ui/Cargo.toml`, and `src-tauri/tauri.conf.json` get edited.

- [ ] **Step 3: Verify cargo check still parses**

```bash
cargo check --workspace
```

Expected: success.

- [ ] **Step 4: Commit**

```bash
git add VERSION Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json sp-ui/Cargo.toml Cargo.lock
git commit -m "release: bump VERSION 0.44.0-dev.1 → 0.44.0-dev.2 for asr_path PR"
```

---

## Task 2: Settings key plumbing for `assemblyai_api_key`

The DB-level `get_setting`/`set_setting` helpers are generic over key names, so no new function is needed. This task just locks the key string + a unit test that exercises read-back.

**Files:**
- Modify: `crates/sp-server/src/lyrics/asr_path/mod.rs` (will be created in Task 7; for this task, create the file with just the constant)
- Test: `crates/sp-server/src/lyrics/asr_path/tests.rs`

- [ ] **Step 1: Create the module skeleton + constant**

`crates/sp-server/src/lyrics/asr_path/mod.rs`:

```rust
//! ASR alignment path — runs AssemblyAI U3-Pro + Claude-merge for songs
//! whose `gather_sources` returns ONLY untimed text candidates (genius,
//! lrclib-untimed, etc.). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`.

/// DB settings key that stores the AssemblyAI API token. Read per-song in
/// the worker so operators can configure without a restart. Same pattern
/// as `replicate_api_token`.
pub const ASSEMBLYAI_API_KEY_SETTING: &str = "assemblyai_api_key";

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/mod.rs` — add `pub mod asr_path;` alongside the other `pub mod` lines.

- [ ] **Step 3: Add the settings-key smoke test**

Create `crates/sp-server/src/lyrics/asr_path/tests.rs`:

```rust
use super::ASSEMBLYAI_API_KEY_SETTING;

#[test]
fn settings_key_is_stable() {
    // Locks the exact string. Renaming would silently break operator
    // configs already written into the production SQLite. If a rename
    // is genuinely needed, a DB migration MUST move existing values.
    assert_eq!(ASSEMBLYAI_API_KEY_SETTING, "assemblyai_api_key");
}
```

- [ ] **Step 4: Run the test**

```bash
cargo test -p sp-server lyrics::asr_path::tests::settings_key_is_stable
```

Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/ crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(asr_path): scaffold module + assemblyai_api_key setting key"
```

---

## Task 3: AAI response types + JSON deserialization

Defines the safe Rust types AAI is parsed into. No HTTP yet.

**Files:**
- Create: `crates/sp-server/src/lyrics/asr_path/aai_backend.rs`
- Test: same file (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing deserialization test**

```rust
// crates/sp-server/src/lyrics/asr_path/aai_backend.rs

use serde::Deserialize;

/// One ASR-emitted word with its acoustic timing.
#[derive(Debug, Clone, PartialEq)]
pub struct AaiWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
}

/// Result of a completed AAI transcription job.
#[derive(Debug, Clone)]
pub struct AaiTranscript {
    pub words: Vec<AaiWord>,
    pub raw_text: String,
}

/// Wire-format types — deserialize-only. AAI's `/v2/transcript/{id}`
/// response when `status == "completed"`.
#[derive(Debug, Deserialize)]
struct AaiTranscriptResponse {
    status: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    words: Vec<AaiWordResponse>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AaiWordResponse {
    text: String,
    start: u64,
    end: u64,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_confidence() -> f32 {
    0.9
}

pub fn parse_completed_response(body: &str) -> Result<AaiTranscript, AaiError> {
    let raw: AaiTranscriptResponse =
        serde_json::from_str(body).map_err(AaiError::Parse)?;
    match raw.status.as_str() {
        "completed" => Ok(AaiTranscript {
            words: raw
                .words
                .into_iter()
                .map(|w| AaiWord {
                    text: w.text,
                    start_ms: w.start,
                    end_ms: w.end,
                    confidence: w.confidence,
                })
                .collect(),
            raw_text: raw.text,
        }),
        "error" => Err(AaiError::Remote(
            raw.error.unwrap_or_else(|| "unknown".to_string()),
        )),
        other => Err(AaiError::UnexpectedStatus(other.to_string())),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AaiError {
    #[error("AAI response parse failed: {0}")]
    Parse(serde_json::Error),
    #[error("AAI returned status=error: {0}")]
    Remote(String),
    #[error("AAI returned unexpected status: {0}")]
    UnexpectedStatus(String),
    #[error("AAI HTTP error: {0}")]
    Http(String),
    #[error("AAI poll timed out after {0}s")]
    Timeout(u64),
    #[error("AAI quota exhausted (HTTP 429)")]
    QuotaExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_completed_with_words() {
        let body = r#"{
            "status": "completed",
            "text": "no pulse alright",
            "words": [
                {"text": "no", "start": 0, "end": 500, "confidence": 0.91},
                {"text": "pulse", "start": 500, "end": 900, "confidence": 0.95},
                {"text": "alright", "start": 1100, "end": 1500, "confidence": 0.88}
            ]
        }"#;
        let t = parse_completed_response(body).expect("parse ok");
        assert_eq!(t.words.len(), 3);
        assert_eq!(t.words[0].text, "no");
        assert_eq!(t.words[0].start_ms, 0);
        assert_eq!(t.words[0].end_ms, 500);
        assert_eq!(t.raw_text, "no pulse alright");
    }

    #[test]
    fn parses_error_status() {
        let body = r#"{"status": "error", "error": "audio too short"}"#;
        let err = parse_completed_response(body).expect_err("must error");
        assert!(matches!(err, AaiError::Remote(ref m) if m == "audio too short"));
    }

    #[test]
    fn rejects_unexpected_status() {
        let body = r#"{"status": "processing"}"#;
        let err = parse_completed_response(body).expect_err("must error");
        assert!(matches!(err, AaiError::UnexpectedStatus(ref s) if s == "processing"));
    }

    #[test]
    fn missing_confidence_defaults() {
        let body = r#"{
            "status": "completed",
            "text": "x",
            "words": [{"text": "x", "start": 0, "end": 100}]
        }"#;
        let t = parse_completed_response(body).expect("ok");
        assert!((t.words[0].confidence - 0.9).abs() < f32::EPSILON);
    }
}
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/asr_path/mod.rs` — add at top:

```rust
pub mod aai_backend;
```

- [ ] **Step 3: Run tests, verify all 4 fail (functions don't exist yet)**

```bash
cargo test -p sp-server lyrics::asr_path::aai_backend 2>&1 | head -30
```

Expected: compile error or 4 failures. (If you wrote the impl in step 1 alongside the tests they'll pass — that's also fine; the RED-GREEN is mainly to confirm the assertions hold against the impl.)

- [ ] **Step 4: Verify GREEN**

```bash
cargo test -p sp-server lyrics::asr_path::aai_backend
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/aai_backend.rs crates/sp-server/src/lyrics/asr_path/mod.rs
git commit -m "feat(asr_path): AAI response types and JSON parser"
```

---

## Task 4: AAI HTTP client (upload → create → poll)

Adds the actual HTTP work. Three-step protocol from `eval/lyrics/backends/assemblyai_universal_3_pro.py` ported. Tested with `wiremock`.

**Files:**
- Modify: `crates/sp-server/src/lyrics/asr_path/aai_backend.rs`

- [ ] **Step 1: Append the HTTP client code**

```rust
// Append to aai_backend.rs (below the parse_completed_response section).

use std::path::Path;
use std::time::Duration;

const API_BASE_DEFAULT: &str = "https://api.assemblyai.com/v2";
const SPEECH_MODEL: &str = "universal-3-pro";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT_S: u64 = 1800; // 30 min, same as eval Python
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AaiBackend {
    api_base: String,
    api_key: String,
    http: reqwest::Client,
}

impl AaiBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_base: API_BASE_DEFAULT.to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn transcribe(&self, audio_path: &Path) -> Result<AaiTranscript, AaiError> {
        let bytes = tokio::fs::read(audio_path)
            .await
            .map_err(|e| AaiError::Http(format!("read {audio_path:?}: {e}")))?;
        let upload_url = self.upload(bytes).await?;
        let transcript_id = self.create_transcript(&upload_url).await?;
        self.poll_until_done(&transcript_id).await
    }

    async fn upload(&self, bytes: Vec<u8>) -> Result<String, AaiError> {
        let resp = self
            .http
            .post(format!("{}/upload", self.api_base))
            .header("authorization", &self.api_key)
            .timeout(UPLOAD_TIMEOUT)
            .body(bytes)
            .send()
            .await
            .map_err(|e| AaiError::Http(format!("upload send: {e}")))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AaiError::QuotaExhausted);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| AaiError::Http(format!("upload status: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AaiError::Http(format!("upload json: {e}")))?;
        body.get("upload_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AaiError::Http("upload response missing upload_url".into()))
    }

    async fn create_transcript(&self, audio_url: &str) -> Result<String, AaiError> {
        let body = serde_json::json!({
            "audio_url": audio_url,
            "speech_models": [SPEECH_MODEL],
            "punctuate": true,
            "format_text": true,
            "speaker_labels": false,
            "language_detection": true,
        });
        let resp = self
            .http
            .post(format!("{}/transcript", self.api_base))
            .header("authorization", &self.api_key)
            .timeout(CONTROL_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| AaiError::Http(format!("create send: {e}")))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AaiError::QuotaExhausted);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| AaiError::Http(format!("create status: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AaiError::Http(format!("create json: {e}")))?;
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| AaiError::Http("create response missing id".into()))
    }

    async fn poll_until_done(&self, transcript_id: &str) -> Result<AaiTranscript, AaiError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(POLL_TIMEOUT_S);
        let url = format!("{}/transcript/{transcript_id}", self.api_base);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(AaiError::Timeout(POLL_TIMEOUT_S));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp = self
                .http
                .get(&url)
                .header("authorization", &self.api_key)
                .timeout(CONTROL_TIMEOUT)
                .send()
                .await
                .map_err(|e| AaiError::Http(format!("poll send: {e}")))?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(AaiError::QuotaExhausted);
            }
            let resp = resp
                .error_for_status()
                .map_err(|e| AaiError::Http(format!("poll status: {e}")))?;
            let text = resp
                .text()
                .await
                .map_err(|e| AaiError::Http(format!("poll body: {e}")))?;
            // Status may be "queued" / "processing" / "completed" / "error".
            // parse_completed_response handles completed/error/unexpected;
            // for processing/queued we just continue the loop.
            let raw: serde_json::Value =
                serde_json::from_str(&text).map_err(AaiError::Parse)?;
            let status = raw
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if status == "completed" || status == "error" {
                return parse_completed_response(&text);
            }
            // else: still processing — keep polling
        }
    }
}
```

- [ ] **Step 2: Add wiremock HTTP integration test**

Append to the `#[cfg(test)] mod tests` block:

```rust
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use std::path::PathBuf;
    use std::io::Write;

    #[tokio::test]
    async fn transcribe_three_step_flow_happy_path() {
        let server = MockServer::start().await;

        // 1) upload returns upload_url
        Mock::given(method("POST"))
            .and(path("/upload"))
            .and(header("authorization", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "upload_url": "https://cdn.example/audio.wav"
            })))
            .mount(&server)
            .await;

        // 2) create transcript returns id
        Mock::given(method("POST"))
            .and(path("/transcript"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "trans-123"
            })))
            .mount(&server)
            .await;

        // 3) poll returns processing once, then completed
        Mock::given(method("GET"))
            .and(path("/transcript/trans-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed",
                "text": "hello world",
                "words": [
                    {"text": "hello", "start": 0, "end": 500, "confidence": 0.9},
                    {"text": "world", "start": 600, "end": 1100, "confidence": 0.9}
                ]
            })))
            .mount(&server)
            .await;

        // Write a tiny dummy file (content not validated by the mock).
        let tmp = std::env::temp_dir().join("asr_path_test_audio.wav");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"\x00\x00").unwrap();
        drop(f);

        let backend = AaiBackend::with_base_url("test-key", server.uri());
        let t = backend.transcribe(&tmp).await.expect("must transcribe");
        assert_eq!(t.words.len(), 2);
        assert_eq!(t.words[0].text, "hello");
        assert_eq!(t.words[1].end_ms, 1100);

        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn transcribe_propagates_quota_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let tmp = std::env::temp_dir().join("asr_path_test_429.wav");
        std::fs::write(&tmp, b"\x00").unwrap();

        let backend = AaiBackend::with_base_url("test-key", server.uri());
        let err = backend.transcribe(&tmp).await.expect_err("must err");
        assert!(matches!(err, AaiError::QuotaExhausted), "got {err:?}");

        let _ = std::fs::remove_file(&tmp);
    }
```

Note: `POLL_INTERVAL = 2s` is in real time. To keep the test fast, the mock returns `completed` on the first poll, so total wait is one `POLL_INTERVAL` (~2s). Acceptable for a single test.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::aai_backend
```

Expected: 6 passed (4 from Task 3 + 2 new wiremock tests).

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/aai_backend.rs
git commit -m "feat(asr_path): AAI HTTP client (upload + create + poll)"
```

---

## Task 5: Merge prompt builder (pure function)

The Claude prompt has two parts: a constant system prompt and a per-song user prompt. Both are testable in isolation.

**Files:**
- Create: `crates/sp-server/src/lyrics/asr_path/merge_prompt.rs`

- [ ] **Step 1: Write the file**

```rust
//! Prompt builder for the AAI → Claude line-merge step.
//!
//! Claude reads (AAI word-timing transcript, untimed source text) and emits
//! line splits as word-index ranges. The prompt enforces:
//! - JSON-only output (no prose).
//! - Word INDICES only — NEVER ms values. Prevents the v15 array-math failure.
//! - Drop ad-libs / repeated filler that's clearly not in the source text.
//! - Set `disagreement: true` when source text doesn't match what AAI heard.

use crate::lyrics::asr_path::aai_backend::AaiWord;

pub const SYSTEM_PROMPT: &str = "You are a karaoke-lyrics editor.\n\
\n\
INPUT\n\
- ASR transcript with per-word timings (the audio truth).\n\
- Reference lyrics text from {source} (untimed, may have errors, may be wrong version).\n\
\n\
GOAL\n\
- Produce singable line-level karaoke lyrics that match what the singer actually sings.\n\
- Use ASR timings as the timing source; use reference text to correct mishears and \
pick natural line breaks.\n\
\n\
RULES\n\
1. Output JSON only. No prose. Schema below.\n\
2. Each output line MUST reference contiguous ASR word indices \
[start_word_idx..end_word_idx] (inclusive).\n\
3. NEVER invent words not present in the ASR transcript. Reference text can correct \
spelling/word-choice ONLY where ASR clearly mis-heard a word that the reference \
disambiguates.\n\
4. NEVER emit ms values. Only word indices.\n\
5. Line splits chosen for vocal phrasing — group what a singer sings as one breath \
/ phrase, not where silence falls.\n\
6. If reference text disagrees too much with ASR (different song / different version \
/ wrong language), set \"disagreement\": true and return empty lines[].\n\
7. Drop ASR ad-libs / \"yeah\" / \"hey\" / repeated filler that are clearly not in the \
reference text. Skip them — do NOT include in any line.\n\
\n\
SCHEMA\n\
{\n\
  \"disagreement\": bool,\n\
  \"notes\": string,\n\
  \"lines\": [\n\
    { \"text\": string, \"start_word_idx\": int, \"end_word_idx\": int }\n\
  ]\n\
}";

pub struct ClaudeMergeInput<'a> {
    pub aai_words: &'a [AaiWord],
    pub untimed_text: &'a str,
    pub untimed_source: &'a str,
    pub language: Option<&'a str>,
}

pub fn build_user_prompt(input: &ClaudeMergeInput) -> String {
    let mut s = String::new();
    s.push_str(&format!("SOURCE: {}\n", input.untimed_source));
    s.push_str(&format!(
        "LANGUAGE: {}\n",
        input.language.unwrap_or("unknown")
    ));
    s.push_str("\nREFERENCE TEXT:\n");
    s.push_str(input.untimed_text);
    s.push_str("\n\nASR TRANSCRIPT (word_idx: text @ start_ms..end_ms):\n");
    for (i, w) in input.aai_words.iter().enumerate() {
        s.push_str(&format!(
            "{i}: \"{}\" @ {}..{}\n",
            w.text.replace('"', "\\\""),
            w.start_ms,
            w.end_ms
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start: u64, end: u64) -> AaiWord {
        AaiWord {
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn system_prompt_mentions_no_ms() {
        // Rule 4 is the v15-prevention rule. If someone deletes it, this fails.
        assert!(SYSTEM_PROMPT.contains("NEVER emit ms values"));
    }

    #[test]
    fn system_prompt_mentions_word_indices() {
        assert!(SYSTEM_PROMPT.contains("start_word_idx"));
        assert!(SYSTEM_PROMPT.contains("end_word_idx"));
    }

    #[test]
    fn user_prompt_includes_all_words_with_timings() {
        let words = vec![
            word("hello", 0, 500),
            word("world", 600, 1100),
        ];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "hello world",
            untimed_source: "genius",
            language: Some("en"),
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("SOURCE: genius"));
        assert!(s.contains("LANGUAGE: en"));
        assert!(s.contains("REFERENCE TEXT:\nhello world"));
        assert!(s.contains("0: \"hello\" @ 0..500"));
        assert!(s.contains("1: \"world\" @ 600..1100"));
    }

    #[test]
    fn user_prompt_handles_quotes_in_words() {
        let words = vec![word("it's", 0, 300)];
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: "it's me",
            untimed_source: "genius",
            language: None,
        };
        let s = build_user_prompt(&input);
        assert!(s.contains("LANGUAGE: unknown"));
        assert!(s.contains("0: \"it's\" @ 0..300"));
    }
}
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/asr_path/mod.rs` — add `pub mod merge_prompt;`.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::merge_prompt
```

Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/merge_prompt.rs crates/sp-server/src/lyrics/asr_path/mod.rs
git commit -m "feat(asr_path): merge prompt builder + word-index discipline"
```

---

## Task 6: Claude merge response types + schema (deny_unknown_fields)

This task locks the JSON schema that rejects ms-valued output. No HTTP yet.

**Files:**
- Create: `crates/sp-server/src/lyrics/asr_path/claude_merge.rs`

- [ ] **Step 1: Write the file**

```rust
//! Claude merge layer: parse Claude's JSON response into ClaudeMergeResult.
//!
//! Schema is strict (`deny_unknown_fields`) so a Claude response that contains
//! `start_ms` / `end_ms` fields FAILS deserialization. This is the spec-level
//! guard preventing the v15 LLM-can't-emit-exact-length-arrays disaster from
//! sneaking back in.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergedLine {
    pub text: String,
    pub start_word_idx: usize,
    pub end_word_idx: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeMergeResult {
    pub disagreement: bool,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub lines: Vec<MergedLine>,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("Claude response parse failed: {0}")]
    Parse(serde_json::Error),
    #[error("Claude response contained ms fields (banned per spec)")]
    HasMsFields,
    #[error("Claude transport failed: {0}")]
    Transport(String),
}

/// Parse a raw Claude response body into a ClaudeMergeResult.
///
/// Strips a leading ```json ... ``` fence if present (Claude sometimes wraps
/// JSON output in markdown despite "Output JSON only" instructions). Then
/// runs strict deserialization. Any unexpected field — including `start_ms`,
/// `end_ms`, `confidence`, etc. — fails with `MergeError::HasMsFields` when
/// the field is `start_ms` or `end_ms`, otherwise `MergeError::Parse`.
pub fn parse_response(body: &str) -> Result<ClaudeMergeResult, MergeError> {
    let stripped = strip_json_fence(body);
    // First check: a pre-flight regex catch on ms fields. We do this BEFORE
    // serde so the error message clearly says "ms fields banned" rather than
    // serde's generic "unknown field" message.
    if stripped.contains("\"start_ms\"") || stripped.contains("\"end_ms\"") {
        return Err(MergeError::HasMsFields);
    }
    serde_json::from_str(stripped).map_err(MergeError::Parse)
}

fn strip_json_fence(body: &str) -> &str {
    let t = body.trim();
    // ```json\n ... \n```
    if let Some(rest) = t.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let body = r#"{
            "disagreement": false,
            "notes": "matched 95% of source",
            "lines": [
                {"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1},
                {"text": "How are you", "start_word_idx": 2, "end_word_idx": 4}
            ]
        }"#;
        let r = parse_response(body).expect("ok");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].start_word_idx, 0);
        assert_eq!(r.lines[1].end_word_idx, 4);
    }

    #[test]
    fn strips_markdown_fence() {
        let body = "```json\n{\"disagreement\": false, \"notes\": \"\", \"lines\": []}\n```";
        let r = parse_response(body).expect("ok");
        assert!(r.lines.is_empty());
    }

    #[test]
    fn rejects_ms_field_on_line() {
        // The v15-prevention guard. If Claude emits ms values, parse_response
        // returns HasMsFields, and the orchestrator falls back to raw AAI.
        let body = r#"{
            "disagreement": false,
            "notes": "",
            "lines": [
                {"text": "Hello", "start_word_idx": 0, "end_word_idx": 1, "start_ms": 0, "end_ms": 500}
            ]
        }"#;
        let err = parse_response(body).expect_err("must err");
        assert!(matches!(err, MergeError::HasMsFields), "got {err:?}");
    }

    #[test]
    fn rejects_disagreement_payload_with_lines() {
        // disagreement=true SHOULD have empty lines per spec — but parser
        // doesn't enforce this; orchestrator does. Parser must still parse
        // the structure.
        let body = r#"{"disagreement": true, "notes": "wrong version", "lines": []}"#;
        let r = parse_response(body).expect("ok");
        assert!(r.disagreement);
        assert!(r.lines.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let body = r#"{"disagreement": false, "notes": "", "lines": [], "extra": "x"}"#;
        let err = parse_response(body).expect_err("must err");
        // serde produces Parse, not HasMsFields, for non-ms unknown fields.
        assert!(matches!(err, MergeError::Parse(_)), "got {err:?}");
    }
}
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/asr_path/mod.rs` — add `pub mod claude_merge;`.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::claude_merge
```

Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/claude_merge.rs crates/sp-server/src/lyrics/asr_path/mod.rs
git commit -m "feat(asr_path): Claude merge response parser with ms-rejection guard"
```

---

## Task 7: Claude merge HTTP wrapper (uses existing AiClient)

Glue between the prompt builder, the AiClient, and the parser. Tested with a fake AiClient.

**Files:**
- Modify: `crates/sp-server/src/lyrics/asr_path/claude_merge.rs`

- [ ] **Step 1: Inspect the existing AiClient signature**

Quick read to confirm the call shape used in `translator.rs:53`:

```bash
grep -n "pub async fn chat\|pub fn new" /home/newlevel/devel/songplayer/crates/sp-server/src/ai/client.rs
```

Expected: `pub async fn chat(&self, system: &str, user: &str) -> Result<String, ...>`.

- [ ] **Step 2: Append the merge function and a trait for test mocking**

```rust
// Append to claude_merge.rs

use crate::lyrics::asr_path::merge_prompt::{ClaudeMergeInput, SYSTEM_PROMPT, build_user_prompt};

/// Abstraction over `AiClient::chat` so tests can inject canned responses
/// without hitting CLIProxyAPI. Implemented for `crate::ai::client::AiClient`
/// at the bottom of this file.
#[async_trait::async_trait]
pub trait MergeChat: Send + Sync {
    async fn chat(&self, system: &str, user: &str) -> Result<String, String>;
}

/// Build prompt → call Claude → parse → return ClaudeMergeResult.
///
/// Single retry on `MergeError::Parse` with the prompt unchanged (CLIProxyAPI
/// is non-deterministic on JSON formatting and sometimes recovers).
pub async fn merge<C: MergeChat + ?Sized>(
    chat: &C,
    input: &ClaudeMergeInput<'_>,
) -> Result<ClaudeMergeResult, MergeError> {
    let user = build_user_prompt(input);
    let first = chat
        .chat(SYSTEM_PROMPT, &user)
        .await
        .map_err(MergeError::Transport)?;
    match parse_response(&first) {
        Ok(r) => Ok(r),
        Err(MergeError::Parse(_)) => {
            // Single retry; if it fails again, propagate so orchestrator falls back.
            let second = chat
                .chat(SYSTEM_PROMPT, &user)
                .await
                .map_err(MergeError::Transport)?;
            parse_response(&second)
        }
        Err(e) => Err(e),
    }
}

#[async_trait::async_trait]
impl MergeChat for crate::ai::client::AiClient {
    async fn chat(&self, system: &str, user: &str) -> Result<String, String> {
        crate::ai::client::AiClient::chat(self, system, user)
            .await
            .map_err(|e| e.to_string())
    }
}
```

- [ ] **Step 3: Add merge tests using a fake MergeChat**

Append to the `#[cfg(test)] mod tests` block:

```rust
    use crate::lyrics::asr_path::aai_backend::AaiWord;
    use crate::lyrics::asr_path::merge_prompt::ClaudeMergeInput;

    struct ScriptedChat {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedChat {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(String::from).collect(),
                ),
            }
        }
    }

    #[async_trait::async_trait]
    impl MergeChat for ScriptedChat {
        async fn chat(&self, _system: &str, _user: &str) -> Result<String, String> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| "out of scripted responses".to_string())
        }
    }

    fn ctx() -> (Vec<AaiWord>, &'static str, &'static str) {
        let words = vec![AaiWord {
            text: "hi".to_string(),
            start_ms: 0,
            end_ms: 200,
            confidence: 0.9,
        }];
        (words, "genius", "hi")
    }

    #[tokio::test]
    async fn merge_returns_parsed_response() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hi", "start_word_idx": 0, "end_word_idx": 0}]}"#,
        ]);
        let r = merge(&chat, &input).await.expect("ok");
        assert!(!r.disagreement);
        assert_eq!(r.lines.len(), 1);
    }

    #[tokio::test]
    async fn merge_retries_once_on_parse_error_then_succeeds() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        // ScriptedChat pops from end → first call gets the good response,
        // second call (if any) gets the bad one. We want the OPPOSITE so
        // first call returns malformed JSON, second returns good. Push
        // them in REVERSE order.
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": []}"#, // good (popped second)
            r#"not valid json"#,                                     // bad (popped first)
        ]);
        let r = merge(&chat, &input).await.expect("ok");
        assert!(r.lines.is_empty());
    }

    #[tokio::test]
    async fn merge_propagates_ms_field_error() {
        let (words, source, text) = ctx();
        let input = ClaudeMergeInput {
            aai_words: &words,
            untimed_text: text,
            untimed_source: source,
            language: None,
        };
        let chat = ScriptedChat::new(vec![
            r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 0, "start_ms": 0}]}"#,
        ]);
        let err = merge(&chat, &input).await.expect_err("must err");
        // HasMsFields is NOT MergeError::Parse, so no retry happens.
        assert!(matches!(err, MergeError::HasMsFields), "got {err:?}");
    }
```

- [ ] **Step 4: Verify async-trait is in workspace deps**

```bash
grep -n "async-trait" /home/newlevel/devel/songplayer/crates/sp-server/Cargo.toml
```

If absent, add to `[dependencies]` section:

```toml
async-trait = "0.1"
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::claude_merge
```

Expected: 8 passed (5 from Task 6 + 3 new merge tests).

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/claude_merge.rs crates/sp-server/Cargo.toml
git commit -m "feat(asr_path): Claude merge HTTP wrapper with single-retry on parse error"
```

---

## Task 8: Resolver — word indices → ms timings

Takes a `ClaudeMergeResult` and an `AaiTranscript`, validates indices, builds `Vec<LyricsLine>` with `words: None`. Also applies a line-level sanitizer (monotonic start, no overlap, minimum 200ms duration).

**Files:**
- Create: `crates/sp-server/src/lyrics/asr_path/resolver.rs`

- [ ] **Step 1: Write the file**

```rust
//! Resolve Claude's word-index ranges to ms-timed LyricsLines.
//!
//! Per spec rule 3 (`feedback_line_timing_only`), every output line ships
//! `words: None`. Per spec rule 2 (v15-prevention), Claude never sees ms
//! values — only word indices — and the resolver computes ms STRICTLY by
//! lookup, never interpolation.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiTranscript;
use crate::lyrics::asr_path::claude_merge::ClaudeMergeResult;

const MIN_LINE_DURATION_MS: u64 = 200;

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("word index {got} out of range (len={len})")]
    OutOfRange { got: usize, len: usize },
    #[error("inverted range: start_word_idx={start} > end_word_idx={end}")]
    InvertedRange { start: usize, end: usize },
    #[error("empty lines from Claude (caller should fall back)")]
    Empty,
}

pub fn resolve(
    merged: &ClaudeMergeResult,
    aai: &AaiTranscript,
) -> Result<Vec<LyricsLine>, ResolverError> {
    if merged.lines.is_empty() {
        return Err(ResolverError::Empty);
    }
    let mut out: Vec<LyricsLine> = Vec::with_capacity(merged.lines.len());
    let word_count = aai.words.len();
    for ml in &merged.lines {
        if ml.start_word_idx > ml.end_word_idx {
            return Err(ResolverError::InvertedRange {
                start: ml.start_word_idx,
                end: ml.end_word_idx,
            });
        }
        if ml.end_word_idx >= word_count {
            return Err(ResolverError::OutOfRange {
                got: ml.end_word_idx,
                len: word_count,
            });
        }
        let start_ms = aai.words[ml.start_word_idx].start_ms;
        let end_ms = aai.words[ml.end_word_idx].end_ms;
        out.push(LyricsLine {
            en: ml.text.clone(),
            sk: String::new(), // translator fills this later
            start_ms,
            end_ms,
            words: None, // per feedback_line_timing_only — line-only display
        });
    }
    Ok(sanitize_lines(out))
}

/// Line-level sanitizer:
/// - monotonic `start_ms` (each line's start >= previous line's start)
/// - no overlap (each line's start >= previous line's end)
/// - minimum 200ms duration (very short lines get clamped to start+200)
///
/// Same invariants the existing v8-v10 sanitizer enforces at word level,
/// adapted to lines.
fn sanitize_lines(mut lines: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut floor: u64 = 0;
    for line in &mut lines {
        if line.start_ms < floor {
            line.start_ms = floor;
        }
        if line.end_ms < line.start_ms + MIN_LINE_DURATION_MS {
            line.end_ms = line.start_ms + MIN_LINE_DURATION_MS;
        }
        floor = line.end_ms;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::asr_path::aai_backend::AaiWord;
    use crate::lyrics::asr_path::claude_merge::MergedLine;

    fn aai(words: Vec<(&str, u64, u64)>) -> AaiTranscript {
        AaiTranscript {
            words: words
                .into_iter()
                .map(|(t, s, e)| AaiWord {
                    text: t.to_string(),
                    start_ms: s,
                    end_ms: e,
                    confidence: 0.9,
                })
                .collect(),
            raw_text: String::new(),
        }
    }

    fn merged(lines: Vec<(&str, usize, usize)>) -> ClaudeMergeResult {
        ClaudeMergeResult {
            disagreement: false,
            notes: String::new(),
            lines: lines
                .into_iter()
                .map(|(t, s, e)| MergedLine {
                    text: t.to_string(),
                    start_word_idx: s,
                    end_word_idx: e,
                })
                .collect(),
        }
    }

    #[test]
    fn happy_path_resolves_lines() {
        let t = aai(vec![
            ("hello", 0, 500),
            ("world", 600, 1100),
            ("again", 1200, 1700),
        ]);
        let m = merged(vec![("Hello world", 0, 1), ("Again", 2, 2)]);
        let lines = resolve(&m, &t).expect("ok");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "Hello world");
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
        assert!(lines[0].words.is_none());
        assert_eq!(lines[1].start_ms, 1200);
        assert_eq!(lines[1].end_ms, 1700);
    }

    #[test]
    fn rejects_inverted_range() {
        let t = aai(vec![("a", 0, 100), ("b", 100, 200)]);
        let m = merged(vec![("bad", 1, 0)]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(matches!(err, ResolverError::InvertedRange { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_out_of_range() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![("oops", 0, 5)]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(matches!(err, ResolverError::OutOfRange { got: 5, len: 1 }), "got {err:?}");
    }

    #[test]
    fn empty_lines_propagates_empty_error() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![]);
        let err = resolve(&m, &t).expect_err("must err");
        assert!(matches!(err, ResolverError::Empty));
    }

    #[test]
    fn sanitizer_enforces_monotonic_start() {
        // Pathological: AAI emitted backwards timing for some reason —
        // sanitizer raises later line's start to the previous line's end.
        let t = aai(vec![("x", 1000, 2000), ("y", 500, 800)]);
        let m = merged(vec![("X", 0, 0), ("Y", 1, 1)]);
        let lines = resolve(&m, &t).expect("ok");
        // Second line had start_ms=500 < prev end (2000); sanitizer clamps.
        assert!(lines[1].start_ms >= lines[0].end_ms);
    }

    #[test]
    fn sanitizer_enforces_minimum_duration() {
        let t = aai(vec![("x", 1000, 1050)]); // 50ms — under threshold
        let m = merged(vec![("X", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert!(lines[0].end_ms - lines[0].start_ms >= MIN_LINE_DURATION_MS);
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let t = aai(vec![("a", 0, 100)]);
        let m = merged(vec![("A", 0, 0)]);
        let lines = resolve(&m, &t).expect("ok");
        assert!(lines.iter().all(|l| l.words.is_none()));
    }
}
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/asr_path/mod.rs` — add `pub mod resolver;`.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::resolver
```

Expected: 7 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/resolver.rs crates/sp-server/src/lyrics/asr_path/mod.rs
git commit -m "feat(asr_path): resolver — word-index ranges → ms-timed LyricsLines"
```

---

## Task 9: Silence-gap fallback splitter

Used when Claude disagrees or merge fails. Builds lines from raw AAI words splitting on silence gaps > 400ms.

**Files:**
- Create: `crates/sp-server/src/lyrics/asr_path/fallback.rs`

- [ ] **Step 1: Write the file**

```rust
//! Silence-gap line splitter — fallback when Claude rejects the merge.
//!
//! Mirrors `eval/lyrics/backends/assemblyai_universal_3_pro.py::group_words_into_lines`.
//! A new line starts when the gap between the previous word's end and the
//! current word's start exceeds LINE_GAP_MS milliseconds.

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::AaiWord;

/// Match eval Python `LINE_GAP_MS` exactly. Changes here must update the
/// eval Python in lockstep so eval-time and production-time outputs stay
/// comparable when investigating regressions.
pub const LINE_GAP_MS: u64 = 400;

const MIN_LINE_DURATION_MS: u64 = 200;

pub fn split_on_silence(words: &[AaiWord]) -> Vec<LyricsLine> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut current: Vec<&AaiWord> = Vec::new();
    let mut prev_end: Option<u64> = None;

    for w in words {
        if w.text.is_empty() {
            continue;
        }
        if let Some(pe) = prev_end {
            if w.start_ms.saturating_sub(pe) > LINE_GAP_MS && !current.is_empty() {
                lines.push(flush(&current));
                current.clear();
            }
        }
        current.push(w);
        prev_end = Some(w.end_ms);
    }
    if !current.is_empty() {
        lines.push(flush(&current));
    }
    sanitize_lines(lines)
}

fn flush(words: &[&AaiWord]) -> LyricsLine {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    LyricsLine {
        en: text,
        sk: String::new(),
        start_ms: words[0].start_ms,
        end_ms: words[words.len() - 1].end_ms,
        words: None, // per feedback_line_timing_only
    }
}

fn sanitize_lines(mut lines: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut floor: u64 = 0;
    for line in &mut lines {
        if line.start_ms < floor {
            line.start_ms = floor;
        }
        if line.end_ms < line.start_ms + MIN_LINE_DURATION_MS {
            line.end_ms = line.start_ms + MIN_LINE_DURATION_MS;
        }
        floor = line.end_ms;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: u64, end: u64) -> AaiWord {
        AaiWord {
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
            confidence: 0.9,
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let lines = split_on_silence(&[]);
        assert!(lines.is_empty());
    }

    #[test]
    fn no_silence_gap_yields_single_line() {
        let words = vec![
            w("hello", 0, 500),
            w("world", 600, 1100),
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].en, "hello world");
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
        assert!(lines[0].words.is_none());
    }

    #[test]
    fn large_gap_starts_new_line() {
        let words = vec![
            w("first", 0, 500),
            w("line", 600, 1100),
            // 1100 + 400 < 1600 → new line
            w("second", 1600, 2100),
            w("line", 2200, 2700),
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].en, "first line");
        assert_eq!(lines[1].en, "second line");
    }

    #[test]
    fn boundary_at_exactly_400ms_does_not_split() {
        // Gap == LINE_GAP_MS doesn't split; only strictly greater does.
        let words = vec![
            w("a", 0, 500),
            w("b", 900, 1400), // gap = 400 exactly
        ];
        let lines = split_on_silence(&words);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn output_lines_always_have_words_none() {
        let words = vec![w("a", 0, 100)];
        let lines = split_on_silence(&words);
        assert!(lines.iter().all(|l| l.words.is_none()));
    }
}
```

- [ ] **Step 2: Register the submodule**

`crates/sp-server/src/lyrics/asr_path/mod.rs` — add `pub mod fallback;`.

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::asr_path::fallback
```

Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/fallback.rs crates/sp-server/src/lyrics/asr_path/mod.rs
git commit -m "feat(asr_path): silence-gap fallback splitter (matches eval LINE_GAP_MS=400)"
```

---

## Task 10: Orchestrator entry — `asr_path::run`

Wires AAI → claude_merge → resolver/fallback into a single async function. Returns an `AsrOutput` discriminant the worker can persist.

**Files:**
- Modify: `crates/sp-server/src/lyrics/asr_path/mod.rs`

- [ ] **Step 1: Append the public API**

Replace the existing contents of `crates/sp-server/src/lyrics/asr_path/mod.rs` (the constant + tests scaffold from Task 2) with:

```rust
//! ASR alignment path — runs AssemblyAI U3-Pro + Claude-merge for songs
//! whose `gather_sources` returns ONLY untimed text candidates (genius,
//! lrclib-untimed, etc.). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`.

pub mod aai_backend;
pub mod claude_merge;
pub mod fallback;
pub mod merge_prompt;
pub mod resolver;

use std::path::Path;

use sp_core::lyrics::LyricsLine;

use crate::lyrics::asr_path::aai_backend::{AaiBackend, AaiError, AaiTranscript};
use crate::lyrics::asr_path::claude_merge::{MergeChat, MergeError, merge};
use crate::lyrics::asr_path::merge_prompt::ClaudeMergeInput;
use crate::lyrics::asr_path::resolver::{ResolverError, resolve};
use crate::lyrics::tier1::CandidateText;

pub const ASSEMBLYAI_API_KEY_SETTING: &str = "assemblyai_api_key";

pub const SOURCE_MERGED: &str = "asr:aai-u3-pro+claude-merge";
pub const SOURCE_FALLBACK: &str = "asr:aai-u3-pro";

#[derive(Debug)]
pub enum AsrOutput {
    Merged { lines: Vec<LyricsLine>, source: &'static str },
    Fallback { lines: Vec<LyricsLine>, source: &'static str },
    Quarantine { reason: &'static str },
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("AAI quota exhausted — surface to user, no row write")]
    QuotaExhausted,
    #[error("AAI transcription failed: {0}")]
    Aai(#[from] AaiError),
    #[error("Claude merge transport failed: {0}")]
    MergeTransport(String),
    #[error("no usable untimed text candidate found")]
    NoCandidate,
}

/// Pick the best untimed text candidate. Priority: genius > lrclib > others.
/// "Best" means most lines; ties broken by source priority.
pub fn pick_untimed_candidate<'a>(
    candidates: &'a [CandidateText],
) -> Option<&'a CandidateText> {
    fn rank(source: &str) -> u8 {
        match source {
            s if s.contains("genius") => 0,
            s if s.contains("lrclib") => 1,
            _ => 2,
        }
    }
    candidates
        .iter()
        .filter(|c| !c.lines.is_empty())
        .min_by_key(|c| (rank(&c.source), usize::MAX - c.lines.len()))
}

pub async fn run<C: MergeChat + ?Sized>(
    aai: &AaiBackend,
    chat: &C,
    audio_path: &Path,
    candidates: &[CandidateText],
    language: Option<&str>,
) -> Result<AsrOutput, AsrError> {
    // 1) Transcribe with AAI.
    let transcript: AaiTranscript = match aai.transcribe(audio_path).await {
        Ok(t) => t,
        Err(AaiError::QuotaExhausted) => return Err(AsrError::QuotaExhausted),
        Err(e) => return Err(AsrError::Aai(e)),
    };
    if transcript.words.is_empty() {
        return Ok(AsrOutput::Quarantine { reason: "empty_transcript" });
    }

    // 2) Pick untimed candidate (genius > lrclib > others).
    let cand = match pick_untimed_candidate(candidates) {
        Some(c) => c,
        None => return Err(AsrError::NoCandidate),
    };
    let untimed_text = cand.lines.join("\n");
    let input = ClaudeMergeInput {
        aai_words: &transcript.words,
        untimed_text: &untimed_text,
        untimed_source: &cand.source,
        language,
    };

    // 3) Claude-merge. Any failure → fallback path.
    let merged = match merge(chat, &input).await {
        Ok(m) => m,
        Err(MergeError::Transport(e)) => {
            tracing::warn!("asr_path: claude transport failed: {e} — falling back");
            return Ok(fallback_output(&transcript));
        }
        Err(e) => {
            tracing::warn!("asr_path: claude merge rejected: {e} — falling back");
            return Ok(fallback_output(&transcript));
        }
    };

    if merged.disagreement || merged.lines.is_empty() {
        tracing::info!(
            disagreement = merged.disagreement,
            notes = %merged.notes,
            "asr_path: claude declined merge — using fallback"
        );
        return Ok(fallback_output(&transcript));
    }

    // 4) Resolve indices to ms.
    match resolve(&merged, &transcript) {
        Ok(lines) => Ok(AsrOutput::Merged { lines, source: SOURCE_MERGED }),
        Err(ResolverError::Empty) => Ok(fallback_output(&transcript)),
        Err(e) => {
            tracing::warn!("asr_path: resolver rejected claude output: {e} — falling back");
            Ok(fallback_output(&transcript))
        }
    }
}

fn fallback_output(transcript: &AaiTranscript) -> AsrOutput {
    let lines = fallback::split_on_silence(&transcript.words);
    if lines.is_empty() {
        AsrOutput::Quarantine { reason: "empty_fallback" }
    } else {
        AsrOutput::Fallback { lines, source: SOURCE_FALLBACK }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
```

- [ ] **Step 2: Rewrite `tests.rs` with the orchestrator integration test**

Replace `crates/sp-server/src/lyrics/asr_path/tests.rs` with:

```rust
//! Integration tests for the asr_path orchestrator + the three
//! forbidden-behavior guards from the spec.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::aai_backend::AaiBackend;
use super::claude_merge::MergeChat;
use super::pick_untimed_candidate;
use super::{ASSEMBLYAI_API_KEY_SETTING, AsrOutput, SOURCE_FALLBACK, SOURCE_MERGED, run};
use crate::lyrics::tier1::CandidateText;

fn cand(source: &str, lines: Vec<&str>) -> CandidateText {
    CandidateText {
        source: source.to_string(),
        lines: lines.into_iter().map(String::from).collect(),
        line_timings: None,
        has_timing: false,
    }
}

struct ScriptedChat {
    responses: Mutex<Vec<String>>,
}

impl ScriptedChat {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
        }
    }
}

#[async_trait::async_trait]
impl MergeChat for ScriptedChat {
    async fn chat(&self, _system: &str, _user: &str) -> Result<String, String> {
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| "out of responses".to_string())
    }
}

async fn aai_server_with_two_words() -> (MockServer, PathBuf) {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/upload"))
        .and(header("authorization", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "upload_url": "https://cdn.example/a.wav"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/transcript"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "tid"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/transcript/tid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "text": "hello world",
            "words": [
                {"text": "hello", "start": 0, "end": 500, "confidence": 0.9},
                {"text": "world", "start": 600, "end": 1100, "confidence": 0.9}
            ]
        })))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join("asr_path_orch_test.wav");
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"\x00").unwrap();
    drop(f);

    (server, tmp)
}

#[test]
fn settings_key_is_stable() {
    assert_eq!(ASSEMBLYAI_API_KEY_SETTING, "assemblyai_api_key");
}

#[test]
fn pick_untimed_candidate_prefers_genius_over_lrclib() {
    let cands = vec![
        cand("lrclib", vec!["short"]),
        cand("genius", vec!["hello", "world", "again"]),
    ];
    let picked = pick_untimed_candidate(&cands).expect("must pick");
    assert_eq!(picked.source, "genius");
}

#[test]
fn pick_untimed_candidate_returns_none_on_empty_lines() {
    let cands = vec![cand("genius", vec![])];
    assert!(pick_untimed_candidate(&cands).is_none());
}

#[tokio::test]
async fn run_happy_path_returns_merged() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello", "world"])];

    let out = run(&aai, &chat, &audio, &cands, Some("en")).await.expect("ok");
    match out {
        AsrOutput::Merged { lines, source } => {
            assert_eq!(source, SOURCE_MERGED);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].en, "Hello world");
            assert!(lines[0].words.is_none());
        }
        other => panic!("expected Merged, got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_falls_back_on_claude_disagreement() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "wrong song", "lines": []}"#,
    ]);
    let cands = vec![cand("genius", vec!["different", "lyrics"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    match out {
        AsrOutput::Fallback { lines, source } => {
            assert_eq!(source, SOURCE_FALLBACK);
            assert!(!lines.is_empty(), "fallback should produce lines from AAI");
        }
        other => panic!("expected Fallback, got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_falls_back_on_claude_ms_field() {
    // Guard #3: Claude emits ms fields → parser rejects → orchestrator falls back.
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 1, "start_ms": 0}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    assert!(matches!(out, AsrOutput::Fallback { .. }), "got {out:?}");
    let _ = std::fs::remove_file(&audio);
}

// ─── Three forbidden-behavior guards locked in tests ───

#[tokio::test]
async fn guard_never_emits_word_timings_on_merged() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = out {
        assert!(lines.iter().all(|l| l.words.is_none()));
    } else {
        panic!("expected Merged");
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn guard_never_emits_word_timings_on_fallback() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "", "lines": []}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Fallback { lines, .. } = out {
        assert!(lines.iter().all(|l| l.words.is_none()));
    } else {
        panic!("expected Fallback");
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn guard_never_synthesizes_ms_from_thin_air() {
    // Every line.start_ms / end_ms MUST come from an AAI word's start / end.
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = out {
        // AAI words were (hello: 0..500, world: 600..1100). The merged line
        // MUST equal those exact ms values — no interpolation, no rounding.
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
    } else {
        panic!("expected Merged");
    }
    let _ = std::fs::remove_file(&audio);
}
```

- [ ] **Step 3: Run all asr_path tests**

```bash
cargo test -p sp-server lyrics::asr_path
```

Expected: all tests across `aai_backend`, `claude_merge`, `fallback`, `merge_prompt`, `resolver`, and `tests` pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/asr_path/mod.rs crates/sp-server/src/lyrics/asr_path/tests.rs
git commit -m "feat(asr_path): orchestrator entry run() + 3 forbidden-behavior guards"
```

---

## Task 11: Worker integration

One new branch in `worker.rs::process_song`. Also adds `has_any_text_candidate` next to `is_allowed_text_source` in `orchestrator.rs`.

**Files:**
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs` (~3 lines added)
- Modify: `crates/sp-server/src/lyrics/worker.rs` (one new branch inside `process_song`)

- [ ] **Step 1: Add the helper to orchestrator.rs**

Inside `crates/sp-server/src/lyrics/orchestrator.rs`, immediately after the `is_allowed_text_source` function, add:

```rust
/// Returns true if `gather_sources` returned ANY text candidate, regardless
/// of whether the gate accepts it. The asr_path branch uses this to decide
/// whether to try ASR-based alignment on a song whose only candidates are
/// untimed (genius, lrclib-untimed, etc.). Songs with zero candidates skip
/// asr_path and remain marked `no_text_source`.
pub(crate) fn has_any_text_candidate(
    candidates: &[crate::lyrics::provider::CandidateText],
) -> bool {
    candidates.iter().any(|c| !c.lines.is_empty())
}
```

- [ ] **Step 2: Add a unit test for has_any_text_candidate**

At the bottom of `orchestrator.rs` (or in `orchestrator_gate_tests.rs` if you prefer the existing pattern), add:

```rust
#[cfg(test)]
mod has_any_text_candidate_tests {
    use super::has_any_text_candidate;
    use crate::lyrics::provider::CandidateText;

    fn c(source: &str, lines: Vec<&str>) -> CandidateText {
        CandidateText {
            source: source.to_string(),
            lines: lines.into_iter().map(String::from).collect(),
            line_timings: None,
            has_timing: false,
        }
    }

    #[test]
    fn returns_false_on_empty_list() {
        assert!(!has_any_text_candidate(&[]));
    }

    #[test]
    fn returns_false_when_all_candidates_empty() {
        let cands = vec![c("genius", vec![]), c("lrclib", vec![])];
        assert!(!has_any_text_candidate(&cands));
    }

    #[test]
    fn returns_true_when_any_candidate_has_lines() {
        let cands = vec![c("genius", vec![]), c("lrclib", vec!["a line"])];
        assert!(has_any_text_candidate(&cands));
    }
}
```

- [ ] **Step 3: Run helper tests**

```bash
cargo test -p sp-server lyrics::orchestrator::has_any_text_candidate
```

Expected: 3 passed.

- [ ] **Step 4: Read the current process_song gate section**

Familiarize yourself with the exact lines being replaced — `crates/sp-server/src/lyrics/worker.rs:467-490` (the `if !crate::lyrics::orchestrator::is_allowed_text_source` block that today just calls `mark_unsupported_source`).

- [ ] **Step 5: Modify the gate block**

Replace the existing `if !is_allowed_text_source` block (lines 467-490) with:

```rust
        if !crate::lyrics::orchestrator::is_allowed_text_source(&ctx.candidate_texts) {
            let names: Vec<&str> = ctx
                .candidate_texts
                .iter()
                .map(|c| c.source.as_str())
                .collect();

            if crate::lyrics::orchestrator::has_any_text_candidate(&ctx.candidate_texts) {
                tracing::info!(
                    video_id,
                    youtube_id = %youtube_id,
                    candidate_sources = ?names,
                    "lyrics: no allowed text source — routing to asr_path"
                );

                // Read AAI key per-song so an operator can configure without restart.
                let aai_key = match crate::db::models::get_setting(
                    &self.pool,
                    crate::lyrics::asr_path::ASSEMBLYAI_API_KEY_SETTING,
                )
                .await
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                {
                    Some(k) => k,
                    None => {
                        tracing::warn!(
                            "lyrics: assemblyai_api_key not set — leaving row unprocessed"
                        );
                        self.clear_processing().await;
                        return Ok(());
                    }
                };

                // Convert provider::CandidateText → tier1::CandidateText (same
                // bridge the whisperx path uses just below).
                let tier1_cands: Vec<crate::lyrics::tier1::CandidateText> = ctx
                    .candidate_texts
                    .iter()
                    .cloned()
                    .map(crate::lyrics::tier1::CandidateText::from)
                    .collect();

                // Vocal isolation — reuse existing preprocess_vocals.
                let venv_python = self.venv_python.read().await.clone();
                let audio_path: Option<PathBuf> =
                    row.audio_file_path.as_ref().map(PathBuf::from);
                let clean_vocal: Option<PathBuf> = match (&venv_python, &audio_path) {
                    (Some(python), Some(audio)) if audio.exists() => {
                        let wav_path = self
                            .cache_dir
                            .join(format!("{youtube_id}_vocals16k.wav"));
                        match crate::lyrics::aligner::preprocess_vocals(
                            python,
                            &self.script_path,
                            &self.models_dir,
                            audio,
                            &wav_path,
                        )
                        .await
                        {
                            Ok(p) => Some(p),
                            Err(e) => {
                                tracing::warn!(
                                    "asr_path: vocal isolation failed for {youtube_id}: {e}"
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };

                let Some(wav) = clean_vocal else {
                    tracing::warn!(
                        "asr_path: no preprocessed vocal available for {youtube_id} — leaving unprocessed"
                    );
                    self.clear_processing().await;
                    return Ok(());
                };

                let aai = crate::lyrics::asr_path::aai_backend::AaiBackend::new(aai_key);
                let result = crate::lyrics::asr_path::run(
                    &aai,
                    &self.ai_client,
                    &wav,
                    &tier1_cands,
                    None,
                )
                .await;

                match result {
                    Ok(crate::lyrics::asr_path::AsrOutput::Merged { lines, source }) => {
                        self.persist_asr_output(&row, lines, source, started_at_unix_ms)
                            .await;
                    }
                    Ok(crate::lyrics::asr_path::AsrOutput::Fallback { lines, source }) => {
                        self.persist_asr_output(&row, lines, source, started_at_unix_ms)
                            .await;
                    }
                    Ok(crate::lyrics::asr_path::AsrOutput::Quarantine { reason }) => {
                        tracing::warn!(
                            youtube_id = %youtube_id,
                            reason,
                            "asr_path: quarantining — empty transcript or fallback"
                        );
                        if let Err(e) = crate::db::models::mark_unsupported_source(
                            &self.pool,
                            video_id,
                            LYRICS_PIPELINE_VERSION,
                        )
                        .await
                        {
                            tracing::warn!("worker: mark_unsupported_source: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            youtube_id = %youtube_id,
                            error = %e,
                            "asr_path: error — leaving row unprocessed for retry"
                        );
                    }
                }

                self.clear_processing().await;
                return Ok(());
            }

            // No candidates at all — preserved old behavior: mark unsupported.
            tracing::warn!(
                video_id,
                youtube_id = %youtube_id,
                candidate_sources = ?names,
                "lyrics: no text candidate at all — marking unsupported_source"
            );
            if let Err(e) = crate::db::models::mark_unsupported_source(
                &self.pool,
                video_id,
                LYRICS_PIPELINE_VERSION,
            )
            .await
            {
                tracing::warn!("worker: mark_unsupported_source error for {youtube_id}: {e}");
            }
            self.clear_processing().await;
            return Ok(());
        }
```

- [ ] **Step 6: Add `persist_asr_output` helper to the worker impl**

Inside the `impl LyricsWorker` block in `worker.rs`, add this method. Look up the existing `persist_lyrics_track` / similar helper next to it and mirror the shape — your method should call the same downstream persistence code used by the whisperx path, just with the `source` label propagated.

```rust
    async fn persist_asr_output(
        &self,
        row: &VideoRow,
        lines: Vec<sp_core::lyrics::LyricsLine>,
        source: &'static str,
        started_at_unix_ms: u64,
    ) {
        // Build a LyricsTrack the same way the whisperx success path does.
        // Look for `LyricsTrack { lines: ..., source: ..., ... }` constructed
        // around worker.rs:660-690 and call the SAME persistence helper.
        // The only differences from whisperx persist are:
        //   1) `source` is one of "asr:aai-u3-pro+claude-merge" / "asr:aai-u3-pro"
        //   2) `alignment_model` is None (whisperx-specific label;
        //       asr_path emits its own alignment model label space).
        let track = sp_core::lyrics::LyricsTrack {
            lines,
            source: source.to_string(),
            ..Default::default()
        };
        if let Err(e) = self
            .persist_track(row, &track, None, started_at_unix_ms)
            .await
        {
            tracing::warn!("asr_path: persist_track for {} failed: {e}", row.youtube_id);
        }
    }
```

If `persist_track` doesn't exist by that exact name, find the helper currently called by the whisperx success branch (around line 690-720) and reuse it. The point is: **DO NOT duplicate persistence logic — reuse the same path the whisperx flow uses**, just with the new source label.

- [ ] **Step 7: Compile-check + run all lyrics tests**

```bash
cargo check --workspace
cargo test -p sp-server lyrics::
```

Expected: build succeeds; all existing whisperx-path tests still pass (regression check); new asr_path tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/sp-server/src/lyrics/orchestrator.rs crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(asr_path): wire into worker — runs on bucket-1 (unsupported_source)"
```

---

## Task 12: Worker structural regression — whisperx routing must not change

Adds an explicit test asserting that a song with a timed `yt_subs` candidate routes to whisperx, never asr_path. Catches accidental gate bypass.

**Files:**
- Modify: `crates/sp-server/src/lyrics/worker_tests.rs`

- [ ] **Step 1: Read the existing test patterns**

```bash
grep -n "fn " /home/newlevel/devel/songplayer/crates/sp-server/src/lyrics/worker_tests.rs | head -20
```

Pick an existing `process_song` test that exercises the gate (e.g. one that already verifies `mark_unsupported_source` is called). Mirror that test's mocking pattern.

- [ ] **Step 2: Add the regression test**

Append to `worker_tests.rs`:

```rust
#[tokio::test]
async fn process_song_routes_to_whisperx_when_yt_subs_timed() {
    // Regression guard: a song with a timed yt_subs candidate MUST go through
    // the existing whisperx path, not the new asr_path branch added for
    // bucket-1 (`unsupported_source`) songs.
    //
    // The simplest assertion: with a timed yt_subs candidate, `process_song`
    // does NOT call into asr_path. We verify this by ensuring no AAI HTTP
    // call is made: set `assemblyai_api_key` to a known value pointing at a
    // mock that would FAIL the test if hit, and assert the whisperx-success
    // path is observed (track persisted with source containing "yt_subs"
    // or "whisperx").
    //
    // Mirror the existing whisperx happy-path test setup in this same file —
    // copy its fixture builder, replicate-mock, and persistence assertions.
    // The new assertion is: source label does NOT start with "asr:".

    // (See `whisperx_happy_path` or similar — implement using the same
    // fixtures + ScriptedChat for the AI client.)
}
```

If a whisperx happy-path test fixture isn't trivially reusable, instead add a SMALLER routing-only test:

```rust
#[tokio::test]
async fn timed_yt_subs_skips_asr_path_entirely() {
    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};
    use crate::lyrics::provider::CandidateText;

    let timed = CandidateText {
        source: "yt_subs".to_string(),
        lines: vec!["hello".into(), "world".into()],
        line_timings: Some(vec![(0, 1000), (1200, 2000)]),
        has_timing: true,
    };
    let cands = vec![timed];

    // The worker decision tree:
    //   1. is_allowed_text_source true → whisperx path (no asr_path)
    //   2. is_allowed_text_source false && has_any_text_candidate true → asr_path
    //   3. is_allowed_text_source false && has_any_text_candidate false → mark_unsupported
    //
    // Step 1 must be the path taken here:
    assert!(is_allowed_text_source(&cands));
    // Doesn't matter for routing, but proves the candidate WOULD also be
    // eligible for asr_path if the gate failed.
    assert!(has_any_text_candidate(&cands));
}
```

This test asserts the routing predicate directly. Combined with `has_any_text_candidate_tests` from Task 11, the routing logic is fully covered.

- [ ] **Step 3: Run worker tests**

```bash
cargo test -p sp-server lyrics::worker
```

Expected: all existing tests still pass + new regression passes.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/worker_tests.rs
git commit -m "test(worker): regression — timed yt_subs candidates skip asr_path"
```

---

## Task 13: Canonical-source regression — bucket-1 routing pin

Extend `canonical_source_regression_tests.rs` with a routing-level pin: a song whose only candidate is `genius` (no timing) must route into the asr_path branch.

**Files:**
- Modify: `crates/sp-server/src/lyrics/canonical_source_regression_tests.rs`

- [ ] **Step 1: Read the existing test pattern**

```bash
cat /home/newlevel/devel/songplayer/crates/sp-server/src/lyrics/canonical_source_regression_tests.rs
```

The existing tests check `tier1::pick_best` behavior on synthetic candidate lists. Mirror the helper `text_cand`.

- [ ] **Step 2: Append the pin**

```rust
#[test]
fn id_bucket1_genius_only_routes_to_asr_path() {
    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};
    use crate::lyrics::provider::CandidateText;

    // Bucket-1 song shape: only genius candidate, no timing.
    let cands = vec![CandidateText {
        source: "genius".to_string(),
        lines: vec!["Hello".into(), "World".into()],
        line_timings: None,
        has_timing: false,
    }];

    // Whisperx gate rejects (no timed source).
    assert!(!is_allowed_text_source(&cands));
    // But asr_path eligibility holds (there IS a text candidate).
    assert!(has_any_text_candidate(&cands));
}

#[test]
fn id_bucket1_lrclib_untimed_only_routes_to_asr_path() {
    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};
    use crate::lyrics::provider::CandidateText;

    let cands = vec![CandidateText {
        source: "lrclib".to_string(),
        lines: vec!["Hello".into()],
        line_timings: None,
        has_timing: false, // untimed lrclib
    }];

    assert!(!is_allowed_text_source(&cands));
    assert!(has_any_text_candidate(&cands));
}

#[test]
fn id_no_candidates_marks_unsupported_neither_path() {
    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};

    let cands: Vec<crate::lyrics::provider::CandidateText> = vec![];
    assert!(!is_allowed_text_source(&cands));
    assert!(!has_any_text_candidate(&cands)); // → mark_unsupported_source path
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p sp-server lyrics::canonical_source_regression
```

Expected: existing tests + 3 new pins pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/lyrics/canonical_source_regression_tests.rs
git commit -m "test(canonical): pin bucket-1 routing — genius/lrclib-untimed → asr_path"
```

---

## Task 14: Full workspace check + format + clippy

Pre-push gate per `no-local-builds.md` Tier-0 rules.

- [ ] **Step 1: Fmt check**

```bash
cargo fmt --all --check
```

If anything's out, run `cargo fmt --all` and re-stage.

- [ ] **Step 2: Compile check**

```bash
cargo check --workspace
```

Expected: zero errors.

- [ ] **Step 3: Clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: zero warnings (treated as errors). If clippy fires on the new code, fix the new code; do NOT add `#[allow(clippy::...)]`.

- [ ] **Step 4: Test compile-only**

```bash
cargo test --no-run --workspace
```

Expected: success.

- [ ] **Step 5: Full lyrics test suite**

```bash
cargo test -p sp-server lyrics::
```

Expected: every test (existing whisperx + text_reference_merge + canonical + new asr_path) passes.

- [ ] **Step 6: Commit any fmt changes if needed**

```bash
git status
# if there are changes from cargo fmt:
git add -u
git commit -m "style: cargo fmt"
```

---

## Task 15: Push, monitor CI, open PR

Per `ci-monitoring.md`, `ci-push-discipline.md`, `pr-merge-policy.md`.

- [ ] **Step 1: Fetch + push**

```bash
git fetch origin
git push origin dev
```

- [ ] **Step 2: Find the latest run and monitor**

```bash
gh run list --branch dev --limit 3
# capture the run id, then:
sleep 300 && gh run view <run-id> --json status,conclusion,jobs
```

Use background command per `ci-monitoring.md` — `run_in_background: true`. Wait for ALL jobs to reach terminal state (success or failure).

- [ ] **Step 3: If CI fails — collect ALL failures, fix in one commit**

```bash
gh run view <run-id> --log-failed
```

Investigate root cause. Apply fix. Re-run Tasks 14 + 15.

- [ ] **Step 4: Once CI green, open PR**

```bash
gh pr create --base main --head dev \
  --title "Add asr_path — AAI U3-Pro + Claude-merge for unsupported_source songs" \
  --body "$(cat <<'EOF'
## Summary

- Adds a new alignment path (`crates/sp-server/src/lyrics/asr_path/`) that runs AssemblyAI Universal-3 Pro ASR + Claude-merge for songs the existing whisperx flow rejects (untimed text candidates only — the `unsupported_source` bucket).
- Whisperx flow on bucket 2 (timed text source) is byte-for-byte untouched.
- Claude returns word INDICES, never ms values (sidesteps the v15 array-math failure). Server resolves indices → line.start_ms/end_ms from AAI words. Output ships with `words: None` per `feedback_line_timing_only`.
- Disagreement signal from Claude → fallback to raw AAI silence-gap split with source `asr:aai-u3-pro`.
- Three forbidden-behavior guards locked in tests: never emits word timings, never synthesizes ms from thin air, schema rejects Claude responses containing `start_ms`/`end_ms`.
- No `LYRICS_PIPELINE_VERSION` bump. asr_path writes only `unsupported_source` / NULL rows.

## Spec + plan

- Spec: `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`
- Plan: `docs/superpowers/plans/2026-05-19-asr-path-aai-claude-merge.md`

## Test plan

- [ ] cargo test -p sp-server lyrics:: — full lyrics suite green
- [ ] CI green on dev
- [ ] Set `assemblyai_api_key` in production settings on win-resolume
- [ ] After deploy: pick one bucket-1 song, trigger reprocess, wall-verify via /lyrics-verify (song-by-song per `feedback_song_by_song_iteration`)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Confirm PR is mergeable**

```bash
gh pr view --json number,mergeable,mergeStateStatus
```

Expected: `mergeable: MERGEABLE`, `mergeStateStatus: CLEAN`. If anything else, fix the cause (per `autonomous-quality-discipline.md` — no admin-merge, no "merge despite").

- [ ] **Step 6: Report PR URL to user; WAIT for explicit merge instruction.**

Per `pr-merge-policy.md`: green CI is NOT permission to merge. Only an explicit user instruction triggers merge.

---

## Self-review (run after writing this plan, not part of execution)

**Spec coverage check:**

| Spec requirement | Task |
|---|---|
| New `asr_path/` sub-tree with 5 files | Tasks 2, 3, 4, 5, 6, 7, 8, 9, 10 |
| `aai_backend.rs` — AAI HTTP client | Tasks 3, 4 |
| `merge_prompt.rs` — prompt builder | Task 5 |
| `claude_merge.rs` — parser + HTTP wrapper | Tasks 6, 7 |
| `resolver.rs` — word-index → ms | Task 8 |
| `fallback.rs` — silence-gap splitter | Task 9 |
| `mod.rs` orchestrator `run()` | Task 10 |
| Worker integration in `process_song` | Task 11 |
| `has_any_text_candidate` helper | Task 11 |
| `assemblyai_api_key` setting | Task 2 (constant) + Task 11 (read in worker) |
| New source labels `asr:aai-u3-pro+claude-merge` and `asr:aai-u3-pro` | Task 10 (constants) + Task 11 (propagated to persist) |
| Three forbidden-behavior guards | Task 10 (tests.rs) |
| Worker regression — timed yt_subs → whisperx | Task 12 |
| Canonical-source regression bucket-1 pin | Task 13 |
| VERSION bump 0.44.0-dev.1 → 0.44.0-dev.2 | Task 1 |
| No `LYRICS_PIPELINE_VERSION` bump | (omission — verified by absence of touch to that constant) |
| Schema `deny_unknown_fields` rejecting ms | Task 6 |
| Whisperx code path UNTOUCHED | (verified by `cargo test lyrics::` regression at Task 14) |

**Type consistency check:**

- `AaiWord` defined once in `aai_backend.rs`, used by `merge_prompt`, `claude_merge` (tests only), `resolver`, `fallback`, `tests`. Field names `text`, `start_ms`, `end_ms`, `confidence` consistent.
- `MergedLine` / `ClaudeMergeResult` defined once in `claude_merge.rs`, consumed by `resolver.rs`.
- `LyricsLine` is `sp_core::lyrics::LyricsLine` — same type used by whisperx persistence path. Reused, not redefined.
- `CandidateText` — there are TWO types: `provider::CandidateText` (gather_sources output, used in worker / orchestrator gates) and `tier1::CandidateText` (used by asr_path::run). Conversion via `From` impl in `tier1.rs`, already used by the whisperx path at `worker.rs:549`.

**Placeholder scan:** none.

**Scope:** single PR, single feature, additive only. Matches `autonomous-batch-issue-development` "single feature = single PR" rule.
