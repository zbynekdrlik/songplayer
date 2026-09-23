//! Unit tests for the #161 mid-job wall-abort core (`idle_gate_abort.rs`).
//!
//! RED-first: the pure `AbortPolicy` and the `run_with_wall_abort` orchestrator
//! assert the intended abort behaviour against the stubbed module — the RED
//! commit ships `ABORT_CONSECUTIVE_BUSY = u32::MAX`, so nothing ever aborts,
//! and the two "must abort" tests FAIL until the GREEN commit sets the real
//! threshold (2). Timed tests run under `start_paused` so the 1 s poll and the
//! mock heavy future advance deterministically without real waiting.

use super::*;
use crate::lyrics::idle_gate::WallActivity;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn busy() -> WallActivity {
    WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    }
}

fn idle() -> WallActivity {
    WallActivity::default()
}

// ---- AbortPolicy (pure decision core) -------------------------------------

#[test]
fn abort_policy_single_busy_does_not_abort() {
    let mut p = AbortPolicy::default();
    assert!(
        !p.observe(true, true),
        "one busy sample must not abort (2 s debounce)"
    );
}

#[test]
fn abort_policy_two_consecutive_busy_aborts() {
    let mut p = AbortPolicy::default();
    assert!(!p.observe(true, true), "first busy → not yet");
    assert!(p.observe(true, true), "second consecutive busy → abort");
}

#[test]
fn abort_policy_busy_idle_busy_does_not_abort() {
    let mut p = AbortPolicy::default();
    assert!(!p.observe(true, true), "busy 1");
    assert!(!p.observe(true, false), "idle resets the streak");
    assert!(
        !p.observe(true, true),
        "busy again → only 1 in a row, no abort"
    );
}

#[test]
fn abort_policy_disabled_gate_never_aborts() {
    let mut p = AbortPolicy::default();
    for _ in 0..5 {
        assert!(
            !p.observe(false, true),
            "gate OFF never aborts even while busy"
        );
    }
}

// ---- run_with_wall_abort (orchestration) ----------------------------------

#[tokio::test(start_paused = true)]
async fn run_with_wall_abort_ok_when_future_completes() {
    let out = run_with_wall_abort(async { 7u32 }, true, || async { idle() }).await;
    assert_eq!(out, Ok(7));
}

#[tokio::test(start_paused = true)]
async fn run_with_wall_abort_aborts_and_drops_future_on_sustained_busy() {
    let completed = Arc::new(AtomicBool::new(false));
    let c = completed.clone();
    // A "heavy" future: it only sets `completed` if it runs to the end. On abort
    // the future is DROPPED before the sleep elapses, so `completed` stays false
    // — proving the child future was killed rather than awaited to completion.
    let heavy = async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        c.store(true, Ordering::SeqCst);
        42u32
    };
    let result = run_with_wall_abort(heavy, true, || async { busy() }).await;
    assert!(
        matches!(result, Err(WallAbort { .. })),
        "sustained busy must abort the running step: {result:?}"
    );
    assert!(
        !completed.load(Ordering::SeqCst),
        "aborted future must be dropped, never awaited to completion"
    );
    if let Err(WallAbort { detail }) = result {
        assert!(
            detail.contains("playing"),
            "detail names the cause: {detail}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn run_with_wall_abort_disabled_gate_lets_future_finish() {
    let completed = Arc::new(AtomicBool::new(false));
    let c = completed.clone();
    let heavy = async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        c.store(true, Ordering::SeqCst);
        9u32
    };
    // Gate OFF: even a permanently-busy wall must let the heavy step finish.
    let result = run_with_wall_abort(heavy, false, || async { busy() }).await;
    assert_eq!(result, Ok(9));
    assert!(completed.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn run_with_wall_abort_scripted_busy_idle_busy_does_not_abort() {
    // Wall reads busy, idle, busy, then idle forever — never two busy in a row,
    // so the run must finish (Ok), proving the debounce holds at the
    // orchestration level too.
    let seq = Arc::new(Mutex::new(vec![busy(), idle(), busy()]));
    let idx = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicBool::new(false));
    let c = completed.clone();
    let heavy = async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        c.store(true, Ordering::SeqCst);
        1u32
    };
    let wall = move || {
        let seq = seq.clone();
        let idx = idx.clone();
        async move {
            let i = idx.fetch_add(1, Ordering::SeqCst);
            let g = seq.lock().unwrap();
            g.get(i).copied().unwrap_or_default()
        }
    };
    let result = run_with_wall_abort(heavy, true, wall).await;
    assert_eq!(result, Ok(1));
    assert!(completed.load(Ordering::SeqCst));
}

// ---- isolation_input — #144 stems-fed ★ vocals decision -------------------

#[test]
fn isolation_input_done_with_vocals_uses_the_stems_sidecar() {
    let p = std::path::Path::new("/cache/foo_audio_vocals.flac");
    assert_eq!(
        isolation_input(Some("done"), p, true),
        IsolationInput::Stems(p.to_path_buf()),
        "stems done + vocals on disk → feed the sidecar into dereverb+resample"
    );
}

#[test]
fn isolation_input_done_but_vocals_missing_waits() {
    let p = std::path::Path::new("/cache/foo_audio_vocals.flac");
    assert_eq!(
        isolation_input(Some("done"), p, false),
        IsolationInput::WaitForStems,
        "status done but the vocals file is not (yet) on disk → wait, never isolate"
    );
}

#[test]
fn isolation_input_pending_or_failed_waits_for_stems() {
    let p = std::path::Path::new("/cache/foo_audio_vocals.flac");
    // NULL/pending, and a retryable 'failed' the stems worker will re-run.
    assert_eq!(
        isolation_input(None, p, false),
        IsolationInput::WaitForStems
    );
    assert_eq!(
        isolation_input(Some("failed"), p, false),
        IsolationInput::WaitForStems
    );
    // Even if a vocals file somehow exists while the status is not 'done', the
    // step waits for the worker's terminal 'done' before trusting the sidecar.
    assert_eq!(isolation_input(None, p, true), IsolationInput::WaitForStems);
}

#[test]
fn isolation_input_unsupported_is_base_tier_only() {
    let p = std::path::Path::new("/cache/foo_audio_vocals.flac");
    assert_eq!(
        isolation_input(Some("unsupported"), p, false),
        IsolationInput::BaseTierOnly,
        "terminal 'unsupported' → no isolation path exists; take the base tier"
    );
    // Terminal even if a stray vocals file exists on disk.
    assert_eq!(
        isolation_input(Some("unsupported"), p, true),
        IsolationInput::BaseTierOnly
    );
}

// ---- isolation_step_timeout — the CPU ×4 scaling reaches the spawn seam ----

#[test]
fn isolation_step_timeout_scales_only_the_cpu_plan() {
    use crate::lyrics::heavy_plan::HeavyStepPlan;
    // 10.5-min song → base clamps to 1280 s (isolation_timeout); the exact base
    // is irrelevant here — we assert the plan-scaling relative to it.
    let dur = Some(640_000);
    let base = crate::lyrics::aligner::isolation_timeout(dur);
    assert_eq!(
        isolation_step_timeout(&HeavyStepPlan::gpu_below_normal(), dur),
        base,
        "a GPU isolation keeps the base ceiling"
    );
    assert_eq!(
        isolation_step_timeout(&HeavyStepPlan::cpu_idle(), dur),
        base * 4,
        "a CPU isolation gets ×4 the base so it is not killed mid-run"
    );
}
