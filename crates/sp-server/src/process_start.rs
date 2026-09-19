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
