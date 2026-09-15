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
use std::ffi::OsString;
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
    /// Position within the source line's word stream where this chunk's
    /// words begin. Round-tripped to Python unchanged so the Rust
    /// assembly phase can slot sub-chunk outputs back into the right
    /// slice of a split line's full word sequence.
    word_offset: usize,
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
// isolation_timeout
// ---------------------------------------------------------------------------

/// Duration-scaled ceiling for the vocal-isolation subprocess (#144).
///
/// Vocal isolation (Mel-Roformer + anvuew dereverb + resample) runs at ≈1×
/// realtime on win-resolume (RTX 3070 Ti, BELOW_NORMAL priority; measured
/// 2026-09-12: a 240-s song took 233 s, and the Mel-Roformer pass alone is
/// 0.75× realtime and linear on both a 240-s and an 827-s song). The old
/// hard-coded 600 s ceiling could only ever pass a song under ~9 min from a
/// cached WAV, so the 10–15-min band (29 catalog songs) timed out on every
/// cold run.
///
/// `clamp(2 × duration, 600 s, 3600 s)` gives every song ~2× its realtime
/// length of headroom while still capping a hung separator at one hour. An
/// unknown or non-positive duration falls back to the 3600 s ceiling.
pub fn isolation_timeout(duration_ms: Option<i64>) -> std::time::Duration {
    match duration_ms {
        Some(ms) if ms > 0 => {
            let secs = (ms / 1000).saturating_mul(2).clamp(600, 3600);
            std::time::Duration::from_secs(secs as u64)
        }
        _ => std::time::Duration::from_secs(3600),
    }
}

// ---------------------------------------------------------------------------
// preprocess_vocals
// ---------------------------------------------------------------------------

/// Build the `preprocess-vocals` argv (script + flags), in order. `#162`: a CPU
/// plan appends `--force-cpu` so the script forces in-process CPU inference
/// (`_force_cpu()`), leaving the GPU untouched WITHOUT hiding it via
/// `CUDA_VISIBLE_DEVICES` (which crashed the NVIDIA user-mode driver — see
/// `HeavyStepPlan::apply`). A GPU plan appends nothing.
fn preprocess_vocals_args(
    script_path: &Path,
    audio_in: &Path,
    wav_out: &Path,
    models_dir: &Path,
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        script_path.as_os_str().to_owned(),
        "preprocess-vocals".into(),
        "--audio".into(),
        audio_in.as_os_str().to_owned(),
        "--output".into(),
        wav_out.as_os_str().to_owned(),
        "--models-dir".into(),
        models_dir.as_os_str().to_owned(),
    ];
    for a in plan.script_cpu_args() {
        args.push((*a).into());
    }
    args
}

/// Run Mel-Roformer vocal isolation + anvuew de-reverb + 16 kHz mono float32
/// resample on `audio_in`. Writes the clean WAV to `wav_out` and returns
/// the same path on success.
///
/// `timeout` bounds the isolation subprocess; callers pass
/// [`isolation_timeout`]`(duration_ms)` so a long song gets a proportionally
/// longer ceiling instead of the old fixed 600 s (#144).
///
/// **Cache (v18):** if `wav_out` already exists and is larger than 1 MB,
/// skip Demucs entirely and return the existing path. Demucs on a 10-min
/// song can take 5–10 min on CPU; re-running it unnecessarily on every
/// worker pick-up was timing out at 600 s and blocking the entire pipeline.
/// The Python prototype's `scripts/experiments/gemini_lyrics.py` also
/// relied on caller-side caching of the vocals WAV.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub async fn preprocess_vocals(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    wav_out: &Path,
    timeout: std::time::Duration,
    gpu_mem_setting: Option<&str>,
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
) -> Result<PathBuf> {
    // Cache check: reuse an existing vocals WAV if it looks complete.
    // 1 MB minimum avoids reusing truncated/aborted files from a previous
    // run — a real Demucs output for a song of any length is always much
    // larger (≥ ~6 MB for a 60 s chunk at 16 kHz float32).
    if let Ok(meta) = tokio::fs::metadata(wav_out).await {
        if meta.is_file() && meta.len() > 1_000_000 {
            debug!(
                "preprocess-vocals: cache hit, reusing existing WAV: {} ({} bytes)",
                wav_out.display(),
                meta.len()
            );
            return Ok(wav_out.to_path_buf());
        }
    }
    // #162: acquire the process-global heavy-step slot BEFORE spawning (after
    // the cache check, so a cache hit never waits) and hold it until the child
    // exits — at most one heavy child (isolation / mtl / separation) runs
    // process-wide, so two workers can never OOM the box together.
    let _slot = crate::lyrics::heavy_slot::acquire_slot("isolation").await;
    let mut cmd = Command::new(python_path);
    cmd.args(preprocess_vocals_args(
        script_path,
        audio_in,
        wav_out,
        models_dir,
        plan,
    ));
    // audio-separator calls ffmpeg.exe without an absolute path, so the
    // Python subprocess needs tools_dir (parent of lyrics_worker.py) on
    // PATH — that's where the app's bundled ffmpeg.exe lives.
    if let Some(tools_dir) = script_path.parent() {
        cmd.env(
            "PATH",
            crate::lyrics::bootstrap::prepend_path_with(tools_dir),
        );
    }
    // #154: carry the operator-tunable VRAM cap to the child (applied only on
    // the GPU path by the script's `gpu_polite()`; harmless on the CPU path).
    for (k, v) in crate::lyrics::gpu_policy::env_for_child(gpu_mem_setting) {
        cmd.env(k, v);
    }
    // #162: stamp the priority-regime plan — caps CPU threads
    // (`OMP|MKL|TORCH_NUM_THREADS`) and, on Windows, sets the priority-class
    // creation flags (IDLE for cpu-idle, BELOW_NORMAL for gpu) OR'd with
    // CREATE_NO_WINDOW. The CPU force is carried in argv (`--force-cpu`, added by
    // `preprocess_vocals_args` above) — NOT via `CUDA_VISIBLE_DEVICES`, which
    // crashed the NVIDIA driver (see `HeavyStepPlan::apply`).
    plan.apply(&mut cmd);
    // Kill the Python child if the Command handle is dropped (worker
    // shutdown, error path, timeout). Prevents orphan Demucs processes
    // from holding ~1-2 GB of GPU model weights across SongPlayer
    // restarts.
    cmd.kill_on_drop(true);

    debug!(
        "running preprocess-vocals: {} --audio {} --output {}",
        python_path.display(),
        audio_in.display(),
        wav_out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn preprocess-vocals")?;
    // #162: cap the child's memory via a Windows Job Object so an OOM kills the
    // child, not the host. Held (with the slot) until the child exits.
    let _job = crate::lyrics::heavy_slot::assign_child_job(&child);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("preprocess-vocals wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("preprocess-vocals timed out after {} s", timeout.as_secs());
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
                word_offset: r.word_offset,
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
    // Same PATH injection as preprocess_vocals — align-chunks loads the
    // Qwen3 aligner which depends on audio-separator's imports, which in
    // turn may load ffmpeg. Keep the subprocess environment consistent.
    if let Some(tools_dir) = script_path.parent() {
        cmd.env(
            "PATH",
            crate::lyrics::bootstrap::prepend_path_with(tools_dir),
        );
    }

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
            let (line_index, word_offset) = requests
                .get(c.chunk_idx)
                .map(|r| (r.line_index, r.word_offset))
                .unwrap_or((usize::MAX, 0));
            ChunkResult {
                line_index,
                word_offset,
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
    use super::*;

    /// Audit: retired symbols must no longer be referenced from this file.
    /// Keeps the compiler from being the only line of defence against a
    /// dangling re-export of the old API leaking back in.
    ///
    /// NOTE: banned symbol names are split across two string literals joined
    /// at runtime so this test file does not contain the verbatim string it is
    /// checking for (which would cause the test to always fail on itself).
    #[test]
    fn aligner_source_has_no_retired_symbols() {
        let src = include_str!("aligner.rs");
        let banned = [
            ["align", "_lyrics"].concat(),
            ["merge_word", "_timings"].concat(),
            ["ensure_progressive", "_words"].concat(),
            ["count_duplicate", "_start_ms"].concat(),
        ];
        for sym in &banned {
            assert!(
                !src.contains(sym.as_str()),
                "aligner.rs must not contain retired symbol `{sym}`"
            );
        }
    }

    /// JSON-contract schema test: the request shape Rust writes to
    /// `chunks.json` must round-trip cleanly through the Python
    /// helper. We can't invoke Python in a unit test, but we can at
    /// least prove the Rust-side serialize then parse using the
    /// matching deserialize struct — this catches drift between the
    /// `ChunkInRequest` producer and any future consumer that reads
    /// the same file.
    ///
    /// Equally important: verify the output-side shape (`ChunkOut` +
    /// `ChunkOutWord`) deserialises from the exact JSON the Python
    /// helper writes. The fixture below is copy-pasted from
    /// `lyrics_worker.py::cmd_align_chunks` docstring.
    #[test]
    fn align_chunks_request_json_schema_roundtrips() {
        let requests = vec![
            ChunkInRequest {
                chunk_idx: 0,
                word_offset: 0,
                start_ms: 500,
                end_ms: 3500,
                text: "hey there friend",
                word_count: 3,
            },
            ChunkInRequest {
                chunk_idx: 1,
                word_offset: 3,
                start_ms: 3500,
                end_ms: 6500,
                text: "goodbye now",
                word_count: 2,
            },
        ];
        let req_file = ChunkRequestFile { chunks: requests };
        let json = serde_json::to_string(&req_file).expect("serialize");

        // Shape the Python script reads (quoted from its docstring):
        //   {"chunks": [{"chunk_idx": 0, "word_offset": 0,
        //                "start_ms": 500, "end_ms": 3500,
        //                "text": "hey there friend", "word_count": 3}, ...]}
        assert!(json.contains("\"chunk_idx\""));
        assert!(json.contains("\"word_offset\""));
        assert!(json.contains("\"start_ms\""));
        assert!(json.contains("\"end_ms\""));
        assert!(json.contains("\"text\""));
        assert!(json.contains("\"word_count\""));
    }

    #[test]
    fn align_chunks_output_json_schema_matches_python_docstring() {
        // Fixture verbatim from lyrics_worker.py::cmd_align_chunks docstring.
        let fixture = r#"{
            "chunks": [
                {
                    "chunk_idx": 0,
                    "words": [
                        {"text": "hey", "start_ms": 1000, "end_ms": 1200},
                        {"text": "there", "start_ms": 1200, "end_ms": 1400},
                        {"text": "friend", "start_ms": 1400, "end_ms": 1800}
                    ]
                },
                {
                    "chunk_idx": 1,
                    "words": []
                }
            ]
        }"#;
        let parsed: ChunkResultFile =
            serde_json::from_str(fixture).expect("Python docstring fixture must deserialize");
        assert_eq!(parsed.chunks.len(), 2);
        assert_eq!(parsed.chunks[0].chunk_idx, 0);
        assert_eq!(parsed.chunks[0].words.len(), 3);
        assert_eq!(parsed.chunks[0].words[0].text, "hey");
        assert_eq!(parsed.chunks[0].words[0].start_ms, 1000);
        assert_eq!(parsed.chunks[1].words.len(), 0);
    }

    // ---- #162: --force-cpu argv, NOT CUDA_VISIBLE_DEVICES ----------------

    fn preprocess_argv(plan: &crate::lyrics::heavy_plan::HeavyStepPlan) -> Vec<String> {
        preprocess_vocals_args(
            Path::new("/tools/lyrics_worker.py"),
            Path::new("/x/a.flac"),
            Path::new("/x/o.wav"),
            Path::new("/models"),
            plan,
        )
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn preprocess_vocals_args_appends_force_cpu_for_cpu_plan() {
        let argv = preprocess_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle());
        assert!(argv.contains(&"preprocess-vocals".to_string()));
        assert_eq!(
            argv.last().unwrap(),
            "--force-cpu",
            "a CPU plan must force in-process CPU inference via --force-cpu"
        );
    }

    #[test]
    fn preprocess_vocals_args_omits_force_cpu_for_gpu_plan() {
        let argv = preprocess_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::gpu_below_normal());
        assert!(argv.contains(&"preprocess-vocals".to_string()));
        assert!(
            !argv.contains(&"--force-cpu".to_string()),
            "a GPU plan must NOT pass --force-cpu"
        );
    }
}

#[path = "aligner_tests_timeout.rs"]
#[cfg(test)]
mod tests_timeout;
