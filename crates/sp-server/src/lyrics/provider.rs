//! Ensemble alignment provider interface and shared types.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Provider output types
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
    pub metadata: serde_json::Value,
}

// Song context (shared input to all providers)
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

// Merge output types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedWordTiming {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub confidence: f32,
    pub source_count: u8,
    pub spread_ms: u32,
}

// Audit log
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

// Provider trait
#[async_trait]
pub trait AlignmentProvider: Send + Sync {
    fn name(&self) -> &str;
    fn base_confidence(&self) -> f32;
    async fn can_provide(&self, ctx: &SongContext) -> bool;
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_timing_serde_roundtrip() {
        let word = WordTiming {
            text: "hallelujah".to_string(),
            start_ms: 1234,
            end_ms: 1890,
            confidence: 0.95,
        };
        let json = serde_json::to_string(&word).expect("serialize");
        let decoded: WordTiming = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, word);
    }

    #[test]
    fn provider_result_serde_roundtrip() {
        let result = ProviderResult {
            provider_name: "whisper".to_string(),
            lines: vec![LineTiming {
                text: "Amazing grace".to_string(),
                start_ms: 500,
                end_ms: 3200,
                words: vec![
                    WordTiming {
                        text: "Amazing".to_string(),
                        start_ms: 500,
                        end_ms: 1100,
                        confidence: 0.98,
                    },
                    WordTiming {
                        text: "grace".to_string(),
                        start_ms: 1200,
                        end_ms: 3200,
                        confidence: 0.97,
                    },
                ],
            }],
            metadata: serde_json::json!({"model": "large-v3", "language": "en"}),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let decoded: ProviderResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.provider_name, result.provider_name);
        assert_eq!(decoded.lines.len(), 1);
        assert_eq!(decoded.lines[0].words.len(), 2);
        assert_eq!(decoded.lines[0].words[0], result.lines[0].words[0]);
        assert_eq!(decoded.lines[0].words[1], result.lines[0].words[1]);
        assert_eq!(decoded.metadata["model"], "large-v3");
    }

    #[test]
    fn audit_log_serde_roundtrip() {
        let log = AuditLog {
            video_id: "dQw4w9WgXcQ".to_string(),
            timestamp: "2026-04-16T12:00:00Z".to_string(),
            reference_text_source: "lrclib".to_string(),
            providers_run: vec!["whisper".to_string(), "gentle".to_string()],
            providers_skipped: vec![("nemo".to_string(), "no vocal path".to_string())],
            per_word_details: vec![WordMergeDetail {
                word_index: 0,
                reference_text: "Amazing".to_string(),
                provider_estimates: vec![
                    ("whisper".to_string(), 500, 0.98),
                    ("gentle".to_string(), 510, 0.85),
                ],
                outliers_rejected: vec![],
                merged_start_ms: 505,
                merged_confidence: 0.91,
                spread_ms: 10,
            }],
            quality_metrics: QualityMetrics {
                avg_confidence: 0.91,
                words_with_zero_timing: 0,
                duplicate_start_pct: 0.0,
                gap_stddev_ms: 42.5,
            },
        };
        let json = serde_json::to_string(&log).expect("serialize");
        let decoded: AuditLog = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.video_id, log.video_id);
        assert_eq!(decoded.providers_run, log.providers_run);
        assert_eq!(decoded.providers_skipped.len(), 1);
        assert_eq!(decoded.providers_skipped[0].1, "no vocal path");
        assert_eq!(decoded.per_word_details[0].merged_start_ms, 505);
        assert_eq!(decoded.quality_metrics.gap_stddev_ms, 42.5);
    }
}
