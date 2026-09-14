//! Build script: embed the short git sha as `SP_GIT_SHA` so the panic hook's
//! startup line and crash records can name the exact build (#156). Fails safe
//! to "unknown" when git is unavailable (source tarball, no VCS) — the value
//! is diagnostic only and must never break the build.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SP_GIT_SHA={sha}");
    // Best-effort: re-run when HEAD moves so the embedded sha stays current.
    // A missing path (e.g. a git worktree's gitdir file, or a tarball) simply
    // means cargo re-runs this script on every build, which is cheap and safe.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
