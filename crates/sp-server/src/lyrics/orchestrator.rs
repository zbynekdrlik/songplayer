//! Per-song ensemble alignment orchestrator.
//!
//! Coordinates: gather text sources → run alignment providers →
//! merge via LLM → translate → quality gate.

use anyhow::{Context, Result};
use sp_core::lyrics::LyricsTrack;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::ai::client::AiClient;
use crate::lyrics::merge;
use crate::lyrics::provider::*;

pub struct Orchestrator {
    providers: Vec<Box<dyn AlignmentProvider>>,
    ai_client: Arc<AiClient>,
    cache_dir: PathBuf,
}

impl Orchestrator {
    pub fn new(
        providers: Vec<Box<dyn AlignmentProvider>>,
        ai_client: Arc<AiClient>,
        cache_dir: PathBuf,
    ) -> Self {
        Self {
            providers,
            ai_client,
            cache_dir,
        }
    }

    /// Run the full ensemble pipeline for one song.
    #[cfg_attr(test, mutants::skip)]
    pub async fn process_song(&self, ctx: &SongContext) -> Result<LyricsTrack> {
        info!(
            video_id = %ctx.video_id,
            "orchestrator: starting ensemble alignment"
        );

        // Pick best reference text
        let (reference_text, reference_source) = self.select_reference_text(ctx);

        // Run providers sequentially (cheapest first, ordered by registration)
        let mut results: Vec<ProviderResult> = Vec::new();
        let mut skipped: Vec<(String, String)> = Vec::new();

        for provider in &self.providers {
            if !provider.can_provide(ctx).await {
                skipped.push((provider.name().into(), "can_provide returned false".into()));
                continue;
            }

            debug!(
                video_id = %ctx.video_id,
                provider = provider.name(),
                "running alignment provider"
            );

            match provider.align(ctx).await {
                Ok(result) => {
                    info!(
                        video_id = %ctx.video_id,
                        provider = provider.name(),
                        lines = result.lines.len(),
                        "provider completed"
                    );
                    results.push(result);
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
            anyhow::bail!("no providers produced results for {}", ctx.video_id);
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
        let total_words = word_details.len().max(1);
        let quality = QualityMetrics {
            avg_confidence: word_details
                .iter()
                .map(|d| d.merged_confidence)
                .sum::<f32>()
                / total_words as f32,
            words_with_zero_timing: word_details
                .iter()
                .filter(|d| d.merged_start_ms == 0)
                .count(),
            duplicate_start_pct: compute_duplicate_start_pct(&track),
            gap_stddev_ms: compute_gap_stddev_ms(&track),
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
        if let Err(e) = merge::write_audit_log(&self.cache_dir, &audit).await {
            warn!(video_id = %ctx.video_id, error = %e, "failed to write audit log");
        }

        info!(
            video_id = %ctx.video_id,
            providers = results.len(),
            avg_confidence = audit.quality_metrics.avg_confidence,
            "orchestrator: ensemble alignment complete"
        );

        Ok(track)
    }

    /// Select the best reference text from candidates.
    /// Priority: ccli > manual_subs > description > lrclib > autosub
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

/// Compute the percentage of words sharing the same start_ms within a track.
/// High duplicate_start_pct indicates degenerate alignment.
fn compute_duplicate_start_pct(track: &LyricsTrack) -> f32 {
    let mut starts: Vec<u64> = Vec::new();
    for line in &track.lines {
        if let Some(words) = &line.words {
            for w in words {
                starts.push(w.start_ms);
            }
        }
    }
    if starts.is_empty() {
        return 0.0;
    }
    starts.sort();
    let duplicates = starts.windows(2).filter(|w| w[0] == w[1]).count();
    (duplicates as f32 / starts.len() as f32) * 100.0
}

/// Compute the standard deviation of gaps between consecutive word start times.
fn compute_gap_stddev_ms(track: &LyricsTrack) -> f32 {
    let mut starts: Vec<u64> = Vec::new();
    for line in &track.lines {
        if let Some(words) = &line.words {
            for w in words {
                starts.push(w.start_ms);
            }
        }
    }
    if starts.len() < 2 {
        return 0.0;
    }
    let gaps: Vec<f64> = starts
        .windows(2)
        .map(|w| (w[1] as f64) - (w[0] as f64))
        .collect();
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    let variance = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gaps.len() as f64;
    variance.sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_reference_text_priority() {
        let orch = Orchestrator {
            providers: vec![],
            ai_client: Arc::new(AiClient::new(crate::ai::AiSettings::default())),
            cache_dir: PathBuf::from("/tmp"),
        };
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![
                CandidateText {
                    source: "autosub".into(),
                    lines: vec!["autosub text".into()],
                    has_timing: false,
                    line_timings: None,
                },
                CandidateText {
                    source: "manual_subs".into(),
                    lines: vec!["manual text".into()],
                    has_timing: true,
                    line_timings: None,
                },
            ],
            autosub_json3: None,
            duration_ms: 180000,
        };
        let (text, source) = orch.select_reference_text(&ctx);
        assert_eq!(source, "manual_subs");
        assert_eq!(text, "manual text");
    }

    #[test]
    fn select_reference_text_fallback() {
        let orch = Orchestrator {
            providers: vec![],
            ai_client: Arc::new(AiClient::new(crate::ai::AiSettings::default())),
            cache_dir: PathBuf::from("/tmp"),
        };
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "unknown_source".into(),
                lines: vec!["fallback text".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: None,
            duration_ms: 180000,
        };
        let (text, source) = orch.select_reference_text(&ctx);
        assert_eq!(source, "unknown_source");
        assert_eq!(text, "fallback text");
    }

    #[test]
    fn compute_duplicate_start_pct_basic() {
        use sp_core::lyrics::{LyricsLine, LyricsWord};
        let track = LyricsTrack {
            version: 2,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 0,
                end_ms: 3000,
                en: "a b c d".into(),
                sk: None,
                words: Some(vec![
                    LyricsWord {
                        text: "a".into(),
                        start_ms: 1000,
                        end_ms: 1500,
                    },
                    LyricsWord {
                        text: "b".into(),
                        start_ms: 1000,
                        end_ms: 1800,
                    }, // duplicate
                    LyricsWord {
                        text: "c".into(),
                        start_ms: 2000,
                        end_ms: 2500,
                    },
                    LyricsWord {
                        text: "d".into(),
                        start_ms: 3000,
                        end_ms: 3500,
                    },
                ]),
            }],
        };
        let pct = compute_duplicate_start_pct(&track);
        // 1 duplicate out of 4 words = 25%
        assert!((pct - 25.0).abs() < 0.1);
    }

    #[test]
    fn compute_gap_stddev_no_words() {
        let track = LyricsTrack {
            version: 2,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![],
        };
        assert_eq!(compute_gap_stddev_ms(&track), 0.0);
    }

    #[test]
    fn compute_gap_stddev_with_varying_gaps() {
        use sp_core::lyrics::{LyricsLine, LyricsWord};
        // Words at: 0, 100, 300, 600 → gaps: 100, 200, 300
        // mean gap = 200, variance = ((100-200)^2 + (200-200)^2 + (300-200)^2) / 3
        //          = (10000 + 0 + 10000) / 3 = 6666.67
        // stddev = sqrt(6666.67) ≈ 81.65
        let track = LyricsTrack {
            version: 2,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 0,
                end_ms: 700,
                en: "a b c d".into(),
                sk: None,
                words: Some(vec![
                    LyricsWord {
                        text: "a".into(),
                        start_ms: 0,
                        end_ms: 90,
                    },
                    LyricsWord {
                        text: "b".into(),
                        start_ms: 100,
                        end_ms: 290,
                    },
                    LyricsWord {
                        text: "c".into(),
                        start_ms: 300,
                        end_ms: 590,
                    },
                    LyricsWord {
                        text: "d".into(),
                        start_ms: 600,
                        end_ms: 700,
                    },
                ]),
            }],
        };
        let stddev = compute_gap_stddev_ms(&track);
        assert!(
            (stddev - 81.65).abs() < 0.1,
            "expected ~81.65, got {stddev}"
        );
    }

    #[test]
    fn compute_gap_stddev_uniform_gaps_is_zero() {
        use sp_core::lyrics::{LyricsLine, LyricsWord};
        // Words at: 0, 500, 1000 → gaps: 500, 500 → stddev = 0
        let track = LyricsTrack {
            version: 2,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 0,
                end_ms: 1100,
                en: "a b c".into(),
                sk: None,
                words: Some(vec![
                    LyricsWord {
                        text: "a".into(),
                        start_ms: 0,
                        end_ms: 400,
                    },
                    LyricsWord {
                        text: "b".into(),
                        start_ms: 500,
                        end_ms: 900,
                    },
                    LyricsWord {
                        text: "c".into(),
                        start_ms: 1000,
                        end_ms: 1100,
                    },
                ]),
            }],
        };
        let stddev = compute_gap_stddev_ms(&track);
        assert!(stddev.abs() < 0.01, "expected 0, got {stddev}");
    }
}
