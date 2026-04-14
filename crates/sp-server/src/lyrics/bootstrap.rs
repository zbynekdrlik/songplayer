//! Bootstrap the Python environment used by Qwen3-ForcedAligner.
//!
//! On Windows, ensures `{tools_dir}/lyrics_venv/` exists with `qwen-asr`
//! installed and the Qwen3-ForcedAligner-0.6B model cached locally. On
//! non-Windows, returns `Ok(None)` — alignment is a Windows-only feature.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// The `-c` script passed to the venv Python by `is_ready` to verify the
/// three Python packages the aligner pipeline depends on are importable
/// AND CUDA is available. Exit code 0 iff all four conditions hold:
///   1. `qwen_asr` importable (the Qwen3-ForcedAligner package)
///   2. `torch` importable
///   3. `audio_separator` importable (the Mel-Roformer vocal isolator)
///   4. `torch.cuda.is_available()` returns True
const IS_READY_PROBE: &str =
    "import qwen_asr, torch, audio_separator, sys; sys.exit(0 if torch.cuda.is_available() else 1)";

/// `audio-separator[gpu]` pip package spec — Mel-Roformer vocal isolation
/// plus ONNX Runtime GPU support. Quoted exactly because pip's shell
/// parsing is deferred — we pass this as a single argv element.
#[allow(dead_code)] // only referenced inside #[cfg(target_os = "windows")] bootstrap
const AUDIO_SEPARATOR_PACKAGE: &str = "audio-separator[gpu]";

/// Seconds to wait for the Mel-Roformer / audio-separator pip install
/// to complete. Generous because audio-separator[gpu] has ~1 GB of
/// onnxruntime-gpu and torch-sibling dependencies that download on
/// first run.
#[allow(dead_code)] // only referenced inside #[cfg(target_os = "windows")] bootstrap
const AUDIO_SEPARATOR_PIP_TIMEOUT_SECS: u64 = 900;

/// Returns the absolute path to the venv Python interpreter, or `None`
/// if the bootstrap is skipped (non-Windows).
#[cfg_attr(test, mutants::skip)]
#[cfg(target_os = "windows")]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir
        .join("lyrics_venv")
        .join("Scripts")
        .join("python.exe")
}

#[cfg_attr(test, mutants::skip)]
#[cfg(not(target_os = "windows"))]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("lyrics_venv").join("bin").join("python")
}

/// Returns `true` if `python_path` exists AND `python_path -c "..."` confirms
/// both `qwen_asr` is importable AND `torch.cuda.is_available()`. Used to
/// decide whether bootstrap is needed. A venv with CPU-only torch fails this
/// check so the bootstrap re-runs and installs the CUDA variant.
#[cfg_attr(test, mutants::skip)]
pub async fn is_ready(python_path: &Path) -> bool {
    use tokio::process::Command;

    if !python_path.exists() {
        return false;
    }

    let mut cmd = Command::new(python_path);
    cmd.args(["-c", IS_READY_PROBE]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let res = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.status()).await;

    matches!(res, Ok(Ok(s)) if s.success())
}

/// Ensure the lyrics venv exists, `qwen-asr` is installed, and the
/// Qwen3-ForcedAligner model is preloaded.
///
/// On Windows:
///   1. Create `{tools_dir}/lyrics_venv/` via `python -m venv` (if missing).
///   2. Run `{venv}/Scripts/python.exe -m pip install -U qwen-asr`.
///   3. Run `{venv}/Scripts/python.exe {script_path} preload --models-dir ...`.
///
/// Fast-paths return `Ok(venv_python)` if `is_ready` already passes.
/// On non-Windows: returns `Ok(None)` unconditionally.
#[cfg_attr(test, mutants::skip)]
pub async fn ensure_ready(
    tools_dir: &Path,
    script_path: &Path,
    models_dir: &Path,
    system_python: &Path,
) -> Result<Option<PathBuf>> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (tools_dir, script_path, models_dir, system_python);
        Ok(None)
    }

    #[cfg(target_os = "windows")]
    {
        use anyhow::Context;
        use tokio::process::Command;

        let venv_python = venv_python_path(tools_dir);
        let venv_dir = tools_dir.join("lyrics_venv");

        if is_ready(&venv_python).await {
            tracing::info!(
                "lyrics bootstrap: venv already ready at {}",
                venv_python.display()
            );
            return Ok(Some(venv_python));
        }

        // 1. Create venv if the interpreter is missing (handles corrupted venv too).
        if !venv_python.exists() {
            if venv_dir.exists() {
                tracing::warn!(
                    "lyrics bootstrap: venv at {} is incomplete (no interpreter), repopulating",
                    venv_dir.display()
                );
            } else {
                tracing::info!("lyrics bootstrap: creating venv at {}", venv_dir.display());
            }
            let mut cmd = Command::new(system_python);
            cmd.args(["-m", "venv"]).arg(&venv_dir);
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
            let status = cmd
                .status()
                .await
                .context("failed to spawn python -m venv")?;
            if !status.success() {
                anyhow::bail!("python -m venv exited with status {status}");
            }
        }

        // 2. Install qwen-asr into the venv. pip's exit code is NOT
        // authoritative: in non-TTY mode it sometimes returns 1 for benign
        // warnings (like leftover `~distribution` stubs from a prior partial
        // install). We log but do not bail on non-zero; the final is_ready
        // check at the end of bootstrap is the real success gate.
        tracing::info!("lyrics bootstrap: installing qwen-asr (this may take several minutes)");
        let mut pip = Command::new(&venv_python);
        pip.args(["-m", "pip", "install", "-U", "qwen-asr"]);
        use std::os::windows::process::CommandExt;
        pip.creation_flags(0x08000000);
        let mut pip_child = pip.spawn().context("failed to spawn pip install")?;
        let pip_status =
            match tokio::time::timeout(std::time::Duration::from_secs(600), pip_child.wait()).await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => anyhow::bail!("pip install qwen-asr spawn failed: {e}"),
                Err(_) => {
                    let _ = pip_child.kill().await;
                    anyhow::bail!("pip install qwen-asr timed out after 10 minutes");
                }
            };
        if !pip_status.success() {
            tracing::warn!(
                "lyrics bootstrap: pip install qwen-asr exited {pip_status} (tolerated, final is_ready check decides)"
            );
        }

        // 2a. Force-reinstall torch with CUDA support. qwen-asr pulls the
        // CPU-only torch wheel from PyPI by default; Qwen3-ForcedAligner
        // inference on a 4-minute audio without CUDA takes minutes instead
        // of seconds. Install the cu124 variant from the PyTorch index.
        //
        // Ordering: this MUST run BEFORE step 2b (audio-separator) so the
        // latter sees CUDA torch already in place and does not downgrade it
        // via its own transitive deps.
        tracing::info!("lyrics bootstrap: installing CUDA torch variant");
        let mut torch_pip = Command::new(&venv_python);
        torch_pip.args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "--force-reinstall",
            "torch",
            "--index-url",
            "https://download.pytorch.org/whl/cu124",
        ]);
        torch_pip.creation_flags(0x08000000);
        let mut torch_child = torch_pip
            .spawn()
            .context("failed to spawn torch pip install")?;
        let torch_status =
            match tokio::time::timeout(std::time::Duration::from_secs(900), torch_child.wait())
                .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => anyhow::bail!("torch CUDA install spawn failed: {e}"),
                Err(_) => {
                    let _ = torch_child.kill().await;
                    anyhow::bail!("torch CUDA install timed out after 15 minutes");
                }
            };
        if !torch_status.success() {
            tracing::warn!(
                "lyrics bootstrap: torch CUDA install exited {torch_status} (tolerated, final is_ready check decides)"
            );
        }

        // 2b. Install audio-separator[gpu] for Mel-Roformer vocal isolation.
        // This preprocessing step runs before Qwen3-ForcedAligner; without
        // vocal isolation the aligner produces degenerate timestamps on
        // sung music (instruments mask phoneme boundaries).
        // Same exit-code tolerance as qwen-asr: final is_ready check decides.
        tracing::info!(
            "lyrics bootstrap: installing {AUDIO_SEPARATOR_PACKAGE} (Mel-Roformer vocal isolation)"
        );
        let mut sep_pip = Command::new(&venv_python);
        sep_pip.args(["-m", "pip", "install", "-U", AUDIO_SEPARATOR_PACKAGE]);
        sep_pip.creation_flags(0x08000000);
        let mut sep_child = sep_pip
            .spawn()
            .context("failed to spawn pip install audio-separator")?;
        let sep_status = match tokio::time::timeout(
            std::time::Duration::from_secs(AUDIO_SEPARATOR_PIP_TIMEOUT_SECS),
            sep_child.wait(),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => anyhow::bail!("pip install audio-separator spawn failed: {e}"),
            Err(_) => {
                let _ = sep_child.kill().await;
                anyhow::bail!(
                    "pip install audio-separator timed out after {AUDIO_SEPARATOR_PIP_TIMEOUT_SECS} s"
                );
            }
        };
        if !sep_status.success() {
            tracing::warn!(
                "lyrics bootstrap: pip install audio-separator exited {sep_status} (tolerated, final is_ready check decides)"
            );
        }

        // Verify the install actually worked regardless of pip's exit codes.
        // Retry up to 5× with backoff: immediately after `pip install
        // --force-reinstall torch`, Windows sometimes fails to import the
        // freshly-written ~300 `.pyd` files for a brief window (file system
        // cache / antivirus scan). Retrying gives the OS time to settle.
        let mut ok = false;
        for attempt in 0..5 {
            if is_ready(&venv_python).await {
                ok = true;
                break;
            }
            tracing::debug!(
                "lyrics bootstrap: is_ready check failed (attempt {attempt}), retrying"
            );
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        if !ok {
            anyhow::bail!(
                "lyrics bootstrap: post-install is_ready check failed after 5 attempts — qwen_asr or CUDA torch not available"
            );
        }

        // 3. Preload the model so the first song doesn't pay the 1.2GB download.
        tracing::info!("lyrics bootstrap: preloading Qwen3-ForcedAligner model");
        let mut preload = Command::new(&venv_python);
        preload
            .arg(script_path)
            .args(["preload", "--models-dir"])
            .arg(models_dir)
            .env("HF_HOME", models_dir);
        preload.creation_flags(0x08000000);
        let mut preload_child = preload.spawn().context("failed to spawn preload")?;
        let preload_status =
            match tokio::time::timeout(std::time::Duration::from_secs(900), preload_child.wait())
                .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => anyhow::bail!("model preload failed: {e}"),
                Err(_) => {
                    let _ = preload_child.kill().await;
                    anyhow::bail!("model preload timed out after 15 minutes");
                }
            };
        if !preload_status.success() {
            anyhow::bail!("model preload exited with status {preload_status}");
        }

        tracing::info!("lyrics bootstrap: ready");
        Ok(Some(venv_python))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_python_path_windows_layout() {
        #[cfg(target_os = "windows")]
        {
            let p = venv_python_path(Path::new("C:\\tools"));
            assert_eq!(
                p,
                PathBuf::from("C:\\tools\\lyrics_venv\\Scripts\\python.exe")
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            let p = venv_python_path(Path::new("/tmp/tools"));
            assert_eq!(p, PathBuf::from("/tmp/tools/lyrics_venv/bin/python"));
        }
    }

    #[tokio::test]
    async fn is_ready_false_when_missing() {
        let result = is_ready(Path::new("/definitely/not/a/real/path/python")).await;
        assert!(!result);
    }

    /// The `is_ready` Python probe must import every runtime dependency
    /// the lyrics worker uses at alignment time. Each import is listed
    /// separately so an unrelated formatting change does not silently
    /// hide a missing package.
    #[test]
    fn is_ready_probe_imports_every_required_package() {
        for pkg in ["qwen_asr", "torch", "audio_separator"] {
            assert!(
                IS_READY_PROBE.contains(pkg),
                "IS_READY_PROBE must import {pkg}, got: {IS_READY_PROBE:?}"
            );
        }
    }

    /// The `is_ready` probe must also gate on `torch.cuda.is_available()`
    /// — a CPU-only torch venv wastes minutes per alignment on CPU
    /// inference. This is the check that forces bootstrap to re-install
    /// CUDA torch when the CPU variant slipped in.
    #[test]
    fn is_ready_probe_gates_on_cuda_availability() {
        assert!(
            IS_READY_PROBE.contains("torch.cuda.is_available()"),
            "IS_READY_PROBE must check torch.cuda.is_available(), got: {IS_READY_PROBE:?}"
        );
    }

    /// The pip package spec for the Mel-Roformer dependency must include
    /// the `[gpu]` extra; `audio-separator` alone pulls CPU ONNX Runtime
    /// which is 2-3× slower and blocks Mel-Roformer from using the GPU.
    #[test]
    fn audio_separator_package_includes_gpu_extra() {
        assert!(
            AUDIO_SEPARATOR_PACKAGE.contains("[gpu]"),
            "AUDIO_SEPARATOR_PACKAGE must request the [gpu] extra, got: {AUDIO_SEPARATOR_PACKAGE:?}"
        );
    }
}
