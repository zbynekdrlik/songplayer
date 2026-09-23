//! Rust wrapper around `scripts/dub_worker.py live-translate` (#183 D4, #184
//! round H step 2).
//!
//! Mirrors `crate::stems::separator::separate_stems`: spawn the Python child at
//! BELOW_NORMAL priority under the process-global heavy slot + a Windows Job
//! Object, bound by the #171 stall-timeout (the child heartbeats and writes its
//! `events.jsonl` into `work_dir`, so a slow-but-progressing real-time stream is
//! never killed). The child streams the video's audio (the vocals stem when it
//! exists, else the original) into ONE continuous Gemini Live Translate session
//! and writes the Slovak dub on the video timeline.
//!
//! The Gemini key is passed ONLY through the child's environment
//! (`GEMINI_API_KEY`) — never on the command line or in a log.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::lyrics::heavy_plan::HeavyStepPlan;

/// The continuous-session stats the child reports (its `session` object) — the
/// numbers the box acceptance reads. Every field is optional so a missing one
/// never fails a finished dub.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DubSessionStats {
    #[serde(default)]
    pub connections: Option<u32>,
    #[serde(default)]
    pub reconnects: Option<u32>,
    #[serde(default)]
    pub output_to_input_ratio: Option<f64>,
    #[serde(default)]
    pub max_voiced_gap_s: Option<f64>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub drain_end_reason: Option<String>,
}

/// The summary JSON the child prints on stdout when the dub completes.
#[derive(Debug, Clone, Deserialize)]
pub struct DubSummary {
    pub out_path: String,
    pub transcripts_path: String,
    #[serde(default)]
    pub session: DubSessionStats,
}

/// Build the `live-translate` argv (script + flags), in order. NO SECRET here —
/// the Gemini key is carried in the child's env by [`run_live_translate`]. Pure,
/// unit-tested.
pub fn live_translate_args(
    script_path: &Path,
    audio_in: &Path,
    out: &Path,
    transcripts: &Path,
    work_dir: &Path,
    model: &str,
    voice: &str,
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
        "--work-dir".into(),
        work_dir.as_os_str().to_owned(),
        // #184 round H step 2: the model is a setting (an upgrade = a setting
        // change); `speaker` = the speaker's own voice, else a pinned prebuilt.
        "--model".into(),
        voice.into(),
        "--voice".into(),
        model.into(),
    ]
}

/// Run the Live-Translate child, returning its parsed [`DubSummary`]. Holds the
/// heavy slot for the child's (long, real-time) lifetime. `api_key` is passed via
/// env only. `plan` supplies BELOW_NORMAL creation flags + the stall window.
#[allow(clippy::too_many_arguments)] // spawn helper: paths + key + model + voice + plan
pub async fn run_live_translate(
    python_path: &Path,
    script_path: &Path,
    audio_in: &Path,
    out: &Path,
    transcripts: &Path,
    work_dir: &Path,
    api_key: &str,
    model: &str,
    voice: &str,
    eta: std::time::Duration,
    plan: &HeavyStepPlan,
) -> Result<DubSummary> {
    // #144 r2: the heavy slot is acquired by the caller (`dabing::worker::
    // process_next` via `acquire_slot_for_spawn`) and held across the whole dub
    // step. No acquire here — a second acquire on the same task would deadlock
    // the Semaphore(1).
    let mut cmd = Command::new(python_path);
    cmd.args(live_translate_args(
        script_path,
        audio_in,
        out,
        transcripts,
        work_dir,
        model,
        voice,
    ));
    // The child shells out to ffmpeg by bare name (decode / loudnorm / mux), so the
    // bundled ffmpeg next to the script must be on PATH — same as the stem child.
    if let Some(tools_dir) = script_path.parent() {
        cmd.env(
            "PATH",
            crate::lyrics::bootstrap::prepend_path_with(tools_dir),
        );
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
    // #171 stall-bounded wait: the child heartbeats + appends events.jsonl in
    // work_dir, so a healthy real-time stream keeps the mtime fresh; a hung child
    // dies (a re-run starts the session from the beginning).
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
    // The tail carries the session summary line (connections, reconnects,
    // output/input, max voiced gap, latency) and the loudness line — the box
    // acceptance reads them from the server log.
    info!(
        "dub live-translate ok; stderr tail:\n{}",
        crate::lyrics::child_output::tail_lines(&stderr, 8, 400)
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

    #[test]
    fn live_translate_args_carry_model_voice_and_no_chunk_plan_or_secret() {
        let args = live_translate_args(
            Path::new("/t/dub_worker.py"),
            Path::new("/c/a_audio_vocals.flac"),
            Path::new("/c/a_dub.flac"),
            Path::new("/c/a_dub_transcripts.json"),
            Path::new("/c/w"),
            "gemini-3.5-live-translate-preview",
            "speaker",
        );
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            joined,
            vec![
                "/t/dub_worker.py",
                "live-translate",
                "--audio",
                "/c/a_audio_vocals.flac",
                "--out",
                "/c/a_dub.flac",
                "--transcripts",
                "/c/a_dub_transcripts.json",
                "--work-dir",
                "/c/w",
                "--model",
                "gemini-3.5-live-translate-preview",
                "--voice",
                "speaker",
            ]
        );
        // The superseded chunk plan / pacing flags are gone (#184 round H).
        for gone in ["--chunk-plan", "--pace"] {
            assert!(!joined.iter().any(|a| a == gone), "{gone} still passed");
        }
        // No API key ever appears in argv.
        assert!(
            !joined.iter().any(|a| a.contains("GEMINI") || a.len() == 39),
            "argv must never carry the API key"
        );
    }

    #[test]
    fn summary_parses_the_session_stats() {
        let json = r#"{"out_path":"/c/a_dub.flac","transcripts_path":"/c/a_dub_transcripts.json",
            "session":{"connections":4,"reconnects":3,"output_to_input_ratio":1.0012,
            "max_voiced_gap_s":4.2,"latency_ms":3100,"drain_end_reason":"quiet","errors":[]}}"#;
        let s: DubSummary = serde_json::from_str(json).unwrap();
        assert_eq!(s.out_path, "/c/a_dub.flac");
        assert_eq!(s.transcripts_path, "/c/a_dub_transcripts.json");
        assert_eq!(s.session.connections, Some(4));
        assert_eq!(s.session.reconnects, Some(3));
        assert_eq!(s.session.output_to_input_ratio, Some(1.0012));
        assert_eq!(s.session.max_voiced_gap_s, Some(4.2));
        assert_eq!(s.session.latency_ms, Some(3100));
        assert_eq!(s.session.drain_end_reason.as_deref(), Some("quiet"));
    }

    #[test]
    fn summary_without_session_stats_still_parses() {
        let s: DubSummary =
            serde_json::from_str(r#"{"out_path":"/c/a_dub.flac","transcripts_path":"/c/t.json"}"#)
                .unwrap();
        assert_eq!(s.out_path, "/c/a_dub.flac");
        assert_eq!(s.session.connections, None);
        assert_eq!(s.session.latency_ms, None);
    }
}
