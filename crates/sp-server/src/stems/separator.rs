//! Rust wrapper around `scripts/stem_worker.py separate` (#14).
//!
//! Mirrors `crate::lyrics::aligner::preprocess_vocals`: spawn the Python GPU
//! subprocess at BELOW_NORMAL priority with the operator VRAM cap, bound by a
//! duration-scaled timeout, kill-on-drop so no orphan separator holds GPU
//! weights across a restart. The subprocess writes the two 48 kHz stereo stems.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

/// Run Kim two-stem separation on `audio_in`, writing `{vocals_out, instrumental_out}`.
///
/// **Cache:** if BOTH outputs already exist and are larger than 1 MB, skip the
/// subprocess and return — a real stem for a song of any length is much larger,
/// so this only skips genuinely-complete pairs (a truncated/aborted file is
/// re-generated). `timeout` bounds the subprocess; callers pass
/// `aligner::isolation_timeout(duration_ms)`.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)] // spawn helper: paths + timeout + cap, same shape as the lyrics workers
#[allow(clippy::too_many_arguments)]
pub async fn separate_stems(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    vocals_out: &Path,
    instrumental_out: &Path,
    timeout: std::time::Duration,
    gpu_mem_setting: Option<&str>,
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
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
    // #154: carry the operator VRAM cap (applied only on the GPU path by the
    // script's `gpu_polite()`; harmless on the CPU path).
    for (k, v) in crate::lyrics::gpu_policy::env_for_child(gpu_mem_setting) {
        cmd.env(k, v);
    }
    // #162: stamp the priority-regime plan — CPU path hides the GPU
    // (`CUDA_VISIBLE_DEVICES=""` → the script's torch builds every model on CPU,
    // byte-identical to the OOM→CPU fallback) + caps CPU threads; Windows
    // priority-class creation flags (IDLE for cpu-idle, BELOW_NORMAL for gpu).
    // Replaces the old inline BELOW_NORMAL.
    plan.apply(&mut cmd);
    cmd.kill_on_drop(true);
    // Capture the child's traceback: inherited stdio drops the Python stderr, so
    // a live failure could not be diagnosed from the log (#14 follow-up).
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    debug!(
        "running separate-stems: {} --audio {} → {} + {}",
        python_path.display(),
        audio_in.display(),
        vocals_out.display(),
        instrumental_out.display()
    );

    let child = cmd.spawn().context("failed to spawn separate-stems")?;
    // `wait_with_output` takes the child BY VALUE and drains both pipes while it
    // waits, so no timeout branch can `child.kill()` any more. That is fine:
    // `kill_on_drop(true)` is set above, so when the timeout fires and we drop the
    // future (hence the child), the runtime SIGKILLs the separator — the same
    // no-orphan guarantee the old explicit `child.kill()` gave.
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => anyhow::bail!("separate-stems wait failed: {e}"),
        Err(_) => anyhow::bail!("separate-stems timed out after {} s", timeout.as_secs()),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let tail = if stderr.trim().is_empty() {
            tail_lines(&stdout, 20, 300)
        } else {
            tail_lines(&stderr, 20, 300)
        };
        warn!(
            "separate-stems failed ({}); output tail:\n{}",
            output.status, tail
        );
        anyhow::bail!(
            "separate-stems exited with status {}; output tail:\n{}",
            output.status,
            tail
        );
    }
    // The script prints `gpu_polite:` diagnostics on stderr — keep the tail visible.
    debug!(
        "separate-stems ok; stderr tail:\n{}",
        tail_lines(&stderr, 5, 300)
    );

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

/// Last `n` non-empty-trimmed lines of `s`, each truncated to `max_len` chars
/// (append `…` when truncated), joined with `\n`. Pure — unit-tested.
fn tail_lines(s: &str, n: usize, max_len: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|line| {
            if line.chars().count() > max_len {
                let truncated: String = line.chars().take(max_len).collect();
                format!("{truncated}…")
            } else {
                (*line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::tail_lines;

    #[test]
    fn keeps_only_the_last_n_lines() {
        let input = (1..=25)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(&input, 20, 300);
        let out_lines: Vec<&str> = out.lines().collect();
        assert_eq!(out_lines.len(), 20);
        assert_eq!(out_lines.first(), Some(&"6"));
        assert_eq!(out_lines.last(), Some(&"25"));
    }

    #[test]
    fn truncates_a_long_line() {
        let long = "x".repeat(500);
        let out = tail_lines(&long, 20, 300);
        assert_eq!(out.chars().count(), 301); // 300 + the ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn empty_in_empty_out() {
        assert_eq!(tail_lines("", 20, 300), "");
    }
}
