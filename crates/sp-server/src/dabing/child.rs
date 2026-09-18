//! Rust wrapper around `scripts/dub_worker.py live-translate` (#183 D4).
//!
//! Mirrors `crate::stems::separator::separate_stems`: spawn the Python child at
//! BELOW_NORMAL priority under the process-global heavy slot + a Windows Job
//! Object, bound by the #171 stall-timeout (the child writes `chunk_N.wav` +
//! `chunk_N.json` and heartbeats into `work_dir`, so a slow-but-progressing
//! real-time stream is never killed). The child streams the ORIGINAL audio into
//! the Gemini Live Translate API and writes the Slovak dub on the video timeline.
//!
//! The Gemini key is passed ONLY through the child's environment
//! (`GEMINI_API_KEY`) — never on the command line or in a log.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::lyrics::heavy_plan::HeavyStepPlan;

/// One chunk's outcome reported by the child (for the drift check + log).
#[derive(Debug, Clone, Deserialize)]
pub struct DubChunkResult {
    pub index: usize,
    pub chunk_start_ms: u64,
    pub chunk_end_ms: u64,
    /// Length of the translated output BEFORE any `atempo` was applied.
    pub out_len_ms: u64,
    pub next_start_ms: Option<u64>,
    /// The `atempo` factor the child actually applied (`1.0` = none).
    pub tempo: f32,
}

impl DubChunkResult {
    /// Source-chunk length.
    pub fn chunk_len_ms(&self) -> u64 {
        self.chunk_end_ms.saturating_sub(self.chunk_start_ms)
    }
}

/// The summary JSON the child prints on stdout when the dub completes.
#[derive(Debug, Clone, Deserialize)]
pub struct DubSummary {
    pub out_path: String,
    pub transcripts_path: String,
    pub chunks: Vec<DubChunkResult>,
}

/// Build the `live-translate` argv (script + flags), in order. NO SECRET here —
/// the Gemini key is carried in the child's env by [`run_live_translate`]. Pure,
/// unit-tested.
pub fn live_translate_args(
    script_path: &Path,
    audio_in: &Path,
    out: &Path,
    transcripts: &Path,
    chunk_plan: &Path,
    work_dir: &Path,
    pace: f32,
) -> Vec<OsString> {
    vec![
        script_path.as_os_str().to_owned(),
        "live-translate".into(),
        "--audio".into(),
        audio_in.as_os_str().to_owned(),
        "--out".into(),
        out.as_os_str().to_owned(),
        "--transcripts".into(),
        transcripts.as_os_str().to_owned(),
        "--chunk-plan".into(),
        chunk_plan.as_os_str().to_owned(),
        "--work-dir".into(),
        work_dir.as_os_str().to_owned(),
        "--pace".into(),
        format!("{pace}").into(),
    ]
}

/// Run the Live-Translate child, returning its parsed [`DubSummary`]. Holds the
/// heavy slot for the child's (long, real-time) lifetime. `api_key` is passed via
/// env only. `plan` supplies BELOW_NORMAL creation flags + the stall window.
#[allow(clippy::too_many_arguments)] // spawn helper: paths + key + pace + plan
pub async fn run_live_translate(
    python_path: &Path,
    script_path: &Path,
    audio_in: &Path,
    out: &Path,
    transcripts: &Path,
    chunk_plan: &Path,
    work_dir: &Path,
    api_key: &str,
    pace: f32,
    eta: std::time::Duration,
    plan: &HeavyStepPlan,
) -> Result<DubSummary> {
    // Hold the process-global heavy slot for this child's lifetime — one heavy
    // child at a time process-wide (shared with stems/lyrics).
    let _slot = crate::lyrics::heavy_slot::acquire_slot("dub live-translate").await;

    let mut cmd = Command::new(python_path);
    cmd.args(live_translate_args(
        script_path,
        audio_in,
        out,
        transcripts,
        chunk_plan,
        work_dir,
        pace,
    ));
    // The child shells out to ffmpeg by bare name (resample / atempo / mux), so the
    // bundled ffmpeg next to the script must be on PATH — same as the stem child.
    if let Some(tools_dir) = script_path.parent() {
        cmd.env("PATH", crate::lyrics::bootstrap::prepend_path_with(tools_dir));
    }
    // Secret via ENV only — never argv, never logged.
    cmd.env("GEMINI_API_KEY", api_key);
    // BELOW_NORMAL creation flags + thread caps (no `--force-cpu`: the dub child is
    // network-bound and uses neither torch nor CUDA).
    plan.apply(&mut cmd);
    cmd.kill_on_drop(true);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    debug!(
        "running dub live-translate: {} --audio {} → {}",
        python_path.display(),
        audio_in.display(),
        out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn dub live-translate")?;
    let _job = crate::lyrics::heavy_slot::assign_child_job(&child);
    let stdout_task = child
        .stdout
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    let stderr_task = child
        .stderr
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    // #171 stall-bounded wait: the child heartbeats + writes chunk_N.wav into
    // work_dir, so a healthy real-time stream keeps the mtime fresh; a hung child
    // dies and work_dir is preserved for the next resume.
    let status = crate::lyrics::heavy_plan::wait_with_stall_timeout(
        &mut child,
        work_dir,
        plan,
        "dub-live-translate",
        eta,
    )
    .await;
    let stdout = match stdout_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let stderr = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let stderr = String::from_utf8_lossy(&stderr);
    let stdout = String::from_utf8_lossy(&stdout);
    let status = status?;
    if !status.success() {
        let tail = crate::lyrics::child_output::failure_tail(&stderr, &stdout, 20, 300);
        warn!("dub live-translate failed ({status}); output tail:\n{tail}");
        anyhow::bail!("dub live-translate exited with status {status}; output tail:\n{tail}");
    }
    debug!(
        "dub live-translate ok; stderr tail:\n{}",
        crate::lyrics::child_output::tail_lines(&stderr, 5, 300)
    );

    // The summary JSON is the LAST non-empty stdout line (the child logs to stderr).
    let summary_line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let summary: DubSummary = serde_json::from_str(summary_line.trim()).with_context(|| {
        format!(
            "dub live-translate produced no parseable summary (stdout tail:\n{})",
            crate::lyrics::child_output::tail_lines(&stdout, 5, 300)
        )
    })?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn live_translate_args_carry_every_flag_and_no_secret() {
        let args = live_translate_args(
            Path::new("/t/dub_worker.py"),
            Path::new("/c/a_audio.flac"),
            Path::new("/c/a_dub.flac"),
            Path::new("/c/a_dub_transcripts.json"),
            Path::new("/c/w/chunk_plan.json"),
            Path::new("/c/w"),
            1.0,
        );
        let joined: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(joined[0], "/t/dub_worker.py");
        assert_eq!(joined[1], "live-translate");
        for flag in ["--audio", "--out", "--transcripts", "--chunk-plan", "--work-dir", "--pace"] {
            assert!(joined.iter().any(|a| a == flag), "missing {flag}");
        }
        // No API key ever appears in argv.
        assert!(
            !joined.iter().any(|a| a.contains("GEMINI") || a.len() == 39),
            "argv must never carry the API key"
        );
    }

    #[test]
    fn summary_parses_and_chunk_len_is_derived() {
        let json = r#"{"out_path":"/c/a_dub.flac","transcripts_path":"/c/a_dub_transcripts.json",
            "chunks":[{"index":0,"chunk_start_ms":0,"chunk_end_ms":60000,"out_len_ms":58000,
            "next_start_ms":60000,"tempo":1.0}]}"#;
        let s: DubSummary = serde_json::from_str(json).unwrap();
        assert_eq!(s.out_path, "/c/a_dub.flac");
        assert_eq!(s.chunks.len(), 1);
        assert_eq!(s.chunks[0].chunk_len_ms(), 60000);
        assert_eq!(s.chunks[0].out_len_ms, 58000);
        let _ = PathBuf::from(&s.out_path);
    }
}
