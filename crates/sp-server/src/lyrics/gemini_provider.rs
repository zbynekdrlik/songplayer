//! Gemini-based AlignmentProvider. Slices the Demucs-dereverbed vocal WAV
//! into 60s chunks with 10s overlap, calls Gemini 3.1 Pro per chunk via the
//! `GeminiClient`, parses responses with `parse_timed_lines`, and merges per
//! `merge_overlap`. Produces line-level timings only (word vectors empty for
//! MVP — word-level work is deferred to a later PR).

use crate::lyrics::gemini_chunks::{merge_overlap, plan_chunks};
use crate::lyrics::gemini_client::GeminiClient;
use crate::lyrics::gemini_parse::parse_timed_lines;
use crate::lyrics::gemini_prompt::build_prompt;
use crate::lyrics::provider::{
    AlignmentProvider, CandidateText, LineTiming, ProviderResult, SongContext, WordTiming,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{debug, warn};

pub struct GeminiProvider {
    pub client: GeminiClient,
    pub ffmpeg_path: PathBuf,
    pub cache_dir: PathBuf,
}

#[async_trait]
impl AlignmentProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn base_confidence(&self) -> f32 {
        // Treated as the sole line-timing source while qwen3 is disabled.
        0.9
    }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        ctx.clean_vocal_path.as_ref().is_some_and(|p| p.exists())
    }

    // I/O-heavy orchestrator — per-chunk call+parse+merge pipeline. Individual
    // pieces (prompt builder, parser, chunk planner, merger, HTTP client) are
    // exhaustively unit-tested. The body here is glue + ffmpeg subprocess.
    #[cfg_attr(test, mutants::skip)]
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let vocal = ctx
            .clean_vocal_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gemini: clean_vocal_path missing"))?;
        let reference = gather_reference_text(&ctx.candidate_texts);
        let plans = plan_chunks(ctx.duration_ms);
        if plans.is_empty() {
            anyhow::bail!("gemini: duration_ms is 0, nothing to transcribe");
        }

        // Per-song tmp dir for chunk WAVs; cleaned on drop.
        let tmp = tempfile::tempdir().context("create chunk tmp dir")?;
        let mut per_chunk: Vec<Vec<crate::lyrics::gemini_parse::ParsedLine>> =
            Vec::with_capacity(plans.len());
        let mut raw_cache_entries: Vec<RawChunk> = Vec::with_capacity(plans.len());

        for (chunk_idx, plan) in plans.iter().enumerate() {
            // Pace between chunks: sleep 1 s before every chunk except the first.
            // This keeps the steady-state Gemini request rate at ~60 RPM, fitting
            // both free and paid quota tiers. (Retry backoff in GeminiClient may
            // briefly exceed this during a retry sequence — that is expected.)
            if chunk_idx > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
            let chunk_wav = tmp.path().join(format!("chunk_{:02}.wav", plan.idx));
            if let Err(e) = slice_chunk(
                &self.ffmpeg_path,
                vocal,
                plan.start_ms,
                plan.end_ms,
                &chunk_wav,
            )
            .await
            {
                warn!("gemini: chunk {} slice failed: {e}", plan.idx);
                per_chunk.push(Vec::new());
                raw_cache_entries.push(RawChunk {
                    start_ms: plan.start_ms,
                    end_ms: plan.end_ms,
                    raw: String::new(),
                });
                continue;
            }
            let prompt = build_prompt(&reference, plan.start_ms, plan.end_ms, ctx.duration_ms);
            debug!(chunk = plan.idx, "gemini: calling Gemini for chunk");
            match self.client.transcribe_chunk(&prompt, &chunk_wav).await {
                Ok(raw) => {
                    let parsed = parse_timed_lines(&raw);
                    debug!(
                        chunk = plan.idx,
                        parsed = parsed.len(),
                        "gemini: chunk parsed"
                    );
                    per_chunk.push(parsed);
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw,
                    });
                }
                Err(e) => {
                    warn!("gemini: chunk {} call failed: {e}", plan.idx);
                    per_chunk.push(Vec::new());
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw: String::new(),
                    });
                }
            }
        }

        // Write raw cache (best-effort; don't fail align on cache write error).
        if let Err(e) = write_raw_cache(&self.cache_dir, &ctx.video_id, &raw_cache_entries).await {
            warn!("gemini: raw cache write failed: {e}");
        }

        let merged = merge_overlap(&plans, &per_chunk);
        if merged.is_empty() {
            anyhow::bail!("gemini: no lines produced from any chunk");
        }

        let lines: Vec<LineTiming> = merged
            .into_iter()
            .map(|g| LineTiming {
                text: g.text,
                start_ms: g.start_ms,
                end_ms: g.end_ms,
                words: Vec::<WordTiming>::new(),
            })
            .collect();

        Ok(ProviderResult {
            provider_name: self.name().into(),
            lines,
            metadata: serde_json::json!({
                "base_confidence": self.base_confidence(),
                "chunks": plans.len(),
            }),
        })
    }
}

/// Pick the best reference text from candidate_texts. Prefers `source="description"`
/// (clean text from YouTube descriptions); falls back to whichever candidate has
/// the most lines. Returns a placeholder if no candidates are available.
pub fn gather_reference_text(candidates: &[CandidateText]) -> String {
    let pick = candidates
        .iter()
        .find(|c| c.source == "description")
        .or_else(|| candidates.iter().max_by_key(|c| c.lines.len()));
    match pick {
        Some(c) if !c.lines.is_empty() => c.lines.join("\n"),
        _ => "(no reference lyrics available for this song)".to_string(),
    }
}

// Pure ffmpeg subprocess wrapper — covered by manual verification + the ffmpeg
// binary's own tests. Mutation skip is for the I/O body.
#[cfg_attr(test, mutants::skip)]
async fn slice_chunk(
    ffmpeg: &Path,
    input: &Path,
    start_ms: u64,
    end_ms: u64,
    out: &Path,
) -> Result<()> {
    let dur_s = (end_ms - start_ms) as f64 / 1000.0;
    let ss_s = start_ms as f64 / 1000.0;
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-ss",
        &format!("{ss_s}"),
        "-t",
        &format!("{dur_s}"),
        "-i",
    ])
    .arg(input)
    .args(["-c:a", "pcm_s16le"])
    .arg(out)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().await.context("run ffmpeg for chunk slice")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg failed: {err}");
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct RawChunk {
    start_ms: u64,
    end_ms: u64,
    raw: String,
}

// Disk I/O — mutation skip. Covered by manual test (file exists on disk after run)
// and by the raw-chunk integration with the retry-from-cache path.
#[cfg_attr(test, mutants::skip)]
async fn write_raw_cache(cache_dir: &Path, video_id: &str, chunks: &[RawChunk]) -> Result<()> {
    let path = cache_dir.join(format!("{video_id}_gemini_chunks.json"));
    let body = serde_json::json!({"chunks": chunks});
    tokio::fs::write(&path, serde_json::to_string_pretty(&body)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::provider::CandidateText;

    fn ctext(source: &str, lines: Vec<&str>) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: lines.into_iter().map(String::from).collect(),
            has_timing: false,
            line_timings: None,
        }
    }

    #[test]
    fn gather_reference_text_prefers_description() {
        let cands = vec![
            ctext("autosub", vec!["a"]),
            ctext("description", vec!["b", "c"]),
        ];
        assert_eq!(gather_reference_text(&cands), "b\nc");
    }

    #[test]
    fn gather_reference_text_falls_back_to_longest() {
        let cands = vec![ctext("autosub", vec!["a", "b"]), ctext("lrclib", vec!["c"])];
        assert_eq!(gather_reference_text(&cands), "a\nb");
    }

    #[test]
    fn gather_reference_text_empty_returns_placeholder() {
        assert!(gather_reference_text(&[]).contains("no reference"));
    }

    #[test]
    fn gather_reference_text_all_empty_returns_placeholder() {
        let cands = vec![ctext("description", vec![]), ctext("autosub", vec![])];
        assert!(gather_reference_text(&cands).contains("no reference"));
    }
}
