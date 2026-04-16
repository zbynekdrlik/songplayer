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

        // Reconcile candidate texts into one canonical reference via Claude text-merge
        let (reference_text, reference_source, _per_line_sources) =
            self.reconcile_reference_text(ctx).await?;

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

        // Single provider: pass through directly (no LLM merge needed).
        // LLM merge only adds value with 2+ providers giving conflicting timings.
        let (track, word_details) = if results.len() == 1 {
            info!(
                video_id = %ctx.video_id,
                provider = %results[0].provider_name,
                "single provider — passing through without LLM merge"
            );
            let pr = &results[0];
            let lines = pr
                .lines
                .iter()
                .map(|l| sp_core::lyrics::LyricsLine {
                    start_ms: l.start_ms,
                    end_ms: l.end_ms,
                    en: l.text.clone(),
                    sk: None,
                    words: Some(
                        l.words
                            .iter()
                            .map(|w| sp_core::lyrics::LyricsWord {
                                text: w.text.clone(),
                                start_ms: w.start_ms,
                                end_ms: w.end_ms,
                            })
                            .collect(),
                    ),
                })
                .collect();
            let track = LyricsTrack {
                version: 2,
                source: format!("ensemble:{}", pr.provider_name),
                language_source: "en".into(),
                language_translation: String::new(),
                lines,
            };
            let details: Vec<WordMergeDetail> = pr
                .lines
                .iter()
                .flat_map(|l| &l.words)
                .enumerate()
                .map(|(i, w)| WordMergeDetail {
                    word_index: i,
                    reference_text: w.text.clone(),
                    provider_estimates: vec![(pr.provider_name.clone(), w.start_ms, w.confidence)],
                    outliers_rejected: vec![],
                    merged_start_ms: w.start_ms,
                    merged_confidence: w.confidence * 0.7,
                    spread_ms: 0,
                })
                .collect();
            (track, details)
        } else {
            // 2+ providers: merge via LLM
            merge::merge_provider_results(
                &self.ai_client,
                &reference_text,
                &reference_source,
                &results,
            )
            .await
            .context("LLM merge failed")?
        };

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

    /// Reconcile candidate texts into one canonical reference via Claude text-merge.
    /// 0 candidates → error; 1 candidate → pass-through; 2+ → Claude merge.
    ///
    /// Returns `(joined_text, aggregated_source_label, per_line_sources)`:
    /// - `joined_text` — one string with lines separated by `\n`, suitable for
    ///   passing to the timing-merge prompt or a single-provider pass-through
    /// - `aggregated_source_label` — if all reconciled lines came from the same
    ///   candidate source, that source; otherwise `"merged:<s1>+<s2>+..."`
    /// - `per_line_sources` — same length as reconciled lines; useful for
    ///   audit-log provenance
    async fn reconcile_reference_text(
        &self,
        ctx: &SongContext,
    ) -> anyhow::Result<(String, String, Vec<String>)> {
        use crate::lyrics::text_merge::merge_candidate_texts;
        if ctx.candidate_texts.is_empty() {
            anyhow::bail!(
                "reconcile_reference_text: no candidates for {}",
                ctx.video_id
            );
        }
        let lines = merge_candidate_texts(&self.ai_client, &ctx.candidate_texts).await?;
        let joined = lines
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        // Aggregate per-line sources into the reference_source label.
        let agg_source = {
            let mut uniq: Vec<&String> = Vec::new();
            for l in &lines {
                if !uniq.contains(&&l.source) {
                    uniq.push(&l.source);
                }
            }
            if uniq.len() == 1 {
                uniq[0].clone()
            } else {
                format!(
                    "merged:{}",
                    uniq.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                )
            }
        };
        let per_line_sources = lines.iter().map(|l| l.source.clone()).collect();
        Ok((joined, agg_source, per_line_sources))
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
#[cfg_attr(test, mutants::skip)] // boundary `< 2` vs `<= 2` is semantically equivalent (single gap → stddev 0)
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

    #[tokio::test]
    async fn reconcile_reference_text_single_candidate_short_circuits() {
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
                source: "lrclib".into(),
                lines: vec!["only text".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: None,
            duration_ms: 180_000,
        };
        let (text, source, _) = orch.reconcile_reference_text(&ctx).await.unwrap();
        assert_eq!(text, "only text");
        assert_eq!(source, "lrclib");
    }

    #[tokio::test]
    async fn reconcile_reference_text_empty_is_error() {
        let orch = Orchestrator {
            providers: vec![],
            ai_client: Arc::new(AiClient::new(crate::ai::AiSettings::default())),
            cache_dir: PathBuf::from("/tmp"),
        };
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![],
            autosub_json3: None,
            duration_ms: 180_000,
        };
        assert!(orch.reconcile_reference_text(&ctx).await.is_err());
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
