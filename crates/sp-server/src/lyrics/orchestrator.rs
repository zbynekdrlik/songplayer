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
            // Quality gate: reject Gemini's uniform-duration hallucinations
            // before they reach the wall. "Saints" (80 lines, 11 unique
            // durations) and similar fabrications get short-circuited here
            // so the song ships as `no_source` instead of fake timings.
            if !duration_histogram_ok(&pr.lines) {
                let mut uniques: Vec<u64> =
                    pr.lines.iter().map(|l| l.end_ms - l.start_ms).collect();
                uniques.sort_unstable();
                uniques.dedup();
                warn!(
                    video_id = %ctx.video_id,
                    provider = %pr.provider_name,
                    total_lines = pr.lines.len(),
                    unique_durations = uniques.len(),
                    "rejecting provider output: duration histogram too collapsed \
                     (Gemini hallucination signature)"
                );
                anyhow::bail!(
                    "duration histogram failed for {} ({} lines, {} unique durations)",
                    ctx.video_id,
                    pr.lines.len(),
                    uniques.len()
                );
            }
            // Single shared helper for cross-line-aware sanitize — same
            // call site as `merge_provider_results`. Keeps the strict-
            // increasing-starts invariant in one place.
            let lines = crate::lyrics::merge::sanitize_track(&pr.lines, ctx.duration_ms);
            let track = LyricsTrack {
                version: 2,
                source: format!("ensemble:{}", pr.provider_name),
                language_source: "en".into(),
                language_translation: String::new(),
                lines,
            };
            // Build word_details from SANITIZED track so quality_score
            // reflects what actually got persisted, not the raw input.
            // Use the shared pass_through_baseline helper so the constant
            // stays in one place (merge.rs). `base_confidence_of` reads
            // the provider's declared base confidence; falls back to 0.7.
            let base_conf = crate::lyrics::merge::base_confidence_of(pr);
            let pass_through_c = crate::lyrics::merge::pass_through_baseline(base_conf);
            let details: Vec<WordMergeDetail> = track
                .lines
                .iter()
                .flat_map(|l| l.words.as_deref().unwrap_or(&[]))
                .enumerate()
                .map(|(i, w)| WordMergeDetail {
                    word_index: i,
                    reference_text: w.text.clone(),
                    provider_estimates: vec![(pr.provider_name.clone(), w.start_ms, base_conf)],
                    outliers_rejected: vec![],
                    merged_start_ms: w.start_ms,
                    merged_confidence: pass_through_c,
                    spread_ms: 0,
                })
                .collect();
            (track, details)
        } else {
            // 2+ providers: deterministic Rust merge (see lyrics/merge.rs
            // — no LLM call, pure math on provider timings).
            merge::merge_provider_results(
                &self.ai_client,
                &reference_text,
                &reference_source,
                &results,
                ctx.duration_ms,
            )
            .await
            .context("ensemble merge failed")?
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

/// Quality gate against Gemini duration-hallucination. Returns `false` when
/// the provider produced 20+ lines with ≤ 8 distinct `(end_ms - start_ms)`
/// values — the signature of an LLM that fabricated uniform timings
/// instead of listening to audio (the 2026-04-23 "Saints" failure mode:
/// 80 lines all at 1400 ms). Below the 20-line floor the metric is too
/// noisy to be meaningful so we pass through.
///
/// Keeping the gate this lenient: real alignments on 60+ lines routinely
/// produce 30+ distinct durations; anything under 9 uniques is a smell
/// regardless of content.
///
/// Known false-positive class: chant-heavy worship songs whose chorus is
/// genuinely a uniform-cadence repeat (e.g. WOMP WOMP — 142 lines, 15
/// uniques, real chant is ~50 of those lines at ~1.5 s each). Issue #52
/// tracks adding `SpotifyLyricsProvider` to short-circuit Gemini for songs
/// where Spotify already has authoritative LINE_SYNCED timings — that's
/// the right fix, not loosening this gate.
fn duration_histogram_ok(lines: &[LineTiming]) -> bool {
    if lines.len() < 20 {
        return true;
    }
    // Gemini's native output is decisecond-precision (`(MM:SS.x)`), so every
    // legitimate duration is already a multiple of 100 ms — the signal we
    // use isn't "roundness" but *sparsity* of distinct values relative to
    // the number of lines. A real 80-line song has 30+ distinct durations;
    // the Saints hallucination had only 11. Threshold scales with line
    // count so short tracks with unavoidable chorus repetition still pass.
    let mut unique: Vec<u64> = lines.iter().map(|l| l.end_ms - l.start_ms).collect();
    unique.sort_unstable();
    unique.dedup();
    let required = (lines.len() / 6).max(8);
    unique.len() > required
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

    #[tokio::test]
    async fn reconcile_reference_text_multi_source_produces_merged_label() {
        // Two candidates → Claude mocked to return lines from different sources.
        // Verifies the aggregate-source label is "merged:lrclib+yt_subs".
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"content": "{\"lines\":[{\"text\":\"line one\",\"source\":\"lrclib\"},{\"text\":\"line two\",\"source\":\"yt_subs\"}]}"}
                }]
            })))
            .mount(&mock).await;

        let orch = Orchestrator {
            providers: vec![],
            ai_client: Arc::new(AiClient::new(crate::ai::AiSettings {
                api_url: format!("{}/v1", mock.uri()),
                api_key: Some("test".into()),
                model: "claude-opus-4-20250514".into(),
                system_prompt_extra: None,
            })),
            cache_dir: PathBuf::from("/tmp"),
        };
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![
                CandidateText {
                    source: "lrclib".into(),
                    lines: vec!["line one".into(), "line two".into()],
                    has_timing: false,
                    line_timings: None,
                },
                CandidateText {
                    source: "yt_subs".into(),
                    lines: vec!["line one".into(), "line two".into()],
                    has_timing: false,
                    line_timings: None,
                },
            ],
            autosub_json3: None,
            duration_ms: 180_000,
        };
        let (text, source, per_line) = orch.reconcile_reference_text(&ctx).await.unwrap();
        assert_eq!(text, "line one\nline two");
        assert_eq!(
            source, "merged:lrclib+yt_subs",
            "multi-source aggregation must produce merged:x+y label"
        );
        assert_eq!(per_line, vec!["lrclib".to_string(), "yt_subs".to_string()]);
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

    /// 80 lines all with the exact same 1400 ms duration — the "Saints"
    /// signature from 2026-04-23 event where Gemini fabricated uniform
    /// timings. Must be rejected so the song ships as `no_source` instead
    /// of going out on the wall with fake timings.
    #[test]
    fn duration_histogram_ok_rejects_uniform_1400ms() {
        let lines: Vec<LineTiming> = (0..80)
            .map(|i| LineTiming {
                text: format!("line {i}"),
                start_ms: i * 1400,
                end_ms: i * 1400 + 1400,
                words: vec![],
            })
            .collect();
        assert!(
            !duration_histogram_ok(&lines),
            "80 lines with 1 unique duration must fail the gate"
        );
    }

    /// 84 lines across 11 round durations (multiples of 100 ms). Still fails
    /// the ≤ 8 threshold. This protects against a Gemini failure mode
    /// somewhere between "all lines 1.4s" and "legitimately varied".
    #[test]
    fn duration_histogram_ok_rejects_eleven_round_values() {
        let durations = [
            1400u64, 2000, 1500, 1000, 500, 2500, 3000, 3500, 400, 700, 1800,
        ];
        // Produce at least 20 lines so the 20-line floor doesn't short-circuit.
        let lines: Vec<LineTiming> = (0..84)
            .map(|i| {
                let dur = durations[i % durations.len()];
                LineTiming {
                    text: format!("l{i}"),
                    start_ms: (i as u64) * 4_000,
                    end_ms: (i as u64) * 4_000 + dur,
                    words: vec![],
                }
            })
            .collect();
        assert!(
            !duration_histogram_ok(&lines),
            "84 lines across 11 round multiples of 100 ms must fail the gate"
        );
    }

    /// 80 lines with 20+ distinct durations — what a real alignment looks
    /// like when Gemini is actually listening to the audio. Must pass.
    #[test]
    fn duration_histogram_ok_accepts_varied_alignment() {
        let lines: Vec<LineTiming> = (0..80)
            .map(|i| LineTiming {
                text: format!("l{i}"),
                start_ms: (i as u64) * 3_000,
                // 42 distinct durations by varying i in a non-repeating mod
                // pattern — 800 ms base + 0..41 * 37 ms.
                end_ms: (i as u64) * 3_000 + 800 + (i as u64 % 42) * 37,
                words: vec![],
            })
            .collect();
        assert!(
            duration_histogram_ok(&lines),
            "80 lines with 20+ unique durations must pass"
        );
    }

    /// Below the 20-line floor, the metric isn't meaningful. A short song
    /// with 12 lines all at 1400 ms should NOT be rejected — we have no
    /// evidence it's a Gemini hallucination (real short interludes do have
    /// repetitive timings).
    #[test]
    fn duration_histogram_ok_passes_through_short_tracks() {
        let lines: Vec<LineTiming> = (0..12)
            .map(|i| LineTiming {
                text: format!("l{i}"),
                start_ms: (i as u64) * 1_500,
                end_ms: (i as u64) * 1_500 + 1_400,
                words: vec![],
            })
            .collect();
        assert!(
            duration_histogram_ok(&lines),
            "< 20 lines should pass through without the metric being applied"
        );
    }

    /// Boundary test for the `< 20` short-circuit in `duration_histogram_ok`.
    /// Exactly 20 lines with ONE unique duration — below the floor the
    /// metric must NOT apply, so this must still be rejected by the
    /// sparsity check (required=8, unique=1 → 1 > 8 is false → reject).
    /// This kills the `<` → `<=` mutant on line 273 (with `<=`, 20 lines
    /// would short-circuit as "too short to evaluate" and return true,
    /// letting uniform timings ship).
    #[test]
    fn duration_histogram_ok_boundary_at_twenty_lines_is_evaluated() {
        let lines: Vec<LineTiming> = (0..20)
            .map(|i| LineTiming {
                text: format!("line {i}"),
                start_ms: (i as u64) * 1400,
                end_ms: (i as u64) * 1400 + 1400, // uniform 1400 ms
                words: vec![],
            })
            .collect();
        assert!(
            !duration_histogram_ok(&lines),
            "at exactly 20 lines the histogram gate MUST evaluate — 1 unique duration must fail"
        );
    }

    /// Boundary test for the `unique.len() > required` comparison on line
    /// 286. With 48 lines, `required = max(48/6, 8) = 8`. Build exactly 8
    /// unique durations. Real: `8 > 8` is false → reject. Mutant `>=`:
    /// `8 >= 8` is true → accept (wrong — this is the Gemini failure mode
    /// boundary we built the gate to catch).
    #[test]
    fn duration_histogram_ok_rejects_exactly_required_unique_count() {
        let durations = [1000u64, 1200, 1400, 1600, 1800, 2000, 2200, 2400];
        assert_eq!(durations.len(), 8);
        let lines: Vec<LineTiming> = (0..48)
            .map(|i| {
                let dur = durations[i % durations.len()];
                LineTiming {
                    text: format!("l{i}"),
                    start_ms: (i as u64) * 4_000,
                    end_ms: (i as u64) * 4_000 + dur,
                    words: vec![],
                }
            })
            .collect();
        // 48 / 6 = 8, max(8, 8) = 8; unique count is also 8, so strictly
        // 8 > 8 is false → reject. Mutant `>=` would accept.
        assert!(
            !duration_histogram_ok(&lines),
            "48 lines with exactly 8 unique durations (=required) must be rejected by strict >"
        );
    }

    /// Partner test to the `< 20` boundary — 19 lines (just under the floor)
    /// MUST short-circuit to true regardless of uniqueness. Documents the
    /// intended short-circuit behavior but does NOT kill the `<=` mutant
    /// alone (on `<=` 19 is still <= 20 → true). Kept for completeness with
    /// the 20-line test above; together they pin both sides of the boundary.
    #[test]
    fn duration_histogram_ok_boundary_at_nineteen_lines_short_circuits() {
        let lines: Vec<LineTiming> = (0..19)
            .map(|i| LineTiming {
                text: format!("line {i}"),
                start_ms: (i as u64) * 1400,
                end_ms: (i as u64) * 1400 + 1400,
                words: vec![],
            })
            .collect();
        assert!(
            duration_histogram_ok(&lines),
            "19 lines must short-circuit to true — below the 20-line evaluation floor"
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
