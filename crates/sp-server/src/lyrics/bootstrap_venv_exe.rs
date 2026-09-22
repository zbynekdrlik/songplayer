//! #168 — materialise an APP-OWNED venv interpreter and inject the mimalloc
//! override into it.
//!
//! On the box, `lyrics_venv\Scripts\python.exe` is CPython's venv REDIRECTOR
//! (`venvlauncher.exe`); the interpreter that actually runs torch is its child,
//! the SYSTEM `C:\Program Files\Python312\python.exe`. An allocator override
//! must live in a binary the app OWNS — never the shared system exe — so this
//! module replaces the redirector with a real, copies-layout interpreter INSIDE
//! the venv (CPython's `getpath` honours `pyvenv.cfg`'s `home` for a real
//! interpreter placed in the venv dir — the pre-3.7.2 "copies" layout, still
//! supported, so `pip.exe` / `is_ready` / every spawn keep working), then runs
//! mimalloc's `minject --inplace` on that copy.
//!
//! The pure decisions ([`needs_recopy`], [`inject_plan`]) are unit-tested on
//! Linux; the file copies and the `minject` invocation are the Windows-only
//! integration seam, called from `bootstrap.rs` after `is_ready`. Every failure
//! is WARN-and-continue — a missing DLL or a failed inject never blocks the
//! bootstrap; the child just runs unretained, exactly as before #168.

use std::time::SystemTime;

#[cfg(windows)]
use std::path::{Path, PathBuf};

/// The mimalloc override DLL file name.
///
/// This is the CMake **Release** output of the `mimalloc` shared target at the
/// pinned tag (v2.2.7): `mi_libname = "mimalloc"` (no `-secure`/`-debug` suffix
/// in Release), so the DLL is `mimalloc.dll` — NOT `mimalloc-override.dll`. It
/// is ALSO minject's default injection target (`--postfix` changes it), so the
/// whole vendor override flow (build → stage → `minject --inplace`) requires
/// exactly this name; `mimalloc-override.dll` would break minject's default.
/// (Verified against microsoft/mimalloc CMakeLists.txt + bin/readme.md @ v2.2.7.)
pub const MIMALLOC_DLL: &str = "mimalloc.dll";

/// The redirection DLL that must sit beside [`MIMALLOC_DLL`] at runtime (a
/// dependency of it); the repo ships the prebuilt x64 one in `bin/`.
pub const MIMALLOC_REDIRECT_DLL: &str = "mimalloc-redirect.dll";

/// The mimalloc import-table patcher (x64), shipped prebuilt in the repo `bin/`.
pub const MINJECT_EXE: &str = "minject.exe";

/// A file's size + modification time — the identity used to decide whether a
/// materialised copy is still current with its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime: SystemTime,
}

/// Whether to run `minject` on the venv interpreter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectPlan {
    /// Do not inject — the reason (no resources staged, or already injected).
    Skip(&'static str),
    /// Run `minject --inplace` to make the mimalloc override the first import.
    Inject,
}

/// Pure: does `dst` need to be (re)copied from `src`? True when `dst` is missing
/// (never materialised), or differs from `src` in length or mtime (a system
/// Python patch update re-materialises the copy on the next boot).
pub fn needs_recopy(src: FileStamp, dst: Option<FileStamp>) -> bool {
    match dst {
        None => true,
        Some(d) => d.len != src.len || d.mtime != src.mtime,
    }
}

/// Pure: whether to `minject` the mimalloc override into the venv interpreter.
/// Skip when the staged resources are missing (nothing to inject) or when the
/// override already loads in the venv exe (idempotent — the `GetModuleHandleW`
/// probe decides); otherwise inject.
pub fn inject_plan(resources_present: bool, already_injected: bool) -> InjectPlan {
    if !resources_present {
        InjectPlan::Skip("mimalloc resources not staged")
    } else if already_injected {
        InjectPlan::Skip("mimalloc.dll already loads in the venv interpreter")
    } else {
        InjectPlan::Inject
    }
}

// ---------------------------------------------------------------------------
// Windows integration seam (materialise + inject). Best-effort: every failure
// bubbles to `prepare_heavy_interpreter`, which logs WARN and returns — the
// bootstrap is NEVER blocked; the child just runs unretained, as before #168.
// ---------------------------------------------------------------------------

/// Materialise an app-owned copies-layout venv interpreter (replacing CPython's
/// redirector) and, when the staged mimalloc resources are present, inject the
/// mimalloc override into it. Called from `bootstrap.rs` after `is_ready`, on
/// every boot; idempotent (a steady-state, already-injected interpreter is left
/// untouched). Non-Windows: a no-op.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub async fn prepare_heavy_interpreter(tools_dir: &Path, venv_python: &Path) {
    if let Err(e) = materialize_and_inject(tools_dir, venv_python).await {
        tracing::warn!("lyrics bootstrap: heavy interpreter prep skipped: {e:#}");
    }
}

/// Non-Windows: the box is Windows-only prod; nothing to do.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
pub async fn prepare_heavy_interpreter(
    _tools_dir: &std::path::Path,
    _venv_python: &std::path::Path,
) {
}

#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
async fn materialize_and_inject(tools_dir: &Path, venv_python: &Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let scripts_dir = venv_python
        .parent()
        .context("venv python.exe has no parent (Scripts) dir")?;
    let venv_dir = scripts_dir
        .parent()
        .context("venv Scripts has no parent (venv) dir")?;
    let home = read_pyvenv_home(&venv_dir.join("pyvenv.cfg"))
        .with_context(|| format!("no `home =` in {}/pyvenv.cfg", venv_dir.display()))?;

    // Locate the staged mimalloc resources and whether the current venv exe is
    // already our injected interpreter (the steady state). "Already injected"
    // means the override actually LOADS in the current venv exe (the vendor
    // probe) — never a parse of `minject -l` output whose format we do not own:
    // a false "yes" there would skip the inject forever on a fresh exe.
    let resources = locate_mimalloc_resources(tools_dir);
    let injected = resources.is_some() && probe_mimalloc_active(venv_python).await == Some(true);

    // Refresh the interpreter's DLLs from `home` (idempotent — these are never
    // patched by minject, so a copy makes dst byte-identical to src; a system
    // Python patch update changes their size/mtime → re-copy on the next boot).
    let dll_count = materialize_venv_dlls(&home, scripts_dir)?;

    // (Re)materialise python.exe ONLY when the current venv exe is not our
    // injected interpreter — copying a pristine exe over an injected one would
    // wipe the injection every boot (an injected exe never equals its source).
    if !injected {
        copy_if_changed(&home.join("python.exe"), &scripts_dir.join("python.exe"))?;
        tracing::info!(
            "lyrics bootstrap: venv interpreter materialised from {} (python.exe + {dll_count} dll)",
            home.display()
        );
    } else {
        tracing::info!(
            "lyrics bootstrap: venv interpreter already app-owned + injected ({dll_count} dll current)"
        );
    }

    match inject_plan(resources.is_some(), injected) {
        InjectPlan::Skip(reason) => {
            tracing::info!("lyrics bootstrap: mimalloc inject skipped ({reason})");
        }
        InjectPlan::Inject => {
            let dir = resources
                .as_ref()
                .expect("InjectPlan::Inject implies resources present");
            // The override DLL + its redirect dependency must sit beside python.exe.
            copy_if_changed(&dir.join(MIMALLOC_DLL), &scripts_dir.join(MIMALLOC_DLL))?;
            copy_if_changed(
                &dir.join(MIMALLOC_REDIRECT_DLL),
                &scripts_dir.join(MIMALLOC_REDIRECT_DLL),
            )?;
            run_minject_inplace(&dir.join(MINJECT_EXE), &scripts_dir.join("python.exe")).await?;
        }
    }

    // Confirm the override actually loads whenever resources are present (every
    // boot, so a regression is visible in the log — the #168 acceptance line).
    if resources.is_some() {
        match probe_mimalloc_active(venv_python).await {
            Some(true) => tracing::info!("lyrics bootstrap: mimalloc override active"),
            Some(false) => {
                tracing::warn!(
                    "lyrics bootstrap: mimalloc override inactive (injected, not loaded)"
                )
            }
            None => tracing::warn!("lyrics bootstrap: mimalloc override probe inconclusive"),
        }
    }
    Ok(())
}

/// Parse the `home = <path>` line from a venv `pyvenv.cfg` (the real interpreter
/// directory the copies-layout venv is based on).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn read_pyvenv_home(pyvenv_cfg: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(pyvenv_cfg).ok()?;
    for line in text.lines() {
        if let Some((key, val)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("home")
        {
            let val = val.trim();
            if !val.is_empty() {
                return Some(PathBuf::from(val));
            }
        }
    }
    None
}

/// Copy every `python3*.dll` + `vcruntime140*.dll` from `home` into
/// `scripts_dir`, re-copying only when size/mtime differ. Returns the count of
/// such DLLs present after the pass.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn materialize_venv_dlls(home: &Path, scripts_dir: &Path) -> anyhow::Result<usize> {
    use anyhow::Context;
    let mut count = 0usize;
    for entry in std::fs::read_dir(home).with_context(|| format!("read_dir {}", home.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let lower = name.to_string_lossy().to_ascii_lowercase();
        let wanted = (lower.starts_with("python3") && lower.ends_with(".dll"))
            || (lower.starts_with("vcruntime140") && lower.ends_with(".dll"));
        if wanted {
            copy_if_changed(&home.join(&name), &scripts_dir.join(&name))?;
            count += 1;
        }
    }
    Ok(count)
}

/// Copy `src` → `dst` when [`needs_recopy`] says so; returns whether a copy ran.
/// `std::fs::copy` on Windows (`CopyFileExW`) preserves the source's last-write
/// time, so an unchanged file is a stable no-op on the next boot.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn copy_if_changed(src: &Path, dst: &Path) -> anyhow::Result<bool> {
    use anyhow::Context;
    let src_stamp = stamp_of(src).with_context(|| format!("source missing: {}", src.display()))?;
    if !needs_recopy(src_stamp, stamp_of(dst)) {
        return Ok(false);
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(true)
}

/// The size+mtime stamp of `p`, or `None` if it cannot be read.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn stamp_of(p: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(p).ok()?;
    Some(FileStamp {
        len: m.len(),
        mtime: m.modified().ok()?,
    })
}

/// Find the staged mimalloc resources: first next to the running exe
/// (`<exe dir>/resources/mimalloc/`, the Tauri-bundled location), else a
/// hand-dropped dev copy at `<tools_dir>/mimalloc/`. Logs the resolved path.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn locate_mimalloc_resources(tools_dir: &Path) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let bundled = exe_dir.join("resources").join("mimalloc");
        if bundled.join(MIMALLOC_DLL).exists() {
            tracing::info!(
                "lyrics bootstrap: mimalloc resources at {}",
                bundled.display()
            );
            return Some(bundled);
        }
    }
    let fallback = tools_dir.join("mimalloc");
    if fallback.join(MIMALLOC_DLL).exists() {
        tracing::info!(
            "lyrics bootstrap: mimalloc resources at {} (tools fallback)",
            fallback.display()
        );
        return Some(fallback);
    }
    None
}

/// Run `minject --force --inplace <exe>` (force = no interactive prompt in a
/// headless service). Errors on a non-zero exit / timeout.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
async fn run_minject_inplace(minject: &Path, exe: &Path) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(minject);
    cmd.arg("--force").arg("--inplace").arg(exe);
    match run_capture(&mut cmd, 60).await {
        Some(o) if o.status.success() => {
            tracing::info!("lyrics bootstrap: minject --inplace {} ok", exe.display());
            Ok(())
        }
        Some(o) => anyhow::bail!(
            "minject exited {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        None => anyhow::bail!("minject timed out or failed to spawn"),
    }
}

/// Probe whether `mimalloc.dll` is loaded into the venv interpreter's process.
/// `Some(true|false)` on a clean run, `None` if the probe itself failed.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
async fn probe_mimalloc_active(venv_python: &Path) -> Option<bool> {
    let mut cmd = tokio::process::Command::new(venv_python);
    cmd.arg("-c").arg(
        "import ctypes,sys;sys.stdout.write('1' if ctypes.windll.kernel32.GetModuleHandleW('mimalloc.dll') else '0')",
    );
    let o = run_capture(&mut cmd, 30).await?;
    if !o.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&o.stdout).trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Run `cmd` hidden (CREATE_NO_WINDOW), capture its output, bounded by
/// `timeout_secs`. `None` on spawn failure or timeout.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
async fn run_capture(
    cmd: &mut tokio::process::Command,
    timeout_secs: u64,
) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await {
        Ok(Ok(o)) => Some(o),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stamp(len: u64, secs: u64) -> FileStamp {
        FileStamp {
            len,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    #[test]
    fn needs_recopy_true_when_dst_missing() {
        assert!(needs_recopy(stamp(100, 5), None));
    }

    #[test]
    fn needs_recopy_true_on_size_mismatch() {
        assert!(needs_recopy(stamp(100, 5), Some(stamp(200, 5))));
    }

    #[test]
    fn needs_recopy_true_on_mtime_mismatch() {
        assert!(needs_recopy(stamp(100, 5), Some(stamp(100, 9))));
    }

    #[test]
    fn needs_recopy_false_when_equal() {
        assert!(!needs_recopy(stamp(100, 5), Some(stamp(100, 5))));
    }

    #[test]
    fn inject_plan_skips_without_resources() {
        assert!(matches!(inject_plan(false, false), InjectPlan::Skip(_)));
        assert!(matches!(inject_plan(false, true), InjectPlan::Skip(_)));
    }

    #[test]
    fn inject_plan_skips_when_already_injected() {
        assert!(matches!(inject_plan(true, true), InjectPlan::Skip(_)));
    }

    #[test]
    fn inject_plan_injects_when_present_and_not_injected() {
        assert_eq!(inject_plan(true, false), InjectPlan::Inject);
    }

    /// Locks the CMake-Release output name for the pinned mimalloc tag — also
    /// minject's default injection target. `mimalloc-override.dll` (an older
    /// name) would break the default flow; see the const's doc.
    #[test]
    fn mimalloc_dll_name_is_the_cmake_release_output() {
        assert_eq!(MIMALLOC_DLL, "mimalloc.dll");
        assert_eq!(MIMALLOC_REDIRECT_DLL, "mimalloc-redirect.dll");
        assert_eq!(MINJECT_EXE, "minject.exe");
    }
}
