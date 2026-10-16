//! Rust subprocess wrapper for `lyrics_worker.py preprocess-vocals`:
//! `preprocess_vocals(vocals) → clean_wav`, anvuew dereverb + 16 kHz (the
//! vocals come from the stems sidecar, #144 — no BS-RoFormer isolation pass).
//! The forced alignment is the mtl aligner's (`mtl_aligner.rs`, its own venv);
//! the retired Qwen3 chunk-alignment wrapper is deleted (#144, v22 one regime).

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, warn};

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

/// Where a song's isolated vocal lives: `{cache}/{youtube_id}_vocals16k.wav`,
/// the 16 kHz mono float WAV [`preprocess_vocals`] writes and the g35t ★ /
/// base tier uploads. Kept across runs; the startup cache scan knows the name
/// (`downloader::cache`). The live g35t probe sends a clip of it (#144).
pub(crate) fn isolated_vocal_path(cache_dir: &Path, youtube_id: &str) -> PathBuf {
    cache_dir.join(format!("{youtube_id}_vocals16k.wav"))
}

/// Build the `preprocess-vocals` argv (script + flags), in order. `#144`: the
/// input is the stems worker's vocals sidecar (`--vocals-in`), not the mix — the
/// step is anvuew dereverb + 16 kHz resample only, the BS-RoFormer isolation
/// pass is gone. `#162`: a CPU plan appends `--force-cpu` so the script forces
/// in-process CPU inference (`_force_cpu()`), leaving the GPU untouched WITHOUT
/// hiding it via `CUDA_VISIBLE_DEVICES` (which crashed the NVIDIA user-mode
/// driver — see `HeavyStepPlan::apply`). A GPU plan appends nothing.
fn preprocess_vocals_args(
    script_path: &Path,
    vocals_in: &Path,
    wav_out: &Path,
    models_dir: &Path,
    work_dir: &Path,
    plan: &crate::lyrics::heavy_plan::HeavyStepPlan,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        script_path.as_os_str().to_owned(),
        "preprocess-vocals".into(),
        "--vocals-in".into(),
        vocals_in.as_os_str().to_owned(),
        "--output".into(),
        wav_out.as_os_str().to_owned(),
        "--models-dir".into(),
        models_dir.as_os_str().to_owned(),
        // #171: per-segment scratch dir for resumable isolation.
        "--work-dir".into(),
        work_dir.as_os_str().to_owned(),
    ];
    for a in plan.script_cpu_args() {
        args.push((*a).into());
    }
    args
}

/// Run anvuew de-reverb + 16 kHz mono float32 resample on `vocals_in` — the
/// stems worker's vocals sidecar (`{base}_audio_vocals.flac`, #184 G0). Writes
/// the clean WAV to `wav_out` and returns the same path on success. #144: the
/// second BS-RoFormer vocal-isolation pass is deleted — every video is already
/// separated once by the stems worker, so the mtl aligner consumes THAT track.
///
/// `work_dir` is the per-segment scratch dir for the resumable script (#171):
/// each isolated segment WAV lands there and is skipped on resume, so a
/// killed/timed-out run continues instead of discarding the whole song. The
/// child is bounded by a STALL timeout (no new segment for
/// [`heavy_plan::stall_timeout`]), NOT the whole-song `timeout` — which now
/// survives only as the ETA in the log line.
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
    vocals_in: &Path,
    wav_out: &Path,
    work_dir: &Path,
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
    // #144 r2: the heavy slot is acquired by the caller
    // (`heavy_plan::isolate_with_regime` via `acquire_slot_for_spawn`) and held
    // across the whole isolation step (incl. the GPU→CPU re-run). No acquire
    // here — a second acquire on the same task would deadlock the Semaphore(1).
    let mut cmd = Command::new(python_path);
    cmd.args(preprocess_vocals_args(
        script_path,
        vocals_in,
        wav_out,
        models_dir,
        work_dir,
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
    // #171: pipe + drain the child's stdio so a failure's Python traceback
    // reaches the log. Previously the child inherited SongPlayer's stdio, so the
    // traceback was lost and the log carried only "exited with status exit code:
    // 1" — the win-resolume isolation exit-1 could not be diagnosed. Drain
    // concurrently (below): the stall waiter only calls `child.wait()`, so a full
    // pipe would deadlock the child (same reason as `stems::separator`).
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    debug!(
        "running preprocess-vocals: {} --vocals-in {} --output {}",
        python_path.display(),
        vocals_in.display(),
        wav_out.display()
    );

    let mut child = cmd.spawn().context("failed to spawn preprocess-vocals")?;
    // #162: cap the child's memory via a Windows Job Object so an OOM kills the
    // child, not the host. Held (with the slot) until the child exits.
    let _job = crate::lyrics::heavy_slot::assign_child_job(&child);
    // Drain stdout/stderr in the background so the pipes never fill and deadlock
    // the child. Both tasks end when the child closes its pipes (normal exit, or
    // SIGKILL on a stall / `kill_on_drop`).
    let stdout_task = child
        .stdout
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    let stderr_task = child
        .stderr
        .take()
        .map(crate::lyrics::child_output::drain_pipe);
    // #171: bound the child by a STALL timeout (kill only when no new segment has
    // been written to work_dir for `stall_timeout`), NOT the whole-song ceiling —
    // a legitimate 3-10 min song runs 40-77 min on this CPU and the old ceiling
    // killed it mid-run and discarded the work. A stall leaves work_dir intact so
    // the next pick resumes from the segments already isolated.
    let wait_res = crate::lyrics::heavy_plan::wait_with_stall_timeout(
        &mut child,
        work_dir,
        plan,
        "preprocess-vocals",
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
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    match wait_res {
        Ok(status) if status.success() => {
            // The script prints `gpu_polite:`/`isolation chunk N/M done`
            // diagnostics on stderr — keep the tail visible for a healthy run.
            debug!(
                "preprocess-vocals ok; stderr tail:\n{}",
                crate::lyrics::child_output::tail_lines(&stderr, 5, 300)
            );
            Ok(wav_out.to_path_buf())
        }
        Ok(status) => {
            // Non-zero exit (the exit-1 class): surface the Python traceback tail.
            let tail = crate::lyrics::child_output::failure_tail(&stderr, &stdout, 30, 300);
            warn!("preprocess-vocals failed ({status}); output tail:\n{tail}");
            anyhow::bail!("preprocess-vocals exited with status {status}; output tail:\n{tail}");
        }
        Err(e) => {
            // A stall kill (or a wait error): the child was killed, its work dir
            // preserved for resume. Surface whatever it printed before dying.
            let tail = crate::lyrics::child_output::failure_tail(&stderr, &stdout, 30, 300);
            warn!("preprocess-vocals {e}; output tail:\n{tail}");
            Err(e.context(format!("preprocess-vocals output tail:\n{tail}")))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// #144: the one place the isolated vocal's path is built (the startup
    /// scan's `VOCALS_RE` matches the same name); the isolation writes it
    /// and the live g35t probe reads it.
    #[test]
    fn the_isolated_vocal_is_named_after_the_youtube_id() {
        assert_eq!(
            isolated_vocal_path(Path::new("/cache"), "gq-4FVRr_ow"),
            Path::new("/cache").join("gq-4FVRr_ow_vocals16k.wav")
        );
    }

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
            // #144: the retired Qwen3-ForcedAligner wrapper.
            ["align", "_chunks"].concat(),
            ["align", "-chunks"].concat(),
        ];
        for sym in &banned {
            assert!(
                !src.contains(sym.as_str()),
                "aligner.rs must not contain retired symbol `{sym}`"
            );
        }
    }

    /// #144: the chunk planner and the chunk assembler of the retired Qwen
    /// aligner went with its chunk-alignment command, their only consumer.
    #[test]
    fn the_lyrics_module_declares_no_retired_chunk_modules() {
        let src = include_str!("mod.rs");
        for module in ["chunking", "assembly"] {
            let decl = format!("pub mod {module};");
            assert!(
                !src.contains(&decl),
                "lyrics/mod.rs must not declare `{decl}`"
            );
        }
    }

    // ---- #162: --force-cpu argv, NOT CUDA_VISIBLE_DEVICES ----------------

    fn preprocess_argv(plan: &crate::lyrics::heavy_plan::HeavyStepPlan) -> Vec<String> {
        preprocess_vocals_args(
            Path::new("/tools/lyrics_worker.py"),
            // #144: the input is the stems worker's vocals sidecar.
            Path::new("/cache/foo_audio_vocals.flac"),
            Path::new("/x/o.wav"),
            Path::new("/models"),
            Path::new("/x/o_isolation"),
            plan,
        )
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn preprocess_vocals_args_passes_the_stems_vocals_sidecar() {
        // #144: the vocals come from `--vocals-in`, not `--audio` (the old mix
        // input + BS-RoFormer isolation pass are gone).
        let argv = preprocess_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle());
        let i = argv
            .iter()
            .position(|a| a == "--vocals-in")
            .expect("preprocess-vocals must receive --vocals-in");
        assert_eq!(argv[i + 1], "/cache/foo_audio_vocals.flac");
        assert!(
            !argv.iter().any(|a| a == "--audio"),
            "the deleted mix-isolation input --audio must not be passed"
        );
    }

    #[test]
    fn preprocess_vocals_args_carries_work_dir() {
        // #171: --work-dir must be threaded to the resumable script, before the
        // trailing --force-cpu (so the last-arg assertions below still hold).
        let argv = preprocess_argv(&crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle());
        let i = argv
            .iter()
            .position(|a| a == "--work-dir")
            .expect("has --work-dir");
        assert_eq!(argv[i + 1], "/x/o_isolation");
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
