//! Rust subprocess wrappers for `lyrics_worker.py`.
//!
//! Two entry points:
//!   - `preprocess_vocals(flac) → clean_wav`: Mel-Roformer + anvuew + 16 kHz
//!   - `align_chunks(wav, chunks) → ChunkResults`: chunked Qwen3 alignment
//!
//! No post-processing, no band-aid, no duplicate-timing fixups. The
//! assembly and quality modules in this crate own all data shaping.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;
use tracing::debug;

use crate::lyrics::assembly::{AlignedWord, ChunkResult};
use crate::lyrics::chunking::ChunkRequest;

// ---------------------------------------------------------------------------
// On-disk JSON shapes shared with Python
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChunkInRequest<'a> {
    chunk_idx: usize,
    start_ms: u64,
    end_ms: u64,
    text: &'a str,
    word_count: usize,
}

#[derive(Debug, Serialize)]
struct ChunkRequestFile<'a> {
    chunks: Vec<ChunkInRequest<'a>>,
}

#[derive(Debug, Deserialize)]
struct ChunkOutWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ChunkOut {
    chunk_idx: usize,
    words: Vec<ChunkOutWord>,
}

#[derive(Debug, Deserialize)]
struct ChunkResultFile {
    chunks: Vec<ChunkOut>,
}

// ---------------------------------------------------------------------------
// preprocess_vocals
// ---------------------------------------------------------------------------

/// Run Mel-Roformer vocal isolation + anvuew de-reverb + 16 kHz mono float32
/// resample on `audio_in`. Writes the clean WAV to `wav_out` and returns
/// the same path on success.
#[cfg_attr(test, mutants::skip)]
pub async fn preprocess_vocals(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    wav_out: &Path,
) -> Result<PathBuf> {
    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "preprocess-vocals".as_ref(),
        "--audio".as_ref(),
        audio_in.as_os_str(),
        "--output".as_ref(),
        wav_out.as_os_str(),
        "--models-dir".as_ref(),
        models_dir.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "running preprocess-vocals: {} --audio {} --output {}",
        python_path.display(),
        audio_in.display(),
        wav_out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn preprocess-vocals")?;
    let status = match tokio::time::timeout(std::time::Duration::from_secs(600), child.wait()).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("preprocess-vocals wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("preprocess-vocals timed out after 600 s");
        }
    };
    if !status.success() {
        anyhow::bail!("preprocess-vocals exited with status {status}");
    }
    Ok(wav_out.to_path_buf())
}

// ---------------------------------------------------------------------------
// align_chunks
// ---------------------------------------------------------------------------

/// Write `requests` to a temp file, invoke `lyrics_worker.py align-chunks`
/// on the clean WAV, parse the result JSON, and return `ChunkResult`s.
///
/// `chunks_path` and `output_path` are caller-owned scratch files that
/// this function writes and then removes on success.
#[cfg_attr(test, mutants::skip)]
pub async fn align_chunks(
    python_path: &Path,
    script_path: &Path,
    audio_wav: &Path,
    requests: &[ChunkRequest],
    chunks_path: &Path,
    output_path: &Path,
) -> Result<Vec<ChunkResult>> {
    let req_file = ChunkRequestFile {
        chunks: requests
            .iter()
            .enumerate()
            .map(|(idx, r)| ChunkInRequest {
                chunk_idx: idx,
                start_ms: r.start_ms,
                end_ms: r.end_ms,
                text: &r.text,
                word_count: r.word_count,
            })
            .collect(),
    };
    let json = serde_json::to_vec(&req_file)?;
    fs::write(chunks_path, &json)
        .await
        .context("failed to write chunks request file")?;

    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "align-chunks".as_ref(),
        "--audio".as_ref(),
        audio_wav.as_os_str(),
        "--chunks".as_ref(),
        chunks_path.as_os_str(),
        "--output".as_ref(),
        output_path.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "running align-chunks with {} requests on {}",
        requests.len(),
        audio_wav.display()
    );

    let mut child = cmd.spawn().context("failed to spawn align-chunks")?;
    let status = match tokio::time::timeout(std::time::Duration::from_secs(900), child.wait()).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("align-chunks wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("align-chunks timed out after 900 s");
        }
    };
    if !status.success() {
        anyhow::bail!("align-chunks exited with status {status}");
    }

    let content = fs::read_to_string(output_path)
        .await
        .context("failed to read align-chunks output")?;
    let parsed: ChunkResultFile =
        serde_json::from_str(&content).context("failed to parse align-chunks output JSON")?;

    let results = parsed
        .chunks
        .into_iter()
        .map(|c| {
            let line_index = requests
                .get(c.chunk_idx)
                .map(|r| r.line_index)
                .unwrap_or(usize::MAX);
            ChunkResult {
                line_index,
                words: c
                    .words
                    .into_iter()
                    .map(|w| AlignedWord {
                        text: w.text,
                        start_ms: w.start_ms,
                        end_ms: w.end_ms,
                    })
                    .collect(),
            }
        })
        .filter(|r| r.line_index != usize::MAX)
        .collect();

    let _ = fs::remove_file(chunks_path).await;
    let _ = fs::remove_file(output_path).await;

    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Audit: retired symbols must no longer be referenced from this file.
    /// Keeps the compiler from being the only line of defence against a
    /// dangling `pub use aligner::align_lyrics` re-export leaking back in.
    #[test]
    fn aligner_source_has_no_retired_symbols() {
        let src = include_str!("aligner.rs");
        for banned in [
            "align_lyrics",
            "merge_word_timings",
            "ensure_progressive_words",
            "count_duplicate_start_ms",
        ] {
            assert!(
                !src.contains(banned),
                "aligner.rs must not contain retired symbol `{banned}`"
            );
        }
    }
}
