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
    // Only while that file exists: a rerun-if-changed path that is missing
    // (cargo-mutants' copy of the tree has no `.git`, nor does a worktree's
    // gitdir file or a tarball) makes cargo re-run this script on EVERY
    // build, and a re-run script rebuilds sp-server — each mutant's test
    // phase recompiled it (~100 s of its 300 s bound, #228: CI run
    // 38011003676 shard 27 timed out before its killing tests ran). Without
    // the file the sha is "unknown" and stays so: watch this script alone.
    if std::path::Path::new("../../.git/HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    } else {
        println!("cargo:rerun-if-changed=build.rs");
    }
}
