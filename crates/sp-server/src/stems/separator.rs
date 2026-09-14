//! Rust wrapper around `scripts/stem_worker.py separate` (#14).
//!
//! Mirrors `crate::lyrics::aligner::preprocess_vocals`: spawn the Python GPU
//! subprocess at BELOW_NORMAL priority with the operator VRAM cap, bound by a
//! duration-scaled timeout, kill-on-drop so no orphan separator holds GPU
//! weights across a restart. The subprocess writes the two 48 kHz stereo stems.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;
use tracing::debug;

/// Run Kim two-stem separation on `audio_in`, writing `{vocals_out, instrumental_out}`.
///
/// **Cache:** if BOTH outputs already exist and are larger than 1 MB, skip the
/// subprocess and return — a real stem for a song of any length is much larger,
/// so this only skips genuinely-complete pairs (a truncated/aborted file is
/// re-generated). `timeout` bounds the subprocess; callers pass
/// `aligner::isolation_timeout(duration_ms)`.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)] // spawn helper: paths + timeout + cap, same shape as the lyrics workers
pub async fn separate_stems(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    vocals_out: &Path,
    instrumental_out: &Path,
    timeout: std::time::Duration,
    gpu_mem_setting: Option<&str>,
) -> Result<()> {
    // Cache check: reuse an existing complete pair.
    if let (Ok(vm), Ok(im)) = (
        tokio::fs::metadata(vocals_out).await,
        tokio::fs::metadata(instrumental_out).await,
    ) && vm.is_file()
        && vm.len() > 1_000_000
        && im.is_file()
        && im.len() > 1_000_000
    {
        debug!(
            "separate-stems: cache hit, reusing {} + {}",
            vocals_out.display(),
            instrumental_out.display()
        );
        return Ok(());
    }

    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "separate".as_ref(),
        "--audio".as_ref(),
        audio_in.as_os_str(),
        "--vocals-out".as_ref(),
        vocals_out.as_os_str(),
        "--instrumental-out".as_ref(),
        instrumental_out.as_os_str(),
        "--models-dir".as_ref(),
        models_dir.as_os_str(),
    ]);
    // audio-separator shells out to ffmpeg by bare name, so the bundled ffmpeg
    // (next to the script in tools_dir) must be on PATH — same as preprocess_vocals.
    if let Some(tools_dir) = script_path.parent() {
        cmd.env(
            "PATH",
            crate::lyrics::bootstrap::prepend_path_with(tools_dir),
        );
    }
    // #154: carry the operator VRAM cap so separation leaves headroom for the
    // live MF decoder on the shared box.
    for (k, v) in crate::lyrics::gpu_policy::env_for_child(gpu_mem_setting) {
        cmd.env(k, v);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW 0x08000000 | BELOW_NORMAL_PRIORITY_CLASS 0x00004000.
        cmd.creation_flags(0x08000000 | 0x00004000);
    }
    cmd.kill_on_drop(true);

    debug!(
        "running separate-stems: {} --audio {} → {} + {}",
        python_path.display(),
        audio_in.display(),
        vocals_out.display(),
        instrumental_out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn separate-stems")?;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("separate-stems wait failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("separate-stems timed out after {} s", timeout.as_secs());
        }
    };
    if !status.success() {
        anyhow::bail!("separate-stems exited with status {status}");
    }

    // Post-condition: both stems must exist and be non-trivial.
    for p in [vocals_out, instrumental_out] {
        let meta = tokio::fs::metadata(p)
            .await
            .with_context(|| format!("separate-stems produced no {}", p.display()))?;
        if meta.len() < 1_000 {
            anyhow::bail!(
                "separate-stems produced a suspiciously small {}",
                p.display()
            );
        }
    }
    Ok(())
}
