//! Orchestrator — drives the tier chain for a single song.
//!
//! Flow: Tier-1 collect → branch on LineSynced/TextOnly/None →
//! WhisperX backend (Tier-2) when needed → claude-merge (TextOnly path) →
//! SubtitleEdit-port line split. Returns `AlignedTrack`; the caller
//! (worker) converts to `LyricsTrack` and translates separately.
//!
//! The orchestrator does NOT hold fetcher factories. Instead,
//! `OrchestratorInput.fetchers` carries the per-song `Vec<FetchFn>`
//! already built by the worker from `candidate_texts`. This keeps
//! the orchestrator stateless between songs and trivially unit-testable
//! — tests inject mock fetchers inline without any factory machinery.
//!
//! Per `feedback_no_legacy_code.md`: this module imports NONE of
//! the legacy providers (gemini_provider, qwen3_provider,
//! autosub_provider, description_provider, text_merge).
//! Those are deleted in Phase G.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tracing::info;

use crate::ai::client::AiClient;
use crate::lyrics::backend::{AlignOpts, AlignedTrack, AlignmentBackend, BackendError};
use crate::lyrics::claude_merge;
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::tier1::{FetchFn, Tier1Result, collect};

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("backend: {0}")]
    Backend(#[from] BackendError),
    #[error("no alignment available: {0}")]
    NoAlignment(String),
}

pub struct Orchestrator {
    pub backend: Arc<dyn AlignmentBackend>,
    pub ai_client: Arc<AiClient>,
    pub split_cfg: SplitConfig,
}

/// Per-song input to `Orchestrator::process`.
///
/// `fetchers` is a `Vec<FetchFn>` built by the worker from the song's
/// `candidate_texts` (and any Spotify fetcher keyed on `spotify_track_id`).
/// Each closure captures its own per-song arguments; the orchestrator
/// calls `tier1::collect(fetchers)` which runs them in parallel.
pub struct OrchestratorInput<'a> {
    /// Pre-built per-song Tier-1 fetcher list. Built by the worker from
    /// `candidate_texts` (and optional Spotify fetcher). The orchestrator
    /// drives `tier1::collect(fetchers)` with these.
    pub fetchers: Vec<FetchFn>,
    /// BCP-47 language code for the ASR backend (e.g. "en").
    pub language: &'a str,
    /// Path to the Mel-Roformer + anvuew dereverb vocal stem.
    /// `None` when `preprocess_vocals` failed or tooling is unavailable.
    /// If `None` and Tier-1 returns `TextOnly` or `None` (requiring backend
    /// alignment), `process` returns `OrchestratorError::NoAlignment`.
    /// Tier-1 `LineSynced` short-circuits before the backend is reached and
    /// therefore succeeds even when this is `None`.
    pub vocal_wav: Option<&'a Path>,
}

impl Orchestrator {
    pub fn new(
        backend: Arc<dyn AlignmentBackend>,
        ai_client: Arc<AiClient>,
        split_cfg: SplitConfig,
    ) -> Self {
        Self {
            backend,
            ai_client,
            split_cfg,
        }
    }

    /// Run the full tier chain for one song and return an `AlignedTrack`.
    ///
    /// The caller (worker) is responsible for:
    /// - Building `OrchestratorInput.fetchers` from `candidate_texts`
    /// - Converting `AlignedTrack` → `LyricsTrack` after this returns
    /// - Calling the translator on the resulting `LyricsTrack`
    pub async fn process(
        &self,
        input: OrchestratorInput<'_>,
    ) -> Result<AlignedTrack, OrchestratorError> {
        // Step 1: Run all Tier-1 fetchers in parallel and pick the best result.
        let tier1_result = collect(input.fetchers).await;

        // Step 2: Branch on Tier-1 outcome.
        match tier1_result {
            Tier1Result::LineSynced(aligned_lines) => {
                // Authoritative line-synced timing — ship directly, no ASR call.
                // Apply split_track to enforce the 32-char cap on long yt_subs lines.
                info!(
                    provenance = %aligned_lines.provenance,
                    lines = aligned_lines.lines.len(),
                    "orchestrator: Tier-1 short-circuit (line-synced), skipping backend"
                );
                let pre_split = AlignedTrack {
                    lines: aligned_lines.lines,
                    provenance: aligned_lines.provenance,
                    raw_confidence: 1.0,
                };
                Ok(split_track(&pre_split, self.split_cfg))
            }
            Tier1Result::TextOnly(text_candidates) => {
                // Text-only: run WhisperX for timing, then use Claude to semantically
                // merge authoritative text with WhisperX phrases to correct mishearings.
                // Claude's prompt enforces the 32-char cap internally — do NOT run
                // split_track on the claude-merge output (double-splitting corrupts timings).
                // If claude-merge fails, fall back to split_track on raw WhisperX.
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 TextOnly path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, None, input.language, &AlignOpts::default())
                    .await?;
                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    text_candidates = text_candidates.len(),
                    "orchestrator: Tier-1 TextOnly — backend called, running claude-merge"
                );
                match claude_merge::merge(&self.ai_client, &asr, &text_candidates).await {
                    Ok(merged) => Ok(merged),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %asr.provenance,
                            error = %e,
                            "orchestrator: claude-merge failed — falling back to raw WhisperX with line split"
                        );
                        Ok(split_track(&asr, self.split_cfg))
                    }
                }
            }
            Tier1Result::None => {
                // No text candidates at all — run WhisperX and ship its output
                // with the line splitter (no reconciliation possible without reference text).
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 None path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, None, input.language, &AlignOpts::default())
                    .await?;
                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    "orchestrator: Tier-1 None — backend called, no reconciliation"
                );
                Ok(split_track(&asr, self.split_cfg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSettings, client::AiClient};
    use crate::lyrics::backend::{
        AlignOpts, AlignedLine, AlignedTrack, AlignedWord, AlignmentBackend, AlignmentCapability,
        BackendError,
    };
    use crate::lyrics::tier1::{CandidateText, FetchFn};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- Mock backend ---

    /// Backend that returns a fixed track and counts how many times `align` was called.
    struct MockBackend {
        call_count: Arc<AtomicUsize>,
        response: AlignedTrack,
    }

    impl MockBackend {
        fn new(response: AlignedTrack) -> (Self, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let b = Self {
                call_count: counter.clone(),
                response,
            };
            (b, counter)
        }
    }

    #[async_trait]
    impl AlignmentBackend for MockBackend {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn revision(&self) -> u32 {
            1
        }
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
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self.response.clone())
        }
    }

    /// Build a minimal ASR track with one line and per-word timings.
    fn asr_track(provenance: &str) -> AlignedTrack {
        AlignedTrack {
            lines: vec![AlignedLine {
                text: "amazing grace".into(),
                start_ms: 0,
                end_ms: 2000,
                words: Some(vec![
                    AlignedWord {
                        text: "amazing".into(),
                        start_ms: 0,
                        end_ms: 1000,
                        confidence: 0.9,
                    },
                    AlignedWord {
                        text: "grace".into(),
                        start_ms: 1000,
                        end_ms: 2000,
                        confidence: 0.9,
                    },
                ]),
            }],
            provenance: provenance.into(),
            raw_confidence: 0.9,
        }
    }

    /// Helper: build a FetchFn that always returns `Some(candidate)`.
    fn fixed_fetcher(candidate: CandidateText) -> FetchFn {
        Arc::new(move || {
            let c = candidate.clone();
            Box::pin(async move { Some(c) })
        })
    }

    /// Helper: build a FetchFn that returns `None` (fetcher failed / missing).
    fn empty_fetcher() -> FetchFn {
        Arc::new(|| Box::pin(async { None }))
    }

    /// Build an AiClient pointed at a wiremock server URL.
    fn mock_ai_client(api_url: &str) -> Arc<AiClient> {
        Arc::new(AiClient::new(AiSettings {
            api_url: format!("{api_url}/v1"),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        }))
    }

    // -----------------------------------------------------------------------
    // Test 1: Tier-1 short-circuit (LineSynced) — backend must NOT be called
    // -----------------------------------------------------------------------

    /// When Tier-1 returns `LineSynced`, the orchestrator ships the line-synced
    /// output directly and NEVER calls the backend's `align` method.
    /// Per `feedback_line_timing_only.md` every line must carry `words: None`.
    #[tokio::test]
    async fn tier1_short_circuit_skips_backend() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // AI server should never be called on the LineSynced path.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        // Build a 12-line timed fetcher — above TIER1_MIN_LINES threshold.
        let lines: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        let timings: Vec<(u64, u64)> = (0..12).map(|i| (i * 1000, i * 1000 + 900)).collect();
        let candidate = CandidateText {
            source: "tier1:spotify".into(),
            lines: lines.clone(),
            line_timings: Some(timings),
            has_timing: true,
        };

        let (mock, call_count) = MockBackend::new(asr_track("mock@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![fixed_fetcher(candidate)],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            })
            .await
            .expect("process should succeed");

        // Backend must NOT have been called.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "backend.align must not be called when Tier-1 short-circuits"
        );

        // Provenance must come from the Tier-1 source.
        assert_eq!(
            result.provenance, "tier1:spotify",
            "provenance must be the tier1 source tag"
        );

        // Per feedback_line_timing_only.md: every line must have words: None.
        for line in &result.lines {
            assert!(
                line.words.is_none(),
                "Tier-1 short-circuit path must ship words: None on every line"
            );
        }

        // Output must have at least as many lines as the input (splitter may expand).
        assert!(result.lines.len() >= 12);
    }

    // -----------------------------------------------------------------------
    // Test 2: Tier-1 TextOnly — backend called, then claude-merge attempted.
    //         Success path: Claude returns valid JSON → provenance ends with +claude-merge.
    // -----------------------------------------------------------------------

    /// When Tier-1 returns `TextOnly`, the orchestrator calls the backend for
    /// timing and then attempts claude-merge. When Claude succeeds, provenance
    /// must contain `+claude-merge`.
    #[tokio::test]
    async fn tier1_text_only_runs_backend_then_claude_merge() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Mock Claude returning 1 merged line.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "{\"lines\": [{\"start_ms\": 0, \"end_ms\": 2000, \"text\": \"amazing grace\"}]}"}}]
            })))
            .mount(&server)
            .await;

        let candidate = CandidateText {
            source: "genius".into(),
            lines: vec!["amazing grace".into(), "how sweet the sound".into()],
            line_timings: None,
            has_timing: false,
        };

        let (mock, call_count) = MockBackend::new(asr_track("mock@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![fixed_fetcher(candidate)],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            })
            .await
            .expect("process should succeed");

        // Backend must have been called exactly once.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "backend.align must be called exactly once on TextOnly path"
        );

        // Provenance must contain "+claude-merge".
        assert!(
            result.provenance.contains("+claude-merge"),
            "TextOnly path must run claude-merge; got provenance: {}",
            result.provenance
        );

        // Per feedback_line_timing_only.md: words must be None on merged output.
        for line in &result.lines {
            assert!(line.words.is_none(), "merged output must have words: None");
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: Tier-1 TextOnly fallback — Claude fails → split_track on raw ASR
    // -----------------------------------------------------------------------

    /// When claude-merge fails (e.g., AI server unreachable), the orchestrator
    /// falls back to split_track on the raw WhisperX output. Provenance must
    /// NOT contain `+claude-merge`.
    #[tokio::test]
    async fn tier1_text_only_fallback_when_claude_fails() {
        // Point at a port nothing is listening on — connection refused = fallback.
        let dead_ai_client = Arc::new(AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:19999/v1".into(),
            api_key: None,
            model: "test".into(),
            system_prompt_extra: None,
        }));

        let candidate = CandidateText {
            source: "genius".into(),
            lines: vec!["amazing grace".into()],
            line_timings: None,
            has_timing: false,
        };

        let (mock, call_count) = MockBackend::new(asr_track("mock@rev1"));
        let orch = Orchestrator::new(Arc::new(mock), dead_ai_client, SplitConfig::default());

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![fixed_fetcher(candidate)],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            })
            .await
            .expect("fallback must succeed even when Claude is unreachable");

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(
            !result.provenance.contains("+claude-merge"),
            "fallback path must not set +claude-merge; got: {}",
            result.provenance
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: Tier-1 None — backend called, no reconciliation
    // -----------------------------------------------------------------------

    /// When Tier-1 returns `None` (no fetchers returned anything usable),
    /// the orchestrator calls the backend but does NOT run claude-merge.
    /// The output provenance must NOT contain `+claude-merge`.
    #[tokio::test]
    async fn tier1_none_runs_backend_only() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // AI server should never be called on the None path.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![empty_fetcher(), empty_fetcher()],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            })
            .await
            .expect("process should succeed");

        // Backend must have been called exactly once.
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "backend.align must be called exactly once on None path"
        );

        // Provenance must NOT contain "+claude-merge".
        assert!(
            !result.provenance.contains("+claude-merge"),
            "None path must skip claude-merge; got provenance: {}",
            result.provenance
        );

        // Provenance must reflect the backend's own ID.
        assert!(
            result.provenance.contains("whisperx"),
            "provenance should come from the backend; got: {}",
            result.provenance
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: Zero fetchers → same as Tier1::None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn zero_fetchers_falls_back_to_backend_only() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            })
            .await
            .expect("process should succeed");

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(!result.provenance.contains("+claude-merge"));
    }
}
