//! Rust wrapper around `scripts/stem_worker.py separate` (#14).
//!
//! Mirrors `crate::lyrics::aligner::preprocess_vocals`: spawn the Python GPU
//! subprocess at BELOW_NORMAL priority with the operator VRAM cap, bound by a
//! duration-scaled timeout, kill-on-drop so no orphan separator holds GPU
//! weights across a restart. The subprocess writes the two 48 kHz stereo stems.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

/// #207 round 3b: how many trailing stderr lines the separation child's
/// success AND failure logs keep. mimalloc's `MIMALLOC_SHOW_STATS=1` (see
/// `lyrics::heavy_alloc_env`) prints a ~40-line reserved/committed/peak block
/// at child exit; the prior 5/20-line tails truncated it before the stats
/// ever reached the log. 60 leaves headroom above the stats block plus the
/// `gpu_polite:`/verbose-option lines already logged today.
pub(crate) const SEPARATION_STDERR_TAIL_LINES: usize = 60;

/// Build the `separate` argv (script + flags), in order. `#162`: a CPU plan
/// appends `--force-cpu` so the script forces in-process CPU inference
/// (`_force_cpu()`), leaving the GPU untouched WITHOUT hiding it via
/// `CUDA_VISIBLE_DEVICES` (which crashed the NVIDIA user-mode driver — see
/// `HeavyStepPlan::apply`). A GPU plan appends nothing.
fn separate_stems_args(
    script_path: &Path,
    audio_in: &Path,
    vocals_out: &Path,
    instrumental_out: &Path,
    models_dir: &Path,
    work_dir: &Path,
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        script_path.as_os_str().to_owned(),
        "separate".into(),
        "--audio".into(),
        audio_in.as_os_str().to_owned(),
        "--vocals-out".into(),
        vocals_out.as_os_str().to_owned(),
        "--instrumental-out".into(),
        instrumental_out.as_os_str().to_owned(),
        "--models-dir".into(),
        models_dir.as_os_str().to_owned(),
        // #171: per-segment scratch dir for resumable separation.
        "--work-dir".into(),
        work_dir.as_os_str().to_owned(),
    ];
    for a in plan.script_cpu_args() {
        args.push((*a).into());
    }
    args
}

/// Run Kim two-stem separation on `audio_in`, writing `{vocals_out, instrumental_out}`.
///
/// **Cache:** if BOTH outputs already exist and are larger than 1 MB, skip the
/// subprocess and return — a real stem for a song of any length is much larger,
/// so this only skips genuinely-complete pairs (a truncated/aborted file is
/// re-generated). `timeout` bounds the subprocess; callers pass
/// `aligner::isolation_timeout(duration_ms)`.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)] // spawn helper: paths + timeout + cap + plan, same shape as the lyrics workers
pub async fn separate_stems(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_in: &Path,
    vocals_out: &Path,
    instrumental_out: &Path,
    work_dir: &Path,
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

    // #144 r2: the heavy slot is acquired by the caller (`stems::worker` via
    // `acquire_slot_for_spawn`) and held across `run_separation_watched` (incl.
    // the GPU→CPU re-run). No acquire here — a second acquire on the same task
    // would deadlock the Semaphore(1).
    let mut cmd = Command::new(python_path);
    cmd.args(separate_stems_args(
        script_path,
        audio_in,
        vocals_out,
        instrumental_out,
        models_dir,
        work_dir,
        plan,
    ));
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
    // #168: retain the injected mimalloc heap for the separation child — never
    // decommit freed pages, reserve+commit one arena up front, so the per-step
    // page-fault storm is paid once, not per inference step. Effective only when
    // the venv interpreter carries the mimalloc override (`bootstrap_venv_exe`);
    // an env no-op otherwise, and numerically invisible to the model.
    // #207: carry the operator `heavy_alloc_mode` + `heavy_purge_delay_ms` (from
    // the live containment published by `refresh_containment` this tick) into the
    // mimalloc env so the box can measure returning the child's ~9 GB commit
    // without a rebuild. Phase-3: `lazy` turns eager commit OFF — the lever.
    let containment = crate::lyrics::heavy_slot::current_containment();
    for (k, v) in crate::lyrics::heavy_alloc_env::heavy_alloc_env(
        containment.alloc_mode,
        containment.purge_delay_ms,
        // #207 round-3c: TEMPORARY literal default until the next lane wires
        // the operator `heavy_alloc_reserve_gib` setting through Containment.
        crate::lyrics::heavy_alloc_env::RESERVE_GIB_DEFAULT,
    ) {
        cmd.env(k, v);
    }
    // #162: stamp the priority-regime plan — caps CPU threads
    // (`OMP|MKL|TORCH_NUM_THREADS`) + Windows priority-class creation flags (IDLE
    // for cpu-idle, BELOW_NORMAL for gpu). The CPU force is carried in argv
    // (`--force-cpu`, added by `separate_stems_args` above) — NOT via
    // `CUDA_VISIBLE_DEVICES`, which crashed the NVIDIA driver (see
    // `HeavyStepPlan::apply`).
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

    let mut child = cmd.spawn().context("failed to spawn separate-stems")?;
    // #162: cap the child's memory (Windows Job Object) so an OOM kills the
    // child, not the host. Held (with the slot) until the child exits below.
    let _job = crate::lyrics::heavy_slot::assign_child_job(&child);
    // Drain stdout/stderr into buffers in the background so the pipes never fill
    // and deadlock the child (#171: the stall waiter below only calls
    // `child.wait()`, not `wait_with_output`, so we must drain the pipes
    // ourselves). Both tasks end when the child closes its pipes (normal exit,
    // or SIGKILL on a stall / `kill_on_drop`).
    let stdout_task = child
        .stdout
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    let stderr_task = child
        .stderr
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    // #171: bound the child by a STALL timeout — kill only when no new segment has
    // been written to work_dir for `stall_timeout` — NOT the whole-song ceiling
    // (a 10.5-min stem runs ~85 min on this CPU and the old ceiling killed it
    // mid-run and discarded the work). A stall leaves work_dir intact so the next
    // pick resumes from the segments already separated.
    let status = crate::lyrics::heavy_plan::wait_with_stall_timeout(
        &mut child,
        work_dir,
        plan,
        "separate-stems",
        timeout,
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
        let tail = crate::lyrics::child_output::failure_tail(
            &stderr,
            &stdout,
            SEPARATION_STDERR_TAIL_LINES,
            300,
        );
        warn!("separate-stems failed ({}); output tail:\n{}", status, tail);
        anyhow::bail!(
            "separate-stems exited with status {}; output tail:\n{}",
            status,
            tail
        );
    }
    // The script prints `gpu_polite:` diagnostics on stderr — keep the tail visible.
    debug!(
        "separate-stems ok; stderr tail:\n{}",
        crate::lyrics::child_output::tail_lines(&stderr, SEPARATION_STDERR_TAIL_LINES, 300)
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

#[cfg(test)]
mod tests {
    // ---- #162: --force-cpu argv, NOT CUDA_VISIBLE_DEVICES ----------------

    fn separate_argv(plan: &crate::lyrics::heavy_plan::HeavyStepPlan) -> Vec<String> {
        super::separate_stems_args(
            std::path::Path::new("/tools/stem_worker.py"),
            std::path::Path::new("/x/a.flac"),
            std::path::Path::new("/x/v.flac"),
            std::path::Path::new("/x/i.flac"),
            std::path::Path::new("/models"),
            std::path::Path::new("/x/a_stemsep"),
            plan,
        )
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn separate_stems_args_appends_force_cpu_for_cpu_plan() {
        let argv = separate_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle());
        assert!(argv.contains(&"separate".to_string()));
        assert_eq!(
            argv.last().unwrap(),
            "--force-cpu",
            "a CPU plan must force in-process CPU inference via --force-cpu"
        );
    }

    #[test]
    fn separate_stems_args_omits_force_cpu_for_gpu_plan() {
        let argv = separate_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::gpu_below_normal());
        assert!(argv.contains(&"separate".to_string()));
        assert!(
            !argv.contains(&"--force-cpu".to_string()),
            "a GPU plan must NOT pass --force-cpu"
        );
    }
}
