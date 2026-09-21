//! #196: process-start instant, surfaced as `/api/v1/status.uptime_s`.
//!
//! The post-deploy E2E job reads `uptime_s` to SKIP restarting a SongPlayer the
//! Deploy job started less than 10 min ago (item 6 — halve the restarts per
//! push). Marked at the top of `lib::start`, read by the status handler.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Record the process start. Idempotent — only the FIRST call is kept, so the
/// uptime is measured from the earliest `start()` in the process.
///
/// mutants::skip — a `OnceLock` set with no observable return; its effect
/// (`uptime_secs`) is a wall-clock read that no terminating unit test can pin.
#[cfg_attr(test, mutants::skip)]
pub fn mark_started() {
    let _ = START.set(Instant::now());
}

/// Seconds since the process started, or `0` if `mark_started` was never called
/// (e.g. a unit test that never boots the server).
///
/// mutants::skip — a wall-clock elapsed read; only catchable by a wall-time
/// assertion (non-deterministic on the runner). The DECISION it feeds lives in
/// the CI shell (skip restart iff deployed version AND `uptime_s < 600`).
#[cfg_attr(test, mutants::skip)]
pub fn uptime_secs() -> u64 {
    START.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

/// #203: raise SongPlayer to `HIGH_PRIORITY_CLASS` at startup so the NDI SDK's
/// own compression threads pre-empt the contained heavy children (stems / lyrics
/// / dub), which run `BELOW_NORMAL` under a Job Object CPU cap + affinity. NOT
/// `REALTIME` — that starves the OS input/paging threads. Best-effort + logged;
/// a no-op off Windows.
///
/// mutants::skip — a one-shot OS scheduling-class call with no in-process oracle
/// (only the kernel scheduler can observe it); the box read verifies it live.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn set_high_priority_class() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
    };
    // SAFETY: GetCurrentProcess returns the current-process pseudo-handle;
    // SetPriorityClass on it only changes this process's scheduling class.
    let ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) };
    if ok == 0 {
        tracing::warn!("could not set SongPlayer HIGH_PRIORITY_CLASS (#203)");
    } else {
        tracing::info!("SongPlayer priority class set to HIGH (#203)");
    }
}

/// Non-Windows: no priority class to set (the box is Windows-only prod).
#[cfg(not(windows))]
pub fn set_high_priority_class() {}

/// The priority-class label for `/api/v1/status.heavy_containment.priority_class`:
/// `"high"` where [`set_high_priority_class`] applies (Windows), else `"default"`.
///
/// mutants::skip — a cfg-derived platform string with no cross-platform value
/// assertion possible (asserting `"high"` fails on the Linux job, `"default"` on
/// the Windows job — the #189 platform-string trap).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn priority_class_label() -> &'static str {
    "high"
}

/// Non-Windows twin — SongPlayer keeps the default class off the box.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
pub fn priority_class_label() -> &'static str {
    "default"
}
