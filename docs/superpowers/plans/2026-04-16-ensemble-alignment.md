# Ensemble Alignment Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single-source lyrics alignment pipeline with a pluggable ensemble that runs multiple providers independently, merges their word-timing estimates via Claude Opus LLM, and ratchets quality up as new providers are added.

**Architecture:** New `ai/` module embeds CLIProxyAPI (ported from presenter) as the unified AI gateway. New `lyrics/provider.rs` defines the `AlignmentProvider` trait. New `lyrics/merge.rs` sends all provider results to Claude Opus for intelligent word matching. Existing Qwen3 pipeline refactored as the first provider. `lyrics/orchestrator.rs` replaces the monolithic `process_song` in `worker.rs`.

**Tech Stack:** Rust 2024, async_trait, reqwest, serde_json, CLIProxyAPI (Go binary), Claude Opus via OpenAI-compatible API, Gemini 3.1 Pro for audio transcription.

**Spec:** `docs/superpowers/specs/2026-04-16-ensemble-alignment-design.md`

---

## File Structure

### New files

| Path | Purpose |
|------|---------|
| `crates/sp-server/src/ai/mod.rs` | AI module: settings, exports |
| `crates/sp-server/src/ai/proxy.rs` | CLIProxyAPI process manager (port from presenter) |
| `crates/sp-server/src/ai/client.rs` | OpenAI-compatible HTTP client |
| `crates/sp-server/src/lyrics/provider.rs` | `AlignmentProvider` trait + types (WordTiming, LineTiming, ProviderResult, SongContext) |
| `crates/sp-server/src/lyrics/merge.rs` | LLM-powered merge: prompt construction, response parsing, audit log |
| `crates/sp-server/src/lyrics/orchestrator.rs` | Per-song pipeline: gather → align → merge → translate → quality gate |
| `crates/sp-server/src/api/ai.rs` | HTTP endpoints for proxy management |

### Modified files

| Path | Change |
|------|--------|
| `crates/sp-server/src/lib.rs` | Add `pub mod ai`, wire proxy + orchestrator startup |
| `crates/sp-server/src/lyrics/mod.rs` | Add provider, merge, orchestrator exports |
| `crates/sp-server/src/lyrics/worker.rs` | Delegate to orchestrator instead of direct Qwen3 calls |
| `crates/sp-server/src/lyrics/aligner.rs` | Refactor as `Qwen3Provider` implementing `AlignmentProvider` |
| `crates/sp-server/src/lyrics/translator.rs` | Add Claude Opus path alongside existing Gemini |
| `crates/sp-server/src/metadata/gemini.rs` | Add Claude Opus path alongside existing Gemini |
| `crates/sp-core/src/config.rs` | Add AI settings constants, update DEFAULT_GEMINI_MODEL |
| `crates/sp-server/Cargo.toml` | Add async-trait dependency |
| `src-tauri/tauri.conf.json` | Bundle CLIProxyAPI binary in resources |

---

## Task 1: Core types — provider interface and data model

**Files:**
- Create: `crates/sp-server/src/lyrics/provider.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`
- Modify: `crates/sp-server/Cargo.toml`

This task defines all shared types that every subsequent task depends on. No I/O, no async — pure type definitions + unit tests for serialization.

- [ ] **Step 1: Add async-trait dependency**

Add to `crates/sp-server/Cargo.toml` under `[dependencies]`:
```toml
async-trait = "0.1"
```

- [ ] **Step 2: Create `provider.rs` with all core types**

```rust
//! Ensemble alignment provider interface and shared types.
//!
//! Every alignment source (Qwen3, WhisperX, auto-subs, Gemini audio, etc.)
//! implements `AlignmentProvider` and produces `ProviderResult`. The merge
//! layer consumes these results and emits a unified `LyricsTrack`.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Provider output types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WordTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LineTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub words: Vec<WordTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResult {
    pub provider_name: String,
    pub lines: Vec<LineTiming>,
    /// Provider-specific metadata preserved for audit trail.
    pub metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Song context (shared input to all providers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CandidateText {
    pub source: String,
    pub lines: Vec<String>,
    pub has_timing: bool,
    pub line_timings: Option<Vec<(u64, u64)>>,
}

#[derive(Debug, Clone)]
pub struct SongContext {
    pub video_id: String,
    pub audio_path: PathBuf,
    pub clean_vocal_path: Option<PathBuf>,
    pub candidate_texts: Vec<CandidateText>,
    pub autosub_json3: Option<PathBuf>,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Merge output types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedWordTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
    pub source_count: u8,
    pub spread_ms: u32,
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WordMergeDetail {
    pub word_index: usize,
    pub reference_text: String,
    pub provider_estimates: Vec<(String, u64, f32)>,
    pub outliers_rejected: Vec<(String, u64)>,
    pub merged_start_ms: u64,
    pub merged_confidence: f32,
    pub spread_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub avg_confidence: f32,
    pub words_with_zero_timing: usize,
    pub duplicate_start_pct: f32,
    pub gap_stddev_ms: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub video_id: String,
    pub timestamp: String,
    pub reference_text_source: String,
    pub providers_run: Vec<String>,
    pub providers_skipped: Vec<(String, String)>,
    pub per_word_details: Vec<WordMergeDetail>,
    pub quality_metrics: QualityMetrics,
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait AlignmentProvider: Send + Sync {
    /// Unique name for logging and audit trail.
    fn name(&self) -> &str;

    /// Static base confidence weight (0.0–1.0).
    fn base_confidence(&self) -> f32;

    /// Cheap pre-check: can this provider produce results for this song?
    async fn can_provide(&self, ctx: &SongContext) -> bool;

    /// Run alignment independently. Returns word-timed lines.
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult>;
}
```

- [ ] **Step 3: Add tests for serialization roundtrip**

Add at the bottom of `provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_timing_serde_roundtrip() {
        let wt = WordTiming {
            text: "praise".into(),
            start_ms: 29510,
            end_ms: 30000,
            confidence: 0.9,
        };
        let json = serde_json::to_string(&wt).unwrap();
        let parsed: WordTiming = serde_json::from_str(&json).unwrap();
        assert_eq!(wt, parsed);
    }

    #[test]
    fn provider_result_serde_roundtrip() {
        let pr = ProviderResult {
            provider_name: "qwen3".into(),
            lines: vec![LineTiming {
                text: "Let's get this party started".into(),
                start_ms: 6796,
                end_ms: 8796,
                words: vec![
                    WordTiming { text: "Let's".into(), start_ms: 6936, end_ms: 7256, confidence: 0.9 },
                    WordTiming { text: "get".into(), start_ms: 7256, end_ms: 7496, confidence: 0.9 },
                ],
            }],
            metadata: serde_json::json!({"source": "yt_subs+qwen3"}),
        };
        let json = serde_json::to_string(&pr).unwrap();
        let parsed: ProviderResult = serde_json::from_str(&json).unwrap();
        assert_eq!(pr.provider_name, parsed.provider_name);
        assert_eq!(pr.lines.len(), parsed.lines.len());
        assert_eq!(pr.lines[0].words.len(), parsed.lines[0].words.len());
    }

    #[test]
    fn audit_log_serde_roundtrip() {
        let log = AuditLog {
            video_id: "VtHoABitbpw".into(),
            timestamp: "2026-04-16T12:00:00Z".into(),
            reference_text_source: "manual_subs".into(),
            providers_run: vec!["qwen3".into(), "autosub".into()],
            providers_skipped: vec![("whisperx".into(), "not installed".into())],
            per_word_details: vec![WordMergeDetail {
                word_index: 0,
                reference_text: "praise".into(),
                provider_estimates: vec![
                    ("qwen3".into(), 29510, 0.9),
                    ("autosub".into(), 29530, 0.6),
                ],
                outliers_rejected: vec![],
                merged_start_ms: 29517,
                merged_confidence: 0.92,
                spread_ms: 20,
            }],
            quality_metrics: QualityMetrics {
                avg_confidence: 0.87,
                words_with_zero_timing: 2,
                duplicate_start_pct: 3.5,
                gap_stddev_ms: 450.0,
            },
        };
        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: AuditLog = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.video_id, "VtHoABitbpw");
        assert_eq!(parsed.per_word_details.len(), 1);
        assert_eq!(parsed.per_word_details[0].merged_start_ms, 29517);
    }
}
```

- [ ] **Step 4: Register module in lyrics/mod.rs**

Add to `crates/sp-server/src/lyrics/mod.rs`:
```rust
pub mod provider;
```

- [ ] **Step 5: Verify compilation and tests**

Run:
```bash
cargo test -p sp-server -- provider::tests
```
Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lyrics/provider.rs crates/sp-server/src/lyrics/mod.rs crates/sp-server/Cargo.toml
git commit -m "feat(lyrics): add AlignmentProvider trait and ensemble data model (#29)

Defines the core types for the ensemble alignment pipeline:
WordTiming, LineTiming, ProviderResult, SongContext, MergedWordTiming,
AuditLog, QualityMetrics, and the AlignmentProvider async trait."
```

---

## Task 2: AI module — CLIProxyAPI process manager

**Files:**
- Create: `crates/sp-server/src/ai/mod.rs`
- Create: `crates/sp-server/src/ai/proxy.rs`
- Modify: `crates/sp-server/src/lib.rs`

Port the CLIProxyAPI process manager from presenter (`crates/presenter-server/src/ai/proxy.rs`). This manages the Go binary lifecycle: start, stop, health check, OAuth login.

- [ ] **Step 1: Clone presenter and read the proxy module**

```bash
git clone --depth 1 https://github.com/zbynekdrlik/presenter.git /tmp/presenter-ref
```

Read these files for reference (do NOT copy verbatim — adapt to SongPlayer's patterns):
- `/tmp/presenter-ref/crates/presenter-server/src/ai/proxy.rs`
- `/tmp/presenter-ref/crates/presenter-server/src/ai/mod.rs`

- [ ] **Step 2: Create `ai/mod.rs` with settings**

```rust
//! AI infrastructure: CLIProxyAPI proxy manager + OpenAI-compatible client.
//!
//! All AI tasks in SongPlayer route through this module:
//! merge alignment, word matching, SK translation, metadata extraction.

pub mod proxy;
pub mod client;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSettings {
    pub api_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub system_prompt_extra: Option<String>,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            api_url: "http://localhost:18787/v1".into(),
            api_key: None,
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        }
    }
}
```

- [ ] **Step 3: Create `ai/proxy.rs` — process manager**

Port from presenter's `proxy.rs`. Key adaptations for SongPlayer:
- Binary name: `CLIProxyAPI` (same as presenter)
- Config file: `cli-proxy-api-config.yaml` in the app data dir (same as presenter)
- Default port: 18787
- Process lifecycle: `start()`, `stop()`, `status()`, `claude_login()`, `complete_login(callback_url)`
- The binary path should be looked up in the tools/cache directory (same pattern as yt-dlp.exe discovery)

The proxy manager struct:
```rust
pub struct AiProxy {
    port: u16,
    binary_path: PathBuf,
    config_path: PathBuf,
    data_dir: PathBuf,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}
```

Key methods to port from presenter:
- `AiProxy::new(data_dir, port)` — discovers binary, sets up config path
- `start(&self)` — spawns process with config
- `stop(&self)` — kills process gracefully
- `status(&self) -> ProxyStatus` — checks if process is alive + if API responds
- `claude_login(&self) -> String` — starts OAuth flow, returns login URL
- `complete_login(&self, callback_url: &str)` — forwards callback

Follow presenter's implementation closely but use SongPlayer's tracing patterns (not println).

- [ ] **Step 4: Register AI module in lib.rs**

Add to `crates/sp-server/src/lib.rs`:
```rust
pub mod ai;
```

- [ ] **Step 5: Verify compilation**

```bash
cargo check -p sp-server
```

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/ai/
git commit -m "feat(ai): add CLIProxyAPI process manager (port from presenter)

Embeds CLIProxyAPI as a managed child process for OpenAI-compatible
access to Claude Opus. Supports start/stop/status/login lifecycle."
```

---

## Task 3: AI module — OpenAI-compatible client

**Files:**
- Create: `crates/sp-server/src/ai/client.rs`
- Modify: `crates/sp-server/src/ai/mod.rs`

HTTP client that talks to CLIProxyAPI's `/v1/chat/completions` endpoint. Port from presenter's `client.rs`.

- [ ] **Step 1: Create `client.rs`**

Read presenter's `/tmp/presenter-ref/crates/presenter-server/src/ai/client.rs` for reference.

The client struct:
```rust
pub struct AiClient {
    http: reqwest::Client,
    settings: AiSettings,
}
```

Key methods:
- `AiClient::new(settings: AiSettings)` — creates reqwest client
- `chat(&self, system: &str, user: &str) -> Result<String>` — sends chat completion request, returns assistant message content
- `chat_json<T: DeserializeOwned>(&self, system: &str, user: &str) -> Result<T>` — same but parses response as JSON

The request format (OpenAI-compatible):
```json
{
  "model": "claude-opus-4-20250514",
  "messages": [
    {"role": "system", "content": "..."},
    {"role": "user", "content": "..."}
  ],
  "temperature": 0.1
}
```

The response parsing extracts `choices[0].message.content`.

Include retry logic: up to 3 attempts with exponential backoff on 429/500/502/503 status codes.

- [ ] **Step 2: Add tests**

Test with a mock (don't call real API in unit tests):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_response() {
        let response_json = r#"{
            "choices": [{
                "message": {
                    "content": "{\"result\": \"hello\"}"
                }
            }]
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(response_json).unwrap();
        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(content, "{\"result\": \"hello\"}");
    }

    #[test]
    fn ai_settings_default() {
        let s = AiSettings::default();
        assert_eq!(s.api_url, "http://localhost:18787/v1");
        assert!(s.model.contains("claude"));
    }
}
```

- [ ] **Step 3: Re-export client from ai/mod.rs**

Already done in Step 2 of Task 2 (`pub mod client;`).

- [ ] **Step 4: Verify and commit**

```bash
cargo test -p sp-server -- ai::client::tests
git add crates/sp-server/src/ai/client.rs crates/sp-server/src/ai/mod.rs
git commit -m "feat(ai): add OpenAI-compatible HTTP client for CLIProxyAPI

Sends chat completions to localhost:18787/v1 with retry logic.
Supports both raw text and JSON-parsed responses."
```

---

## Task 4: AI API endpoints

**Files:**
- Create: `crates/sp-server/src/api/ai.rs`
- Modify: `crates/sp-server/src/api/mod.rs`

HTTP endpoints for managing the CLIProxyAPI proxy from the dashboard. Follow the existing pattern in `api/routes.rs`.

- [ ] **Step 1: Create `api/ai.rs` with endpoints**

Endpoints (all under `/api/v1/ai/`):
- `POST /proxy/start` — starts CLIProxyAPI process
- `POST /proxy/stop` — stops CLIProxyAPI process
- `POST /proxy/login` — initiates Claude OAuth, returns login URL
- `POST /proxy/complete-login` — body: `{"callback_url": "..."}`, forwards to proxy
- `GET /status` — returns `{"running": bool, "api_url": str, "authenticated": bool}`

Follow the existing pattern from `api/routes.rs`: extract `State<AppState>`, return `Json<serde_json::Value>`.

The `AppState` needs a new field: `ai_proxy: Arc<AiProxy>`. Add it in Task 7 when wiring everything in `lib.rs`.

For now, define the handler functions with the right signatures:
```rust
pub async fn proxy_start(State(state): State<Arc<AppState>>) -> impl IntoResponse
pub async fn proxy_stop(State(state): State<Arc<AppState>>) -> impl IntoResponse
pub async fn proxy_login(State(state): State<Arc<AppState>>) -> impl IntoResponse
pub async fn proxy_complete_login(State(state): State<Arc<AppState>>, Json(body): Json<serde_json::Value>) -> impl IntoResponse
pub async fn ai_status(State(state): State<Arc<AppState>>) -> impl IntoResponse
```

- [ ] **Step 2: Register routes in api/mod.rs**

Add the AI routes to the router:
```rust
pub mod ai;

// In the router function, add:
.route("/api/v1/ai/proxy/start", post(ai::proxy_start))
.route("/api/v1/ai/proxy/stop", post(ai::proxy_stop))
.route("/api/v1/ai/proxy/login", post(ai::proxy_login))
.route("/api/v1/ai/proxy/complete-login", post(ai::proxy_complete_login))
.route("/api/v1/ai/status", get(ai::ai_status))
```

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/api/ai.rs crates/sp-server/src/api/mod.rs
git commit -m "feat(api): add AI proxy management endpoints

POST start/stop/login/complete-login + GET status for CLIProxyAPI
lifecycle management from the dashboard."
```

---

## Task 5: LLM-powered merge layer

**Files:**
- Create: `crates/sp-server/src/lyrics/merge.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`

The core innovation: send all provider results to Claude Opus for intelligent word matching and timing merge.

- [ ] **Step 1: Create `merge.rs` with prompt construction**

```rust
//! LLM-powered merge layer for ensemble alignment.
//!
//! Accepts 1–N `ProviderResult`s, constructs a Claude Opus prompt,
//! parses the JSON response into a merged `LyricsTrack`, and writes
//! an audit log.

use anyhow::{Context, Result};
use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};
use std::path::Path;
use tokio::fs;
use tracing::debug;

use crate::ai::client::AiClient;
use crate::lyrics::provider::*;

/// Build the merge prompt for Claude Opus.
///
/// This is a pure function (no I/O) so it can be unit-tested with
/// fixture data.
pub fn build_merge_prompt(
    reference_text: &str,
    reference_source: &str,
    provider_results: &[ProviderResult],
) -> (String, String) {
    let system = "You are a lyrics alignment merger. You receive word-level \
        timestamp results from multiple independent alignment providers for the \
        same song. Your job: produce a single merged result with the best \
        possible timing for each word.\n\n\
        Rules:\n\
        1. Match each provider's words to the reference text intelligently — \
           handle contractions (you're = youre), ASR errors (grace vs Grace's), \
           abbreviations (G.O.D = GOD), dropped words.\n\
        2. If multiple providers matched a word: use weighted average of their \
           timings (weights given per provider). Reject any estimate >2000ms \
           from the median of others.\n\
        3. If only one provider matched: use it with reduced confidence (0.7x).\n\
        4. If no provider matched: zero-timed placeholder, confidence 0.\n\
        5. If gap >2000ms between adjacent words within a line: set display_split=true.\n\
        6. Return ONLY valid JSON, no markdown fences."
        .to_string();

    let mut user = String::new();
    user.push_str(&format!(
        "Reference text (source: {reference_source}):\n{reference_text}\n\n"
    ));
    user.push_str("Provider results:\n");
    for pr in provider_results {
        user.push_str(&format!(
            "\n--- {} (base_confidence in metadata) ---\n",
            pr.provider_name
        ));
        // Serialize compactly: just word + start_ms per line
        for line in &pr.lines {
            let words_str: Vec<String> = line.words.iter().map(|w| {
                format!("{}@{}ms", w.text, w.start_ms)
            }).collect();
            user.push_str(&format!("  [{}] {}\n", line.start_ms, words_str.join(" ")));
        }
    }
    user.push_str("\nReturn JSON: {\"lines\": [{\"text\": \"...\", \"start_ms\": N, \
        \"end_ms\": N, \"display_split\": false, \"words\": [{\"text\": \"...\", \
        \"start_ms\": N, \"end_ms\": N, \"confidence\": 0.95, \"sources_agreed\": 2, \
        \"spread_ms\": 50}]}]}");

    (system, user)
}

/// Parsed LLM merge response.
#[derive(Debug, serde::Deserialize)]
struct MergeResponse {
    lines: Vec<MergeResponseLine>,
}

#[derive(Debug, serde::Deserialize)]
struct MergeResponseLine {
    text: String,
    start_ms: u64,
    end_ms: u64,
    #[serde(default)]
    display_split: bool,
    words: Vec<MergeResponseWord>,
}

#[derive(Debug, serde::Deserialize)]
struct MergeResponseWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
    confidence: f32,
    #[serde(default)]
    sources_agreed: u8,
    #[serde(default)]
    spread_ms: u32,
}

/// Run the LLM merge: send prompt to Claude, parse response, return
/// merged LyricsTrack + audit data.
pub async fn merge_provider_results(
    ai_client: &AiClient,
    reference_text: &str,
    reference_source: &str,
    provider_results: &[ProviderResult],
) -> Result<(LyricsTrack, Vec<WordMergeDetail>)> {
    let (system, user) = build_merge_prompt(reference_text, reference_source, provider_results);

    debug!(
        "merge: sending {} providers to Claude ({} chars prompt)",
        provider_results.len(),
        user.len()
    );

    let response: MergeResponse = ai_client
        .chat_json(&system, &user)
        .await
        .context("LLM merge call failed")?;

    // Convert MergeResponse → LyricsTrack
    let lines: Vec<LyricsLine> = response
        .lines
        .iter()
        .map(|l| LyricsLine {
            start_ms: l.start_ms,
            end_ms: l.end_ms,
            en: l.text.clone(),
            sk: None,
            words: Some(
                l.words
                    .iter()
                    .map(|w| LyricsWord {
                        text: w.text.clone(),
                        start_ms: w.start_ms,
                        end_ms: w.end_ms,
                    })
                    .collect(),
            ),
        })
        .collect();

    let track = LyricsTrack {
        version: 2, // v2 = ensemble-merged
        source: format!(
            "ensemble:{}",
            provider_results
                .iter()
                .map(|p| p.provider_name.as_str())
                .collect::<Vec<_>>()
                .join("+")
        ),
        language_source: "en".into(),
        language_translation: String::new(),
        lines,
    };

    // Build audit details from response
    let mut details = Vec::new();
    let mut word_idx = 0;
    for line in &response.lines {
        for word in &line.words {
            details.push(WordMergeDetail {
                word_index: word_idx,
                reference_text: word.text.clone(),
                // The LLM doesn't return per-provider breakdowns in v1;
                // we record what we sent and what came back.
                provider_estimates: Vec::new(),
                outliers_rejected: Vec::new(),
                merged_start_ms: word.start_ms,
                merged_confidence: word.confidence,
                spread_ms: word.spread_ms,
            });
            word_idx += 1;
        }
    }

    Ok((track, details))
}

/// Write the audit log to disk alongside the lyrics JSON.
pub async fn write_audit_log(cache_dir: &Path, log: &AuditLog) -> Result<()> {
    let path = cache_dir.join(format!("{}_alignment_audit.json", log.video_id));
    let json = serde_json::to_string_pretty(log)?;
    fs::write(&path, json).await?;
    debug!("wrote audit log to {}", path.display());
    Ok(())
}
```

- [ ] **Step 2: Add unit tests for prompt construction**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_merge_prompt_includes_reference_and_providers() {
        let results = vec![
            ProviderResult {
                provider_name: "qwen3".into(),
                lines: vec![LineTiming {
                    text: "Hello world".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    words: vec![
                        WordTiming { text: "Hello".into(), start_ms: 1000, end_ms: 1500, confidence: 0.9 },
                        WordTiming { text: "world".into(), start_ms: 1500, end_ms: 2000, confidence: 0.9 },
                    ],
                }],
                metadata: serde_json::json!({}),
            },
        ];
        let (system, user) = build_merge_prompt("Hello world", "manual_subs", &results);
        assert!(system.contains("lyrics alignment merger"));
        assert!(user.contains("Hello world"));
        assert!(user.contains("manual_subs"));
        assert!(user.contains("qwen3"));
        assert!(user.contains("Hello@1000ms"));
    }

    #[test]
    fn build_merge_prompt_handles_multiple_providers() {
        let results = vec![
            ProviderResult {
                provider_name: "qwen3".into(),
                lines: vec![],
                metadata: serde_json::json!({}),
            },
            ProviderResult {
                provider_name: "autosub".into(),
                lines: vec![],
                metadata: serde_json::json!({}),
            },
        ];
        let (_, user) = build_merge_prompt("test", "lrclib", &results);
        assert!(user.contains("qwen3"));
        assert!(user.contains("autosub"));
    }

    #[test]
    fn parse_merge_response_json() {
        let json = r#"{
            "lines": [{
                "text": "Hello world",
                "start_ms": 1000,
                "end_ms": 2000,
                "display_split": false,
                "words": [
                    {"text": "Hello", "start_ms": 1000, "end_ms": 1500, "confidence": 0.95, "sources_agreed": 2, "spread_ms": 50},
                    {"text": "world", "start_ms": 1500, "end_ms": 2000, "confidence": 0.9, "sources_agreed": 1, "spread_ms": 0}
                ]
            }]
        }"#;
        let parsed: MergeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].words.len(), 2);
        assert_eq!(parsed.lines[0].words[0].confidence, 0.95);
    }
}
```

- [ ] **Step 3: Register in lyrics/mod.rs**

```rust
pub mod merge;
```

- [ ] **Step 4: Verify and commit**

```bash
cargo test -p sp-server -- merge::tests
git add crates/sp-server/src/lyrics/merge.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): add LLM-powered merge layer for ensemble alignment

Constructs Claude Opus prompt from N provider results, parses the
merged word timings, writes audit log. Pure prompt construction is
unit-tested with fixture data."
```

---

## Task 6: Refactor Qwen3 pipeline as AlignmentProvider

**Files:**
- Modify: `crates/sp-server/src/lyrics/aligner.rs`
- Create: `crates/sp-server/src/lyrics/qwen3_provider.rs` (or add to aligner.rs)

Wrap the existing `preprocess_vocals` + `align_chunks` pipeline behind the `AlignmentProvider` trait. The existing functions stay unchanged — we just add a struct that implements the trait and calls them.

- [ ] **Step 1: Create `qwen3_provider.rs`**

```rust
//! Qwen3-ForcedAligner as an AlignmentProvider.
//!
//! Wraps the existing vocal-isolation + chunked-alignment pipeline
//! (aligner.rs, chunking.rs, assembly.rs) behind the trait interface.

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use crate::lyrics::provider::*;

pub struct Qwen3Provider {
    pub python_path: PathBuf,
    pub script_path: PathBuf,
    pub models_dir: PathBuf,
}

#[async_trait]
impl AlignmentProvider for Qwen3Provider {
    fn name(&self) -> &str {
        "qwen3"
    }

    fn base_confidence(&self) -> f32 {
        0.9
    }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        // Qwen3 needs the clean vocal WAV (produced by Mel-Roformer + anvuew)
        ctx.clean_vocal_path.is_some()
    }

    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let clean_vocal = ctx
            .clean_vocal_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Qwen3 requires clean_vocal_path"))?;

        // Use the best candidate text as alignment input
        let text = ctx
            .candidate_texts
            .first()
            .ok_or_else(|| anyhow::anyhow!("No candidate text for Qwen3"))?;

        // Build a LyricsTrack from candidate text for chunking
        let track = crate::lyrics::provider_utils::candidate_to_track(text);

        // Run existing chunking → alignment → assembly pipeline
        let chunks = crate::lyrics::chunking::plan_chunks(&track);
        let chunk_results = crate::lyrics::aligner::align_chunks(
            &self.python_path,
            &self.script_path,
            clean_vocal,
            &chunks,
            &ctx.audio_path.with_extension("chunks.json"),
            &ctx.audio_path.with_extension("aligned.json"),
        )
        .await?;
        let aligned = crate::lyrics::assembly::assemble(&track, &chunk_results);

        // Convert LyricsTrack → ProviderResult
        Ok(track_to_provider_result("qwen3", &aligned))
    }
}

/// Convert a CandidateText into a minimal LyricsTrack for chunking.
pub(crate) mod provider_utils {
    use sp_core::lyrics::{LyricsLine, LyricsTrack};
    use crate::lyrics::provider::CandidateText;

    pub fn candidate_to_track(text: &CandidateText) -> LyricsTrack {
        let lines = text.lines.iter().enumerate().map(|(i, line_text)| {
            let (start, end) = text.line_timings
                .as_ref()
                .and_then(|t| t.get(i))
                .copied()
                .unwrap_or((0, 0));
            LyricsLine {
                start_ms: start,
                end_ms: end,
                en: line_text.clone(),
                sk: None,
                words: None,
            }
        }).collect();

        LyricsTrack {
            version: 1,
            source: text.source.clone(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        }
    }
}

/// Convert a LyricsTrack (with word timings) to a ProviderResult.
fn track_to_provider_result(provider_name: &str, track: &sp_core::lyrics::LyricsTrack) -> ProviderResult {
    ProviderResult {
        provider_name: provider_name.into(),
        lines: track
            .lines
            .iter()
            .map(|l| LineTiming {
                text: l.en.clone(),
                start_ms: l.start_ms,
                end_ms: l.end_ms,
                words: l
                    .words
                    .as_ref()
                    .map(|ws| {
                        ws.iter()
                            .map(|w| WordTiming {
                                text: w.text.clone(),
                                start_ms: w.start_ms,
                                end_ms: w.end_ms,
                                confidence: 0.9,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect(),
        metadata: serde_json::json!({"source": track.source}),
    }
}
```

- [ ] **Step 2: Register in lyrics/mod.rs**

```rust
pub mod qwen3_provider;
```

- [ ] **Step 3: Verify and commit**

```bash
cargo check -p sp-server
git add crates/sp-server/src/lyrics/qwen3_provider.rs crates/sp-server/src/lyrics/mod.rs
git commit -m "feat(lyrics): refactor Qwen3 pipeline as AlignmentProvider

Wraps existing preprocess_vocals + align_chunks + assembly behind the
AlignmentProvider trait. No behavior change — same pipeline, new
interface for ensemble merge."
```

---

## Task 7: Orchestrator — gather → align → merge → translate

**Files:**
- Create: `crates/sp-server/src/lyrics/orchestrator.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs`
- Modify: `crates/sp-server/src/lyrics/worker.rs`

The orchestrator replaces the monolithic `process_song` in `worker.rs`. It coordinates the full per-song pipeline.

- [ ] **Step 1: Create `orchestrator.rs`**

```rust
//! Per-song ensemble alignment orchestrator.
//!
//! Coordinates: gather text sources → run alignment providers →
//! merge via LLM → translate → quality gate.

use anyhow::{Context, Result};
use sp_core::lyrics::LyricsTrack;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::ai::client::AiClient;
use crate::lyrics::merge;
use crate::lyrics::provider::*;

pub struct Orchestrator {
    providers: Vec<Box<dyn AlignmentProvider>>,
    ai_client: Arc<AiClient>,
    cache_dir: std::path::PathBuf,
}

impl Orchestrator {
    pub fn new(
        providers: Vec<Box<dyn AlignmentProvider>>,
        ai_client: Arc<AiClient>,
        cache_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            providers,
            ai_client,
            cache_dir,
        }
    }

    /// Run the full ensemble pipeline for one song.
    pub async fn process_song(&self, ctx: &SongContext) -> Result<LyricsTrack> {
        info!(
            video_id = %ctx.video_id,
            "orchestrator: starting ensemble alignment"
        );

        // Pick best reference text
        let (reference_text, reference_source) = self.select_reference_text(ctx);

        // Run providers sequentially (cheapest first)
        let mut results: Vec<ProviderResult> = Vec::new();
        let mut skipped: Vec<(String, String)> = Vec::new();

        for provider in &self.providers {
            if !provider.can_provide(ctx).await {
                skipped.push((
                    provider.name().into(),
                    "can_provide returned false".into(),
                ));
                continue;
            }

            debug!(
                video_id = %ctx.video_id,
                provider = provider.name(),
                "running alignment provider"
            );

            match provider.align(ctx).await {
                Ok(result) => {
                    results.push(result);

                    // Early-stop: if we have 2+ providers, check quality
                    if results.len() >= 2 {
                        // Could add early-stop logic here in the future
                    }
                }
                Err(e) => {
                    warn!(
                        video_id = %ctx.video_id,
                        provider = provider.name(),
                        error = %e,
                        "provider failed, continuing with remaining"
                    );
                    skipped.push((provider.name().into(), format!("{e}")));
                }
            }
        }

        if results.is_empty() {
            anyhow::bail!(
                "no providers produced results for {}",
                ctx.video_id
            );
        }

        // Merge via LLM
        let (track, word_details) = merge::merge_provider_results(
            &self.ai_client,
            &reference_text,
            &reference_source,
            &results,
        )
        .await
        .context("LLM merge failed")?;

        // Compute quality metrics
        let quality = QualityMetrics {
            avg_confidence: word_details
                .iter()
                .map(|d| d.merged_confidence)
                .sum::<f32>()
                / word_details.len().max(1) as f32,
            words_with_zero_timing: word_details
                .iter()
                .filter(|d| d.merged_start_ms == 0)
                .count(),
            duplicate_start_pct: crate::lyrics::quality::duplicate_start_pct(&track),
            gap_stddev_ms: crate::lyrics::quality::gap_stddev_ms(&track),
        };

        // Write audit log
        let audit = AuditLog {
            video_id: ctx.video_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            reference_text_source: reference_source.clone(),
            providers_run: results.iter().map(|r| r.provider_name.clone()).collect(),
            providers_skipped: skipped,
            per_word_details: word_details,
            quality_metrics: quality,
        };
        let _ = merge::write_audit_log(&self.cache_dir, &audit).await;

        info!(
            video_id = %ctx.video_id,
            providers = results.len(),
            avg_confidence = audit.quality_metrics.avg_confidence,
            "orchestrator: ensemble alignment complete"
        );

        Ok(track)
    }

    /// Select the best reference text from candidates.
    /// Priority: manual_subs > description > lrclib > autosub
    fn select_reference_text(&self, ctx: &SongContext) -> (String, String) {
        let priority = ["ccli", "manual_subs", "description", "lrclib", "autosub"];
        for source in &priority {
            if let Some(ct) = ctx.candidate_texts.iter().find(|c| c.source == *source) {
                return (ct.lines.join("\n"), ct.source.clone());
            }
        }
        // Fallback: first available
        if let Some(ct) = ctx.candidate_texts.first() {
            return (ct.lines.join("\n"), ct.source.clone());
        }
        (String::new(), "none".into())
    }
}
```

- [ ] **Step 2: Register in lyrics/mod.rs**

```rust
pub mod orchestrator;
```

- [ ] **Step 3: Update `worker.rs` to use orchestrator**

In `worker.rs`, replace the direct `run_chunked_alignment` call in `process_song` with a call to `orchestrator.process_song(ctx)`. The worker builds a `SongContext` from the `VideoLyricsRow` and delegates to the orchestrator.

This is the integration point — keep the existing worker loop but swap the inner processing logic. The worker still handles: polling for unprocessed songs, retry backoff, writing the final lyrics JSON, updating the DB.

- [ ] **Step 4: Verify and commit**

```bash
cargo check -p sp-server
git add crates/sp-server/src/lyrics/orchestrator.rs crates/sp-server/src/lyrics/mod.rs crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(lyrics): add ensemble orchestrator, wire into worker

Orchestrator coordinates: select reference text → run providers →
merge via LLM → compute quality → write audit log. Worker delegates
to orchestrator instead of calling Qwen3 directly."
```

---

## Task 8: Migrate SK translation to Claude Opus

**Files:**
- Modify: `crates/sp-server/src/lyrics/translator.rs`

Add a Claude Opus translation path alongside the existing Gemini translator. When CLIProxyAPI is available, use Claude. Otherwise fall back to Gemini.

- [ ] **Step 1: Add `translate_via_claude` function to translator.rs**

```rust
/// Translate lyrics to Slovak via Claude Opus (CLIProxyAPI).
pub async fn translate_via_claude(
    ai_client: &crate::ai::client::AiClient,
    track: &LyricsTrack,
) -> Result<Vec<String>> {
    let lines_en: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    let user = format!(
        "Translate these English worship song lyrics to Slovak. \
         Preserve the line structure exactly — each English line maps \
         to one Slovak line. Use natural Slovak worship vocabulary.\n\n\
         Return a JSON array of translated lines (strings only, same \
         count as input).\n\n\
         English lines:\n{}",
        serde_json::to_string_pretty(&lines_en)?
    );
    let system = "You are a professional translator specializing in worship \
        music lyrics. Translate English to Slovak. Return ONLY a JSON array \
        of strings.";

    let translations: Vec<String> = ai_client
        .chat_json(system, &user)
        .await
        .context("Claude translation failed")?;

    if translations.len() != track.lines.len() {
        anyhow::bail!(
            "translation count mismatch: got {} lines, expected {}",
            translations.len(),
            track.lines.len()
        );
    }

    Ok(translations)
}
```

- [ ] **Step 2: Update the translation caller to try Claude first**

In the existing translation flow (called from `worker.rs::retry_missing_translations`), add logic:
```rust
// Try Claude first if AI client available
if let Some(ai_client) = &self.ai_client {
    match translate_via_claude(ai_client, &track).await {
        Ok(translations) => { /* apply and persist */ },
        Err(e) => {
            warn!("Claude translation failed, falling back to Gemini: {e}");
            // existing Gemini path
        }
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/lyrics/translator.rs crates/sp-server/src/lyrics/worker.rs
git commit -m "feat(lyrics): add Claude Opus SK translation with Gemini fallback

Routes translation through CLIProxyAPI → Claude Opus first. Falls
back to existing Gemini path if proxy is unavailable."
```

---

## Task 9: Migrate metadata extraction to Claude Opus

**Files:**
- Modify: `crates/sp-server/src/metadata/gemini.rs`
- Modify: `crates/sp-server/src/metadata/mod.rs`

Same pattern as translation: add a Claude Opus provider alongside Gemini.

- [ ] **Step 1: Create `ClaudeMetadataProvider` in metadata/mod.rs**

Implement the existing `MetadataProvider` trait using the AI client. The prompt is the same as Gemini's (extract song/artist from video title).

- [ ] **Step 2: Wire Claude as primary, Gemini as fallback**

In the `get_metadata` function, try Claude first via `ai_client.chat_json()`, fall back to existing Gemini on error.

- [ ] **Step 3: Commit**

```bash
git add crates/sp-server/src/metadata/
git commit -m "feat(metadata): add Claude Opus provider with Gemini fallback"
```

---

## Task 10: Update config defaults and wire everything in lib.rs

**Files:**
- Modify: `crates/sp-core/src/config.rs`
- Modify: `crates/sp-server/src/lib.rs`

- [ ] **Step 1: Update config.rs**

```rust
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.1-pro-preview";

// New AI settings
pub const SETTING_AI_API_URL: &str = "ai_api_url";
pub const SETTING_AI_MODEL: &str = "ai_model";
pub const DEFAULT_AI_API_URL: &str = "http://localhost:18787/v1";
pub const DEFAULT_AI_MODEL: &str = "claude-opus-4-20250514";
```

- [ ] **Step 2: Wire AI proxy + orchestrator in lib.rs start()**

In the `start()` function:
1. Create `AiProxy` and optionally start it
2. Create `AiClient` with settings from DB
3. Create `Orchestrator` with providers list + AI client
4. Add `ai_proxy: Arc<AiProxy>` and `ai_client: Arc<AiClient>` to `AppState`
5. Pass `ai_client` to `LyricsWorker` (add field)
6. Pass orchestrator to worker

- [ ] **Step 3: Commit**

```bash
git add crates/sp-core/src/config.rs crates/sp-server/src/lib.rs
git commit -m "feat: wire AI proxy + ensemble orchestrator into server startup

Updates Gemini default to 3.1-pro-preview. Adds AI settings to config.
Wires CLIProxyAPI proxy, AI client, and orchestrator into AppState."
```

---

## Task 11: Push, monitor CI, open PR

- [ ] **Step 1: Run cargo fmt and verify tests**

```bash
cargo fmt --all --check
cargo test -p sp-server
```

- [ ] **Step 2: Push and monitor CI**

```bash
git push origin dev
gh run list --branch dev --limit 1
# Monitor until all jobs green
```

- [ ] **Step 3: Open PR**

```bash
gh pr create --base main --head dev \
  --title "feat(#29): ensemble alignment pipeline with Claude Opus merge layer" \
  --body "..."
```

---

## Verification

After all tasks complete:

1. `cargo check -p sp-server` compiles
2. `cargo test -p sp-server` — all existing + new tests pass
3. `cargo fmt --all --check` clean
4. CLIProxyAPI starts via dashboard, Claude OAuth works
5. A song processes through the ensemble pipeline (even with just Qwen3 as the single provider, the merge layer runs)
6. Audit log is written to cache dir
7. SK translation routes through Claude when proxy is up, Gemini when down
8. CI green, PR mergeable
