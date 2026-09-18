//! Centralized yt-dlp command construction + JS-runtime (Deno) support for
//! YouTube's n-challenge (#189).
//!
//! Since ~2026-09 YouTube requires yt-dlp's EJS solver to run JavaScript (the
//! "n-challenge"). yt-dlp delegates to an external runtime and enables **Deno**
//! by default when it is on `PATH`; node/quickjs/bun are opt-in and are NOT the
//! runtime yt-dlp's solver wants (the box had `--js-runtimes node` forced and
//! no node installed → every new download failed `n challenge solving failed`).
//!
//! The fix ships a pinned `deno.exe` into the tools dir next to yt-dlp/ffmpeg
//! (see [`super::tools::ToolsManager::ensure_deno`]) and routes EVERY yt-dlp
//! spawn through [`ytdlp_command`], which prepends the tools dir to the child's
//! `PATH` (so the bundled deno is found first) and adds `--js-runtimes deno`
//! when the installed yt-dlp advertises the flag.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::OnceLock;

/// Pinned Deno release shipped with the tools dir. Bump alongside the
/// `yt-dlp --update` cadence when a newer Deno is needed (update [`DENO_SHA256`]
/// too — it must match the exact zip this version downloads).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const DENO_VERSION: &str = "2.9.7";

/// SHA-256 of `deno-x86_64-pc-windows-msvc.zip` for [`DENO_VERSION`], verified
/// against denoland/deno's published `.sha256sum` and an independent re-hash.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const DENO_SHA256: &str =
    "a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238";

/// yt-dlp runtime token we ship + prefer for the n-challenge solver. Deno is
/// the only runtime yt-dlp enables by default and the one its EJS solver wants.
const PREFERRED_JS_RUNTIME: &str = "deno";

/// GitHub release URL for the pinned Deno Windows (x86_64-pc-windows-msvc) zip.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn deno_asset_url(version: &str) -> String {
    format!(
        "https://github.com/denoland/deno/releases/download/v{version}/deno-x86_64-pc-windows-msvc.zip"
    )
}

/// Build a child-process `PATH` value with `tools_dir` FIRST, so a yt-dlp child
/// resolves the bundled `deno.exe` (and ffmpeg) ahead of anything on the
/// machine `PATH`. Uses the platform `PATH` separator (`;` on Windows, `:` on
/// Unix) via `std::env::join_paths`.
pub(crate) fn path_with_tools(tools_dir: &Path, inherited: Option<&OsStr>) -> OsString {
    let inherited_paths: Vec<_> = match inherited {
        Some(p) => std::env::split_paths(p).collect(),
        None => Vec::new(),
    };
    let all = std::iter::once(tools_dir.to_path_buf()).chain(inherited_paths);
    match std::env::join_paths(all) {
        Ok(joined) => joined,
        // A component containing the separator char is the only failure mode;
        // fall back to the tools dir alone so the bundled deno is still found.
        Err(_) => tools_dir.as_os_str().to_os_string(),
    }
}

/// The explicit yt-dlp JS-runtime args, returned ONLY when the installed yt-dlp
/// advertises the `--js-runtimes` flag in its `--help`. When it does not, the
/// empty list is returned and PATH-based auto-detection is the sole mechanism.
pub(crate) fn js_runtime_args(ytdlp_help_text: &str) -> Vec<String> {
    if ytdlp_help_text.contains("--js-runtimes") {
        vec![
            "--js-runtimes".to_string(),
            PREFERRED_JS_RUNTIME.to_string(),
        ]
    } else {
        Vec::new()
    }
}

/// Parse the version out of `deno --version` stdout (first line, e.g.
/// `deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)`).
pub(crate) fn parse_deno_version(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .next()?
        .strip_prefix("deno ")?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Verdict of the startup JS-runtime self-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsRuntimeStatus {
    /// Deno is present; the n-challenge is solvable. Carries the deno version.
    Ok(String),
    /// No deno on the box — every new download will fail the n-challenge.
    Missing,
    /// Deno is present but the self-check still hit the n-challenge solver
    /// failure (should not happen if deno works). Carries a short reason.
    SolverFailed(String),
}

/// Classify the startup self-check. `js_runtime_ok` for the dashboard is
/// exactly `matches!(_, JsRuntimeStatus::Ok(_))`.
///
/// A self-check that fails for a NON-n-challenge reason (the separate cookie
/// bot-check, #141, or a transient network error) does NOT flip the verdict —
/// the JS runtime itself is in place, which is all this ticket owns.
pub(crate) fn js_runtime_verdict(
    deno_version: Option<&str>,
    simulate_exit_ok: bool,
    stderr: &str,
) -> JsRuntimeStatus {
    match deno_version {
        None => JsRuntimeStatus::Missing,
        Some(ver) => {
            if simulate_exit_ok {
                JsRuntimeStatus::Ok(ver.to_string())
            } else if stderr.contains("n challenge") {
                JsRuntimeStatus::SolverFailed("n challenge solving failed".to_string())
            } else {
                JsRuntimeStatus::Ok(ver.to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Process-global detected runtime args + the shared command builder
// ---------------------------------------------------------------------------

/// Detected once at tools-ready from `yt-dlp --help`; read by every
/// [`ytdlp_command`]. Unset in tests → no runtime flag appended. The support is
/// an install-wide, process-global constant (same yt-dlp binary everywhere), so
/// it is memoized here rather than threaded through every call site.
static JS_RUNTIME_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// Memoize the detected JS-runtime args (idempotent; first writer wins).
pub(crate) fn set_js_runtime_args(args: Vec<String>) {
    let _ = JS_RUNTIME_ARGS.set(args);
}

fn detected_js_runtime_args() -> &'static [String] {
    JS_RUNTIME_ARGS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// The ONE yt-dlp command builder (#189). Every yt-dlp spawn goes through this
/// so the child gets: the tools dir FIRST on `PATH` (bundled deno/ffmpeg),
/// `--js-runtimes deno` when supported, `CREATE_NO_WINDOW`, and UTF-8 stdio.
/// The caller appends the invocation-specific args (the URL stays last).
pub(crate) fn ytdlp_command(ytdlp_path: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    let tools_dir = ytdlp_path.parent().unwrap_or_else(|| Path::new("."));
    cmd.env(
        "PATH",
        path_with_tools(tools_dir, std::env::var_os("PATH").as_deref()),
    );
    super::hide_console_window(&mut cmd);
    super::apply_utf8_env(&mut cmd);
    cmd.args(detected_js_runtime_args());
    cmd
}

/// Public video used by the startup self-check — a stable, always-available
/// upload. `--simulate` still extracts formats, so it exercises the n-challenge.
const SELFCHECK_VIDEO_ID: &str = "dQw4w9WgXcQ";

/// Run the startup JS-runtime self-check (#189): a `--simulate -f bestaudio`
/// fetch through the shared builder (so the bundled deno is on PATH), with the
/// production cookies when present so it gets past the separate bot-check to the
/// n-challenge. Returns the verdict + the read-back deno version.
pub(crate) async fn run_selfcheck(
    ytdlp_path: &Path,
    deno_path: Option<&Path>,
    cookies: Option<&Path>,
) -> (JsRuntimeStatus, Option<String>) {
    let deno_version = match deno_path {
        Some(d) => super::tools::deno_version(d).await,
        None => None,
    };
    let mut cmd = ytdlp_command(ytdlp_path);
    cmd.arg("--simulate")
        .arg("--no-warnings")
        .arg("-f")
        .arg("bestaudio");
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.arg(format!(
        "https://www.youtube.com/watch?v={SELFCHECK_VIDEO_ID}"
    ))
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let (exit_ok, stderr) = match cmd.output().await {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, format!("self-check spawn failed: {e}")),
    };
    let verdict = js_runtime_verdict(deno_version.as_deref(), exit_ok, &stderr);
    (verdict, deno_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn deno_asset_url_points_to_pinned_windows_zip() {
        assert_eq!(
            deno_asset_url("2.9.7"),
            "https://github.com/denoland/deno/releases/download/v2.9.7/\
             deno-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn path_with_tools_prepends_tools_dir() {
        let tools = Path::new("/opt/tools");
        let inherited = std::ffi::OsStr::new("/usr/bin:/bin");
        let got = path_with_tools(tools, Some(inherited));
        // On the Linux CI test host the PATH separator is `:`; the tools dir
        // must come FIRST so the bundled deno wins over anything installed.
        assert_eq!(got.to_str().unwrap(), "/opt/tools:/usr/bin:/bin");
    }

    #[test]
    fn path_with_tools_handles_empty_inherited() {
        let tools = Path::new("/opt/tools");
        let got = path_with_tools(tools, None);
        assert_eq!(got.to_str().unwrap(), "/opt/tools");
    }

    #[test]
    fn js_runtime_args_returns_deno_flag_when_supported() {
        let help = "  --js-runtimes RUNTIME[:PATH]    Additional JavaScript runtime to enable";
        assert_eq!(
            js_runtime_args(help),
            vec!["--js-runtimes".to_string(), "deno".to_string()],
            "when yt-dlp advertises --js-runtimes we must enable deno (not node) — \
             node is not the runtime yt-dlp's n-challenge solver uses"
        );
    }

    #[test]
    fn js_runtime_args_empty_when_flag_unsupported() {
        let help = "  --some-other-flag    An unrelated option";
        assert!(js_runtime_args(help).is_empty());
    }

    #[test]
    fn parse_deno_version_extracts_semver() {
        let stdout = "deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)\n\
                      v8 13.0.0\ntypescript 5.6.0\n";
        assert_eq!(parse_deno_version(stdout).as_deref(), Some("2.9.7"));
    }

    #[test]
    fn parse_deno_version_none_on_garbage() {
        assert_eq!(parse_deno_version("not deno output"), None);
        assert_eq!(parse_deno_version(""), None);
    }

    #[test]
    fn js_runtime_verdict_missing_when_no_deno() {
        assert_eq!(
            js_runtime_verdict(None, false, "n challenge solving failed"),
            JsRuntimeStatus::Missing
        );
    }

    #[test]
    fn js_runtime_verdict_ok_on_clean_simulate() {
        assert_eq!(
            js_runtime_verdict(Some("2.9.7"), true, ""),
            JsRuntimeStatus::Ok("2.9.7".to_string())
        );
    }

    #[test]
    fn js_runtime_verdict_solver_failed_on_nchallenge() {
        assert_eq!(
            js_runtime_verdict(
                Some("2.9.7"),
                false,
                "ERROR: [youtube] xyz: n challenge solving failed"
            ),
            JsRuntimeStatus::SolverFailed("n challenge solving failed".to_string())
        );
    }

    #[test]
    fn js_runtime_verdict_ok_when_failure_is_not_nchallenge() {
        // A cookie bot-check / network failure is the #141 gate, not this
        // ticket's — deno is present, so js_runtime_ok stays true.
        assert_eq!(
            js_runtime_verdict(
                Some("2.9.7"),
                false,
                "ERROR: Sign in to confirm you're not a bot"
            ),
            JsRuntimeStatus::Ok("2.9.7".to_string())
        );
    }
}
