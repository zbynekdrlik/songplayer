//! Orchestrator — drives the tier chain for a single song.
//!
//! Flow: Tier-1 collect → branch on LineSynced/TextOnly/None →
//! WhisperX backend (Tier-2) when needed → text_reference_merge (TextOnly path) →
//! Returns `AlignedTrack`; the caller (worker) converts to `LyricsTrack` and
//! translates separately.
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
use crate::lyrics::claude_merge::best_authoritative_candidate;
use crate::lyrics::claude_merge::coverage_ok;
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::text_reference_merge;
use crate::lyrics::tier1::{FetchFn, Tier1Result, collect};
use crate::lyrics::timed_reference_merge;

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
    /// Per-song debug-output sink. When populated, every alignment + merge
    /// stage writes a JSON sidecar to `cache_dir` for permanent visibility:
    /// `{youtube_id}_whisperx_track.json` (raw alignment backend output)
    /// and `{youtube_id}_descmerge_audit.json` (description-merge per-phase
    /// state). When `None`, sidecar writes are skipped — used by tests.
    pub audit: Option<crate::lyrics::audit_ctx::AuditContext<'a>>,
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
                // yt_subs has authoritative line text but YouTube auto-caption
                // line breaks split mid-phrase. The description+whisperx
                // pipeline (text_reference_merge) already solves chorus
                // repeats (Phase 2 + 2.8 sliding-window LCS), Claude line
                // mapping (Phase 1 — far stronger than forward-greedy LCS),
                // mishearing absorbs (2.6/2.65/2.7), karaoke split (Phase 3
                // Claude + 4 emit_with_subs), and cap+monotonic (Phase 5).
                // Route yt_subs through that same pipeline by clustering
                // caption-window adjacent lines into phrases and treating
                // them as a text candidate (yt_subs internal timing is
                // discarded; whisperx provides word-level boundaries).
                //
                // spotify / lrclib still short-circuit through
                // timed_reference_merge Mode B — their line breaks already
                // match phrase boundaries.
                let is_yt_subs = aligned_lines.provenance == "yt_subs"
                    || aligned_lines.provenance.starts_with("tier1:yt_subs");
                if is_yt_subs {
                    // yt_subs is the AUTHORITY for what is sung. Trust
                    // its lines + per-line timing. Only re-break LONG
                    // phrase clusters into karaoke sub-lines, with
                    // whisperx providing internal sub-line anchors when
                    // available and proportional interpolation when
                    // whisperx missed/mistranscribed words. yt_subs
                    // text is never dropped or substituted.
                    info!(
                        provenance = %aligned_lines.provenance,
                        lines = aligned_lines.lines.len(),
                        "orchestrator: Tier-1 yt_subs LineSynced → cluster + Claude split + whisperx anchors with proportional fallback"
                    );
                    let wav_opt = input.vocal_wav;
                    let asr_opt: Option<AlignedTrack> = if let Some(wav) = wav_opt {
                        match self
                            .backend
                            .align(wav, input.language, &AlignOpts::default())
                            .await
                        {
                            Ok(a) => {
                                crate::lyrics::audit_ctx::write_whisperx_track(
                                    input.audit.as_ref(),
                                    &a,
                                )
                                .await;
                                Some(a)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %e,
                                    "orchestrator: yt_subs whisperx align failed; using proportional split only"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let asr_words: Vec<crate::lyrics::backend::AlignedWord> = asr_opt
                        .as_ref()
                        .map(|a| {
                            a.lines
                                .iter()
                                .filter_map(|l| l.words.as_ref())
                                .flatten()
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let clustered =
                        crate::lyrics::yt_subs_split::cluster_caption_windows(&aligned_lines.lines);
                    let mut output: Vec<crate::lyrics::backend::AlignedLine> =
                        Vec::with_capacity(clustered.len());
                    for cluster in &clustered {
                        let split = crate::lyrics::yt_subs_split::split_cluster(
                            &self.ai_client,
                            &cluster.text,
                            cluster.start_ms,
                            cluster.end_ms,
                            &asr_words,
                        )
                        .await;
                        output.extend(split);
                    }
                    let provenance = match asr_opt.as_ref() {
                        Some(a) => format!("yt_subs+{}", a.provenance),
                        None => "yt_subs+timed-merge".into(),
                    };
                    return Ok(AlignedTrack {
                        lines: output,
                        provenance,
                        raw_confidence: asr_opt.map(|a| a.raw_confidence).unwrap_or(1.0),
                    });
                }

                // spotify / lrclib short-circuit (no ASR needed).
                info!(
                    provenance = %aligned_lines.provenance,
                    lines = aligned_lines.lines.len(),
                    "orchestrator: Tier-1 short-circuit (line-synced), routing to timed_reference_merge Mode B"
                );
                let candidate = aligned_lines_to_candidate(&aligned_lines);
                let song_duration_ms = candidate
                    .line_timings
                    .as_ref()
                    .and_then(|t| t.last())
                    .map(|(_, e)| (*e) as u32)
                    .unwrap_or(0);
                match timed_reference_merge::process(
                    None,
                    None,
                    &candidate,
                    song_duration_ms,
                    input.audit.as_ref(),
                )
                .await
                {
                    Ok(track) => Ok(track),
                    Err(e) => {
                        let fallback_lines = aligned_lines.lines;
                        let fallback_prov = aligned_lines.provenance;
                        tracing::warn!(
                            provenance = %fallback_prov,
                            error = %e,
                            "orchestrator: timed_reference_merge failed on LineSynced — falling back to split_track"
                        );
                        let pre_split = AlignedTrack {
                            lines: fallback_lines,
                            provenance: fallback_prov,
                            raw_confidence: 1.0,
                        };
                        Ok(split_track(&pre_split, self.split_cfg))
                    }
                }
            }
            Tier1Result::TextOnly(text_candidates) => {
                // Text-only path: run WhisperX for word timing, pick best
                // authoritative candidate, then route through
                // text_reference_merge::process (the unified text-merge
                // pipeline). Per the 2026-05-07 unification spec the
                // single-Claude-call merge in claude_merge::merge is retired.
                //
                // Provenance shape: `{best.source}+{asr.provenance}` (e.g.
                // "description+whisperx-large-v3@rev1", "genius+whisperx-large-v3@rev1").
                //
                // text_reference_merge runs its own Claude line-split (Phase 3)
                // internally — no external split_track wrap is needed when it
                // succeeds. On failure, fall back to split_track on raw
                // WhisperX so the song still ships timed lyrics.
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 TextOnly path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, input.language, &AlignOpts::default())
                    .await?;
                crate::lyrics::audit_ctx::write_whisperx_track(input.audit.as_ref(), &asr).await;

                let best = match best_authoritative_candidate(&text_candidates) {
                    Some(b) if !b.lines.is_empty() => b,
                    _ => {
                        info!(
                            provenance = %asr.provenance,
                            "orchestrator: TextOnly with no usable candidate — shipping raw WhisperX with line split"
                        );
                        return Ok(split_track(&asr, self.split_cfg));
                    }
                };

                let song_duration_ms = asr.lines.last().map(|l| l.end_ms).unwrap_or(0);

                if best.has_timing && coverage_ok(best, song_duration_ms) {
                    info!(
                        provenance = %asr.provenance,
                        best_source = %best.source,
                        song_duration_ms,
                        "orchestrator: Tier-1 TextOnly + timed candidate (coverage_ok) → timed_reference_merge Mode A"
                    );
                    match timed_reference_merge::process(
                        Some(self.ai_client.as_ref()),
                        Some(&asr),
                        best,
                        song_duration_ms,
                        input.audit.as_ref(),
                    )
                    .await
                    {
                        Ok(track) => return Ok(track),
                        Err(e) => {
                            tracing::warn!(
                                provenance = %asr.provenance,
                                best_source = %best.source,
                                error = %e,
                                "orchestrator: timed_reference_merge failed — retrying via text_reference_merge"
                            );
                            // fall through to the text_reference_merge branch below
                        }
                    }
                }

                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    text_candidates = text_candidates.len(),
                    best_source = %best.source,
                    best_has_timing = best.has_timing,
                    "orchestrator: Tier-1 TextOnly — backend called, routing to text_reference_merge"
                );

                match text_reference_merge::process(
                    &self.ai_client,
                    &asr,
                    best,
                    input.audit.as_ref(),
                )
                .await
                {
                    Ok(merged) => Ok(merged),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %asr.provenance,
                            best_source = %best.source,
                            error = %e,
                            "orchestrator: text_reference_merge failed — falling back to raw WhisperX with line split"
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
                    .align(wav, input.language, &AlignOpts::default())
                    .await?;
                crate::lyrics::audit_ctx::write_whisperx_track(input.audit.as_ref(), &asr).await;
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

/// Convert a `Tier1::LineSynced` payload into a timed `CandidateText` so
/// the orchestrator can route it through `timed_reference_merge::process`
/// (Mode B). Source is taken from the `AlignedLines.provenance` (which is
/// the original tier1 source label like `"tier1:spotify"`).
fn aligned_lines_to_candidate(
    aligned_lines: &crate::lyrics::tier1::AlignedLines,
) -> crate::lyrics::tier1::CandidateText {
    let lines: Vec<String> = aligned_lines.lines.iter().map(|l| l.text.clone()).collect();
    let line_timings: Vec<(u64, u64)> = aligned_lines
        .lines
        .iter()
        .map(|l| (l.start_ms as u64, l.end_ms as u64))
        .collect();
    crate::lyrics::tier1::CandidateText {
        source: aligned_lines.provenance.clone(),
        lines,
        line_timings: Some(line_timings),
        has_timing: true,
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
                fetchers: vec![fixed_fetcher(candidate)],
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
                fetchers: vec![fixed_fetcher(candidate)],
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
                fetchers: vec![fixed_fetcher(candidate)],
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
                fetchers: vec![empty_fetcher(), empty_fetcher()],
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
                fetchers: vec![],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
                audit: None,
            })
            .await
            .expect("process should succeed");

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(!result.provenance.contains("+claude-merge"));
    }
}
