//! Tests for `Orchestrator`. Sibling file referenced by `orchestrator.rs`
//! under `#[path = "orchestrator_tests.rs"] #[cfg(test)] mod tests;` to keep
//! `orchestrator.rs` under the 1000-line airuleset cap.

use super::*;
use crate::ai::{AiSettings, client::AiClient};
use crate::lyrics::backend::{
    AlignOpts, AlignedLine, AlignedTrack, AlignedWord, AlignmentBackend, AlignmentCapability,
    BackendError,
};
use crate::lyrics::tier1::CandidateText;
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
        }
    }
    async fn align(
        &self,
        _wav: &Path,
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
            candidates: vec![candidate],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
        })
        .await
        .expect("process should succeed");

    // Backend must NOT have been called.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "backend.align must not be called when Tier-1 short-circuits"
    );

    // Provenance must come from the Tier-1 source + timed-merge suffix.
    assert_eq!(
        result.provenance, "tier1:spotify+timed-merge",
        "LineSynced path now routes through timed_reference_merge Mode B → +timed-merge suffix"
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
// Test 2: Tier-1 TextOnly — backend called, routes to text_reference_merge.
//         description source: provenance starts with "description+".
// -----------------------------------------------------------------------

/// When Tier-1 returns `TextOnly`, the orchestrator calls the backend for
/// timing and routes to `text_reference_merge::process` (no Claude phrase-merge).
/// The merged output's provenance starts with `{best.source}+`. For
/// description-only it is `description+whisperx-large-v3@rev1`.
#[tokio::test]
async fn tier1_text_only_routes_to_text_reference_merge() {
    use crate::lyrics::tier1::CandidateText;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // text_reference_merge::process calls Claude internally for line-mapping
    // (Phase 1) and line-split (Phase 3). Provide a permissive mock that
    // returns a 500 — text_reference_merge falls back to the deterministic
    // NW DP path on parse failure, which is fine for this test (we only
    // assert provenance + lack of +claude-merge suffix).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let candidate = CandidateText {
        source: "description".into(),
        lines: vec!["amazing grace".into(), "how sweet the sound".into()],
        line_timings: None,
        has_timing: false,
    };

    let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
    let orch = Orchestrator::new(
        Arc::new(mock),
        mock_ai_client(&server.uri()),
        SplitConfig::default(),
    );

    let result = orch
        .process(OrchestratorInput {
            candidates: vec![candidate],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
        })
        .await
        .expect("process should succeed");

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(
        result.provenance.starts_with("description+"),
        "TextOnly with description winner must produce description+... provenance; got: {}",
        result.provenance
    );
    assert!(
        !result.provenance.contains("+claude-merge"),
        "+claude-merge suffix is retired; got: {}",
        result.provenance
    );
    for line in &result.lines {
        assert!(line.words.is_none(), "merged output must have words: None");
    }
}

// -----------------------------------------------------------------------
// Test 2b: Tier-1 TextOnly — genius source routes to text_reference_merge.
// -----------------------------------------------------------------------

/// Post-fix: when the best-authoritative candidate is genius (description
/// absent), the orchestrator routes to `text_reference_merge::process`
/// (NOT the deleted Claude phrase-merge). Provenance starts with
/// `genius+`. The retired `+claude-merge` suffix must not appear.
#[tokio::test]
async fn tier1_text_only_with_genius_routes_to_text_reference_merge() {
    use crate::lyrics::tier1::CandidateText;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let candidate = CandidateText {
        source: "genius".into(),
        lines: vec!["amazing grace".into(), "how sweet the sound".into()],
        line_timings: None,
        has_timing: false,
    };

    let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
    let orch = Orchestrator::new(
        Arc::new(mock),
        mock_ai_client(&server.uri()),
        SplitConfig::default(),
    );

    let result = orch
        .process(OrchestratorInput {
            candidates: vec![candidate],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
        })
        .await
        .expect("process should succeed");

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(
        result.provenance.starts_with("genius+"),
        "genius-winning text-only must produce genius+... provenance; got: {}",
        result.provenance
    );
    assert!(
        !result.provenance.contains("+claude-merge"),
        "+claude-merge suffix is retired; got: {}",
        result.provenance
    );
}

// -----------------------------------------------------------------------
// Test 3: Tier-1 TextOnly fallback — text_reference_merge fails → split_track on raw ASR
// -----------------------------------------------------------------------

/// When text_reference_merge fails (e.g., AI server unreachable AND NW-DP
/// fallback also fails), the orchestrator falls back to split_track on the
/// raw WhisperX output. Provenance must NOT contain `+claude-merge`.
#[tokio::test]
async fn tier1_text_only_fallback_when_text_reference_merge_fails() {
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
            candidates: vec![candidate],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
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
            candidates: vec![],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
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
            candidates: vec![],
            language: "en",
            vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
            audit: None,
        })
        .await
        .expect("process should succeed");

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(!result.provenance.contains("+claude-merge"));
}
