//! Qwen3-ForcedAligner as an AlignmentProvider.
//!
//! Wraps the existing vocal-isolation + chunked-alignment pipeline
//! (aligner.rs, chunking.rs, assembly.rs) behind the trait interface.

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use sp_core::lyrics::{LyricsLine, LyricsTrack};

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
        // Qwen3 needs the clean vocal WAV (produced by preprocess_vocals)
        ctx.clean_vocal_path.is_some()
    }

    #[cfg_attr(test, mutants::skip)]
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
        let track = candidate_to_track(text);

        // Run existing chunking → alignment → assembly pipeline
        let chunks = crate::lyrics::chunking::plan_chunks(&track);

        // Create temp paths for chunk I/O JSON
        let chunks_json = ctx.audio_path.with_extension("qwen3_chunks.json");
        let aligned_json = ctx.audio_path.with_extension("qwen3_aligned.json");

        let chunk_results = crate::lyrics::aligner::align_chunks(
            &self.python_path,
            &self.script_path,
            clean_vocal,
            &chunks,
            &chunks_json,
            &aligned_json,
        )
        .await?;

        let aligned = crate::lyrics::assembly::assemble(track, chunk_results);

        // Convert LyricsTrack → ProviderResult
        Ok(track_to_provider_result(&aligned))
    }
}

/// Convert a CandidateText into a minimal LyricsTrack for chunking.
pub fn candidate_to_track(text: &CandidateText) -> LyricsTrack {
    let lines = text
        .lines
        .iter()
        .enumerate()
        .map(|(i, line_text)| {
            let (start, end) = text
                .line_timings
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
        })
        .collect();

    LyricsTrack {
        version: 1,
        source: text.source.clone(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines,
    }
}

/// Convert a LyricsTrack (with word timings from assembly) to a ProviderResult.
fn track_to_provider_result(track: &LyricsTrack) -> ProviderResult {
    ProviderResult {
        provider_name: "qwen3".into(),
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
                                confidence: 0.9, // Qwen3 base confidence
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect(),
        metadata: serde_json::json!({"source": track.source}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_to_track_preserves_lines() {
        let candidate = CandidateText {
            source: "manual_subs".into(),
            lines: vec!["Hello world".into(), "Second line".into()],
            has_timing: true,
            line_timings: Some(vec![(1000, 2000), (3000, 4000)]),
        };
        let track = candidate_to_track(&candidate);
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].en, "Hello world");
        assert_eq!(track.lines[0].start_ms, 1000);
        assert_eq!(track.lines[0].end_ms, 2000);
        assert_eq!(track.lines[1].start_ms, 3000);
    }

    #[test]
    fn candidate_to_track_without_timings() {
        let candidate = CandidateText {
            source: "lrclib".into(),
            lines: vec!["Line one".into()],
            has_timing: false,
            line_timings: None,
        };
        let track = candidate_to_track(&candidate);
        assert_eq!(track.lines[0].start_ms, 0);
        assert_eq!(track.lines[0].end_ms, 0);
    }

    #[test]
    fn track_to_provider_result_converts_words() {
        use sp_core::lyrics::{LyricsLine, LyricsWord};
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 1000,
                end_ms: 2000,
                en: "Hello".into(),
                sk: None,
                words: Some(vec![LyricsWord {
                    text: "Hello".into(),
                    start_ms: 1000,
                    end_ms: 1500,
                }]),
            }],
        };
        let result = track_to_provider_result(&track);
        assert_eq!(result.provider_name, "qwen3");
        assert_eq!(result.lines[0].words[0].confidence, 0.9);
        assert_eq!(result.lines[0].words[0].text, "Hello");
    }

    #[test]
    fn qwen3_provider_name_and_confidence() {
        let provider = Qwen3Provider {
            python_path: PathBuf::from("/usr/bin/python3"),
            script_path: PathBuf::from("/scripts/worker.py"),
            models_dir: PathBuf::from("/models"),
        };
        assert_eq!(provider.name(), "qwen3");
        assert_eq!(provider.base_confidence(), 0.9);
    }
}
