//! Bootstrap the Python environment used by Qwen3-ForcedAligner.
//!
//! On Windows, ensures `{tools_dir}/lyrics_venv/` exists with `qwen-asr`
//! installed and the Qwen3-ForcedAligner-0.6B model cached locally. On
//! non-Windows, returns `Ok(None)` — alignment is a Windows-only feature.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Returns the absolute path to the venv Python interpreter, or `None`
/// if the bootstrap is skipped (non-Windows).
#[cfg(target_os = "windows")]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir
        .join("lyrics_venv")
        .join("Scripts")
        .join("python.exe")
}

#[cfg(not(target_os = "windows"))]
pub fn venv_python_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("lyrics_venv").join("bin").join("python")
}

/// Returns `true` if `python_path` exists AND running `python_path -c "import qwen_asr"`
/// exits 0 within 10 seconds. Used to decide whether bootstrap is needed.
#[cfg_attr(test, mutants::skip)]
pub async fn is_ready(python_path: &Path) -> bool {
    use tokio::process::Command;

    if !python_path.exists() {
        return false;
    }

    let mut cmd = Command::new(python_path);
    cmd.args(["-c", "import qwen_asr"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let res = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.status()).await;

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

        // 2. Install qwen-asr into the venv.
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
                Ok(Err(e)) => anyhow::bail!("pip install qwen-asr failed: {e}"),
                Err(_) => {
                    let _ = pip_child.kill().await;
                    anyhow::bail!("pip install qwen-asr timed out after 10 minutes");
                }
            };
        if !pip_status.success() {
            anyhow::bail!("pip install qwen-asr exited with status {pip_status}");
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
}
