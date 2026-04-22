//! Alignment provider that short-circuits Gemini when the gather phase produced
//! a YouTube manual-subs candidate with line-level timing. Per
//! `feedback_no_autosub.md` this NEVER accepts auto-subs — only human-authored
//! manual subs, detected via `has_timing = true` on the `yt_subs` candidate
//! (auto-subs never land in candidate_texts with timing).
//!
//! Output: ProviderResult { provider_name: "yt_subs" }. Downstream reporting
//! labels the resulting song `source = "yt_subs"` so existing ensemble tests
//! that scan for `ensemble:*` sources are not affected.

use anyhow::Result;
use async_trait::async_trait;

use crate::lyrics::provider::{
    AlignmentProvider, CandidateText, LineTiming, ProviderResult, SongContext,
};

pub struct YtManualSubsProvider;

#[async_trait]
impl AlignmentProvider for YtManualSubsProvider {
    fn name(&self) -> &str {
        "yt_subs"
    }
    fn base_confidence(&self) -> f32 {
        0.95
    }
    async fn can_provide(&self, ctx: &SongContext) -> bool {
        find_yt_subs_with_timing(&ctx.candidate_texts).is_some()
    }
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let (lines, timings) = find_yt_subs_with_timing(&ctx.candidate_texts)
            .ok_or_else(|| anyhow::anyhow!("yt_subs candidate with timing unavailable"))?;
        let line_timings: Vec<LineTiming> = lines
            .iter()
            .zip(timings.iter())
            .map(|(text, (start, end))| LineTiming {
                text: text.clone(),
                start_ms: *start,
                end_ms: *end,
                words: Vec::new(),
            })
            .collect();
        Ok(ProviderResult {
            provider_name: "yt_subs".to_string(),
            lines: line_timings,
            metadata: serde_json::json!({"base_confidence": 0.95}),
        })
    }
}

fn find_yt_subs_with_timing(
    candidates: &[CandidateText],
) -> Option<(Vec<String>, Vec<(u64, u64)>)> {
    candidates.iter().find_map(|c| {
        if c.source == "yt_subs" && c.has_timing {
            let timings = c.line_timings.clone()?;
            if timings.len() != c.lines.len() {
                return None;
            }
            Some((c.lines.clone(), timings))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn subs_with_timing() -> CandidateText {
        CandidateText {
            source: "yt_subs".to_string(),
            lines: vec!["line one".to_string(), "line two".to_string()],
            has_timing: true,
            line_timings: Some(vec![(0, 2000), (2000, 4500)]),
        }
    }

    fn subs_no_timing() -> CandidateText {
        CandidateText {
            source: "yt_subs".to_string(),
            lines: vec!["line".to_string()],
            has_timing: false,
            line_timings: None,
        }
    }

    fn fake_ctx(cands: Vec<CandidateText>) -> SongContext {
        SongContext {
            video_id: "vid".to_string(),
            audio_path: PathBuf::new(),
            clean_vocal_path: None,
            candidate_texts: cands,
            autosub_json3: None,
            duration_ms: 10_000,
        }
    }

    #[tokio::test]
    async fn can_provide_true_when_yt_subs_has_timing() {
        let p = YtManualSubsProvider;
        assert!(p.can_provide(&fake_ctx(vec![subs_with_timing()])).await);
    }

    #[tokio::test]
    async fn can_provide_false_when_no_yt_subs_candidate() {
        let p = YtManualSubsProvider;
        let ctx = fake_ctx(vec![CandidateText {
            source: "description".to_string(),
            lines: vec!["x".to_string()],
            has_timing: false,
            line_timings: None,
        }]);
        assert!(!p.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn can_provide_false_when_yt_subs_has_no_timing() {
        let p = YtManualSubsProvider;
        assert!(!p.can_provide(&fake_ctx(vec![subs_no_timing()])).await);
    }

    #[tokio::test]
    async fn align_produces_line_timings_from_yt_subs() {
        let p = YtManualSubsProvider;
        let out = p
            .align(&fake_ctx(vec![subs_with_timing()]))
            .await
            .expect("align ok");
        assert_eq!(out.provider_name, "yt_subs");
        assert_eq!(out.lines.len(), 2);
        assert_eq!(out.lines[0].text, "line one");
        assert_eq!(out.lines[0].start_ms, 0);
        assert_eq!(out.lines[0].end_ms, 2000);
        assert!(out.lines[0].words.is_empty(), "line-level only, no words");
        assert_eq!(out.lines[1].start_ms, 2000);
        assert_eq!(out.lines[1].end_ms, 4500);
    }

    #[tokio::test]
    async fn align_errors_when_no_timing_present() {
        let p = YtManualSubsProvider;
        let err = p.align(&fake_ctx(vec![subs_no_timing()])).await.err();
        assert!(
            err.is_some(),
            "align must error when no yt_subs timing is available"
        );
    }

    #[tokio::test]
    async fn find_yt_subs_rejects_length_mismatch() {
        // Defensive: if upstream gives us lines and timings of different
        // lengths, we refuse to pair them silently.
        let bad = CandidateText {
            source: "yt_subs".to_string(),
            lines: vec!["a".to_string(), "b".to_string()],
            has_timing: true,
            line_timings: Some(vec![(0, 1000)]), // only one!
        };
        assert!(find_yt_subs_with_timing(&[bad]).is_none());
    }
}
