//! Bootstrap the lyrics venv: the Python environment of `preprocess-vocals`
//! (anvuew dereverb), the stems worker (`stem_worker.py`) and the dub worker.
//!
//! On Windows, ensures `{tools_dir}/lyrics_venv/` exists with
//! `audio-separator[gpu]`, the cu124 torch triplet and the numeric stack, and
//! the anvuew dereverb model cached locally. On non-Windows, returns
//! `Ok(None)` — the heavy steps are a Windows-only feature. The forced
//! aligner (mtl) has its own venv; #144 deleted the retired Qwen aligner
//! package from this one (v22 one regime).

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::lyrics::bootstrap_probe::{PROBE_TIMEOUT, Readiness};

/// Prepend `dir` to the current `PATH` env var, with the OS-appropriate
/// separator, and return the joined string. Exposed as `pub(crate)` so
/// `aligner.rs` can use the same helper when spawning its subprocesses.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn prepend_path_with(dir: &Path) -> std::ffi::OsString {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let existing = std::env::var("PATH").unwrap_or_default();
    let mut joined = std::ffi::OsString::from(dir);
    joined.push(separator);
    joined.push(existing);
    joined
}

/// The `-c` script passed to the venv Python by `is_ready` to verify every
/// Python package the venv's live scripts depend on is importable AND CUDA is
/// available. Exit code 0 iff all conditions hold:
///   1. `torch` importable
///   2. `audio_separator` importable (the anvuew dereverb and the stems
///      worker's Kim separation)
///   3. `numba` + `librosa` + `soundfile` importable — the numeric stack
///      `preprocess-vocals` runs on. A too-new numpy (>= 2.5) breaks numba's
///      import ("Numba needs NumPy 2.4 or less"), so importing them here is
///      what makes a broken pin report "not ready" and trigger the repair;
///      without it the venv reported ready while every song failed isolation
///      (win-resolume, 2026-09-11, #144).
///   4. `torch.cuda.is_available()` returns True
///
/// #144: the retired Qwen aligner package is not probed: nothing live
/// imports it, so a broken copy left in an old venv must not reinstall it.
const IS_READY_PROBE: &str = "import torch, audio_separator, numba, librosa, soundfile, sys; sys.exit(0 if torch.cuda.is_available() else 1)";

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

/// numpy pin repaired AFTER the cu124 torch force-reinstall. numba 0.65/0.66
/// (April) cap numpy at 2.4 ("Numba needs NumPy 2.4 or less. Got NumPy 2.5.");
/// the torch `--upgrade --force-reinstall` in step 2b re-resolves torch's
/// dependency tree ignoring the constraints of already-installed packages, so
/// on win-resolume (2026-09-11, #144) it pulled numpy 2.5.2 next to numba
/// 0.65 and every `preprocess-vocals` run (librosa → numba) failed. Re-pin
/// numpy below 2.5 LAST so the numeric stack imports again.
#[allow(dead_code)] // only referenced inside #[cfg(target_os = "windows")] bootstrap
const NUMPY_PIN: &str = "numpy<2.5";

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

/// The pinned `google-genai` SDK spec for the dub worker (#183 D4). The Gemini
/// Live Translate child imports `google.genai`; this is the version verified
/// against `gemini-3.5-live-translate-preview`.
pub const GENAI_PACKAGE: &str = "google-genai==2.24.0";

/// Ensure `google-genai` is importable in the lyrics venv (#183 D4). Idempotent
/// and LIGHT: probes `import google.genai` first (a fast subprocess) and only
/// pip-installs [`GENAI_PACKAGE`] when it is missing — so it never triggers the
/// heavy torch reinstall the `is_ready` gate does. Returns the SDK version
/// string on success (logged as the design's startup self-check). Called by the
/// dub worker before its first synthesis, not by the main bootstrap gate.
#[cfg_attr(test, mutants::skip)]
pub async fn ensure_genai(venv_python: &Path) -> anyhow::Result<String> {
    use anyhow::Context;
    use tokio::process::Command;

    async fn probe_version(py: &Path) -> Option<String> {
        let mut cmd = Command::new(py);
        cmd.args([
            "-c",
            "import google.genai as g, sys; sys.stdout.write(getattr(g, '__version__', 'unknown'))",
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd.output().await.ok()?;
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
        None
    }

    if let Some(v) = probe_version(venv_python).await {
        tracing::info!("dub worker: google-genai already present (v{v})");
        return Ok(v);
    }

    tracing::info!("dub worker: installing {GENAI_PACKAGE} into the lyrics venv");
    let mut pip = Command::new(venv_python);
    pip.args(["-m", "pip", "install", "-U", GENAI_PACKAGE]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        pip.creation_flags(0x0800_0000);
    }
    let mut child = pip
        .spawn()
        .context("failed to spawn pip install google-genai")?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(300), child.wait())
        .await
        .context("pip install google-genai timed out")?
        .context("pip install google-genai wait failed")?;
    if !status.success() {
        tracing::warn!(
            "dub worker: pip install google-genai exited {status} (tolerated; the import probe decides)"
        );
    }
    probe_version(venv_python)
        .await
        .ok_or_else(|| anyhow::anyhow!("google-genai still not importable after install"))
}

/// Probe the venv: `python_path -c IS_READY_PROBE` (every package imports
/// AND `torch.cuda.is_available()`), answered with its REASON (#221 BLOCKER):
/// `Ready`, `Missing` (no interpreter), `Timeout` (not done within
/// `PROBE_TIMEOUT`; the probe is killed), or `Failed` with the exit code and
/// the end of its stderr. `bootstrap_probe::decide` turns it into the
/// fast-path decision. A venv with CPU-only torch fails the probe (exit 1, no
/// traceback), is retried, and gets the CUDA variant only after the retries.
#[cfg_attr(test, mutants::skip)]
pub async fn is_ready(python_path: &Path) -> Readiness {
    use std::process::Stdio;
    use tokio::process::Command;

    if !python_path.exists() {
        return Readiness::Missing;
    }

    // IMPORTANT: do NOT set CREATE_NO_WINDOW on Windows here.
    //
    // On the production win-resolume machine (2026-04-19), a fresh SongPlayer
    // restart found that invoking the venv Python with `CREATE_NO_WINDOW`
    // (0x08000000) caused PyTorch's `torch.cuda.is_available()` to return
    // False even though the same command run from a normal console or via
    // PowerShell without the flag reports True. The exact interaction is
    // unknown (likely CUDA driver/context probing that depends on a console
    // handle), but the consequence is severe: `is_ready` returned false,
    // bootstrap concluded the venv was broken, and kicked off a 10-15 min
    // pip reinstall of the whole venv (audio-separator[gpu] + torch). That
    // repeated on every SongPlayer restart, stalling the lyrics worker.
    //
    // The `is_ready` probe is internal-only: there's no user-facing console
    // to hide. Keeping the flag off costs nothing and restores reliable
    // CUDA detection. The CREATE_NO_WINDOW flag remains on the longer-
    // running subprocess calls (preprocess-vocals, the stem separation)
    // where a brief window flicker during a long run would be visible.
    //
    // #221: stderr is captured (the reason: a traceback's last line names an
    // import failure), and a probe that runs out of time is killed
    // (`kill_on_drop`), so it never holds the GPU while the next one runs.
    let mut cmd = Command::new(python_path);
    cmd.args(["-c", IS_READY_PROBE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Readiness::failed(None, &e.to_string()),
    };
    // The first CUDA init on a cold driver can take 10-20 s on its own.
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Err(_) => Readiness::Timeout,
        Ok(Err(e)) => Readiness::failed(None, &e.to_string()),
        Ok(Ok(out)) if out.status.success() => Readiness::Ready,
        Ok(Ok(out)) => Readiness::failed(out.status.code(), &String::from_utf8_lossy(&out.stderr)),
    }
}

/// Ensure the lyrics venv exists, its packages are installed, and the anvuew
/// dereverb model is preloaded.
///
/// On Windows:
///   1. Create `{tools_dir}/lyrics_venv/` via `python -m venv` (if missing).
///   2. Install `audio-separator[gpu]`, then the cu124 torch triplet, then
///      repair the numpy pin.
///   3. Run `{venv}/Scripts/python.exe {script_path} preload --models-dir ...`.
///
/// Fast-paths return `Ok(venv_python)` when `bootstrap_probe::decide` says the
/// venv is ready (a slow or failed cold probe is RETRIED for ~3 min first,
/// #221) or kept timing out (used as it is: a timeout never reinstalls).
/// Only a proven import failure, a missing interpreter, or a failure that
/// outlived the retries runs the install.
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
        use crate::lyrics::bootstrap_probe::{FastPath, RETRY_PLAN, decide, install_worked};
        use anyhow::Context;
        use tokio::process::Command;

        let venv_python = venv_python_path(tools_dir);
        let venv_dir = tools_dir.join("lyrics_venv");

        // #221: the probe's reason decides; a timeout or a CUDA / init failure
        // is retried (~3 min) before any install.
        let decision = decide(|| is_ready(&venv_python), RETRY_PLAN).await;
        if decision.path != FastPath::Install {
            if decision.path == FastPath::UseAsIs {
                tracing::warn!(
                    probes = decision.probes,
                    "lyrics bootstrap: the venv probe kept timing out — using the venv at {} as it is (a timeout never reinstalls)",
                    venv_python.display()
                );
            } else {
                tracing::info!(
                    probes = decision.probes,
                    "lyrics bootstrap: venv already ready at {}",
                    venv_python.display()
                );
            }
            // #168: replace the venv redirector with an app-owned interpreter
            // and inject the retained mimalloc heap (idempotent; WARN-and-continue).
            crate::lyrics::bootstrap_venv_exe::prepare_heavy_interpreter(tools_dir, &venv_python)
                .await;
            return Ok(Some(venv_python));
        }
        tracing::info!(
            probes = decision.probes,
            "lyrics bootstrap: the venv needs the install (see the probe's reason above)"
        );

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

        use std::os::windows::process::CommandExt;

        // 2a. Install audio-separator[gpu] (the anvuew dereverb, the stems
        // worker's Kim separation; it brings librosa, soundfile, numpy and
        // scipy). pip's exit code is NOT authoritative: in non-TTY mode it
        // sometimes returns 1 for benign warnings (like leftover
        // `~distribution` stubs from a prior partial install). We log but do
        // not bail on non-zero; the final is_ready check is the real success
        // gate.
        //
        // Ordering: install BEFORE the CUDA torch force-reinstall below.
        // `audio-separator[gpu]` pulls onnxruntime-gpu plus its own torch
        // build — installing it AFTER cu124 torch was observed to clobber
        // cu124 with the audio-separator sibling torch, breaking CUDA in
        // `is_ready`. Installing it BEFORE means the final force-reinstall
        // of cu124 torch in step 2b is authoritative and wins.
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

        // 2b. Force-reinstall torch with CUDA support. `audio-separator[gpu]`
        // in step 2a pulls its own sibling torch build (the CPU-only wheel
        // from PyPI) that clobbers cu124 unless we run this AFTER it. GPU
        // separation without CUDA takes minutes instead of seconds.
        // Install the cu124 variant from the PyTorch index LAST so it wins
        // — pip `--force-reinstall` on `torch` alone replaces whatever
        // torch build the earlier steps left behind.
        //
        // Pin the triplet: installing `torch` alone with --force-reinstall
        // on win-resolume produced torchvision 0.26 + torchaudio 2.11, which
        // bind against a torch 2.11 ABI that doesn't exist on the cu124
        // index. Matched versions keep torchvision importable.
        tracing::info!("lyrics bootstrap: installing CUDA torch variant");
        let mut torch_pip = Command::new(&venv_python);
        torch_pip.args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "--force-reinstall",
            "torch==2.6.0+cu124",
            "torchvision==0.21.0+cu124",
            "torchaudio==2.6.0+cu124",
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

        // 2c. Repair the numpy pin (#144). The cu124 torch force-reinstall in
        // step 2b re-resolves torch's dependency tree with `--force-reinstall`,
        // ignoring the constraints of already-installed packages — on
        // win-resolume (2026-09-11) it pulled numpy 2.5.2 next to numba 0.65,
        // and numba 0.65/0.66 cap numpy at 2.4, so every `preprocess-vocals`
        // run (librosa → numba) died with "Numba needs NumPy 2.4 or less. Got
        // NumPy 2.5." Re-pin numpy below 2.5 LAST so the numeric stack imports
        // again. WARN-and-tolerate like the other pip steps; the final
        // is_ready probe (now importing numba/librosa/soundfile) is the gate.
        tracing::info!("lyrics bootstrap: repairing numpy pin ({NUMPY_PIN})");
        let mut numpy_pip = Command::new(&venv_python);
        numpy_pip.args(["-m", "pip", "install", NUMPY_PIN]);
        numpy_pip.creation_flags(0x08000000);
        let mut numpy_child = numpy_pip
            .spawn()
            .context("failed to spawn numpy pin pip install")?;
        let numpy_status =
            match tokio::time::timeout(std::time::Duration::from_secs(600), numpy_child.wait())
                .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => anyhow::bail!("numpy pin install spawn failed: {e}"),
                Err(_) => {
                    let _ = numpy_child.kill().await;
                    anyhow::bail!("numpy pin install timed out after 10 minutes");
                }
            };
        if !numpy_status.success() {
            tracing::warn!(
                "lyrics bootstrap: numpy pin install exited {numpy_status} (tolerated, final is_ready check decides)"
            );
        }

        // Verify the install actually worked regardless of pip's exit codes,
        // with the #221 Readiness policy of the fast path (release 0.69.0
        // review 🔵 9): `decide` logs every probe's reason (WARN) and retries a
        // timeout or an init failure (~3 min — also the time Windows needs to
        // load the freshly written `.pyd` files past an antivirus scan, which
        // fails as `DLL load failed`, a retry). A probe that keeps timing out
        // is a slow GPU / driver, never a failed install (five cold CUDA
        // timeouts used to fail it and disable isolation for the whole
        // process); a proven import failure, or a failure that outlives the
        // retries, is.
        let verified = decide(|| is_ready(&venv_python), RETRY_PLAN).await;
        if !install_worked(verified.path) {
            anyhow::bail!(
                "lyrics bootstrap: the post-install venv check failed after {} probe(s) — audio_separator, the numeric stack or CUDA torch is not available (each probe's reason is in its WARN above)",
                verified.probes
            );
        }
        if verified.path == FastPath::UseAsIs {
            tracing::warn!(
                probes = verified.probes,
                "lyrics bootstrap: the post-install venv probe kept timing out — going on with the installed venv (a timeout is never a failed install)"
            );
        }

        // 3. Preload the anvuew dereverb model so the first song doesn't pay
        // its download inside the isolation's stall timeout.
        tracing::info!("lyrics bootstrap: preloading the anvuew dereverb model");
        let mut preload = Command::new(&venv_python);
        preload
            .arg(script_path)
            .args(["preload", "--models-dir"])
            .arg(models_dir)
            .env("HF_HOME", models_dir)
            // audio-separator shells out to ffmpeg.exe without a full path,
            // so the Python subprocess needs tools_dir prepended to PATH —
            // that's where the app's bundled ffmpeg.exe lives alongside
            // yt-dlp.exe. Without this the preload fails with
            // "[WinError 2] The system cannot find the file specified"
            // from inside audio_separator's constructor.
            .env("PATH", prepend_path_with(tools_dir));
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
        // #168: replace the venv redirector with an app-owned interpreter and
        // inject the retained mimalloc heap (idempotent; WARN-and-continue).
        crate::lyrics::bootstrap_venv_exe::prepare_heavy_interpreter(tools_dir, &venv_python).await;
        Ok(Some(venv_python))
    }
}

#[path = "bootstrap_tests_numpy_pin.rs"]
#[cfg(test)]
mod bootstrap_tests_numpy_pin;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::bootstrap_probe::Readiness;

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
    async fn is_ready_reports_a_missing_interpreter() {
        let result = is_ready(Path::new("/definitely/not/a/real/path/python")).await;
        assert_eq!(result, Readiness::Missing);
    }

    /// The `is_ready` Python probe must import every runtime dependency
    /// the venv's live scripts use (`preprocess-vocals`, `stem_worker.py`).
    /// Each import is listed separately so an unrelated formatting change
    /// does not silently hide a missing package.
    #[test]
    fn is_ready_probe_imports_every_required_package() {
        for pkg in ["torch", "audio_separator", "numba", "librosa", "soundfile"] {
            assert!(
                IS_READY_PROBE.contains(pkg),
                "IS_READY_PROBE must import {pkg}, got: {IS_READY_PROBE:?}"
            );
        }
    }

    /// #144: v22 is one regime (mtl force-align in its own venv + Gemini 3.5
    /// Transcribe). Nothing live imports the retired Qwen aligner package, so
    /// a broken copy of it must never make the venv "not ready" (a full
    /// reinstall), and the install never fetches it. The names are split so
    /// this file does not contain what it looks for.
    #[test]
    fn the_venv_needs_no_retired_aligner_package() {
        let retired = ["qw", "en"].concat();
        assert!(
            !IS_READY_PROBE.to_lowercase().contains(&retired),
            "IS_READY_PROBE must not import the retired aligner, got: {IS_READY_PROBE:?}"
        );
        let pip_arg = ["\"qwen", "-asr\""].concat();
        assert!(
            !include_str!("bootstrap.rs").contains(&pip_arg),
            "the bootstrap must not pip install the retired aligner"
        );
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

    /// bootstrap must pin torch + torchvision + torchaudio to versions that
    /// form a compatible ABI triplet. Observed on win-resolume: installing
    /// `torch` alone with --force-reinstall leaves torchvision at 0.26 and
    /// torchaudio at 2.11, which binds against a torch 2.11 ABI that
    /// doesn't exist on the cu124 index — importing torchvision fails with
    /// "operator torchvision::nms does not exist".
    #[test]
    fn bootstrap_pins_matched_torch_triplet() {
        let src = include_str!("bootstrap.rs");
        assert!(
            src.contains("torch==2.6.0+cu124"),
            "bootstrap.rs must pin torch==2.6.0+cu124"
        );
        assert!(
            src.contains("torchvision==0.21.0+cu124"),
            "bootstrap.rs must pin torchvision==0.21.0+cu124"
        );
        assert!(
            src.contains("torchaudio==2.6.0+cu124"),
            "bootstrap.rs must pin torchaudio==2.6.0+cu124"
        );
    }

    /// The anvuew dereverb model (SDR 19.17, 2026 SOTA) must be referenced
    /// in the Python helper so preload warms it at bootstrap. This ensures
    /// the first song doesn't pay the ~500 MB download inside the
    /// alignment subprocess timeout.
    #[test]
    fn bootstrap_preloads_anvuew_dereverb() {
        let py_src = include_str!("../../../../scripts/lyrics_worker.py");
        assert!(
            py_src.contains("dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt"),
            "lyrics_worker.py must reference the anvuew dereverb checkpoint"
        );
    }
}
