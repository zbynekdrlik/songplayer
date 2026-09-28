//! #221 BLOCKER: the lyrics-venv probe's reason and the fast-path decision.
//! `decide` runs on a paused clock with a scripted probe, so the retry
//! schedule is exact and no test waits for real.
//! Wired via `#[cfg(test)] #[path = "bootstrap_probe_tests.rs"] mod tests;`.

use std::future::{Ready, ready};
use std::time::Duration;

use tokio::time::Instant;

use super::*;

/// A probe that answers `outcomes` in order (and panics when asked again).
fn scripted(outcomes: Vec<Readiness>) -> impl FnMut() -> Ready<Readiness> {
    let mut outcomes = outcomes.into_iter();
    move || ready(outcomes.next().expect("probed more often than scripted"))
}

/// A probe that always answers `outcome`.
fn always(outcome: Readiness) -> impl FnMut() -> Ready<Readiness> {
    move || ready(outcome.clone())
}

/// The probe's exit when `torch.cuda.is_available()` is false: 1, and no
/// traceback (at most a warning).
fn cuda_unavailable() -> Readiness {
    Readiness::failed(
        Some(1),
        "C:\\venv\\torch\\cuda\\__init__.py:129: UserWarning: CUDA initialization: CUDA driver initialization failed\n  return torch._C._cuda_getDeviceCount() > 0\n",
    )
}

/// An import failure of a required package (a Python traceback, CRLF).
fn qwen_asr_missing() -> Readiness {
    Readiness::failed(
        Some(1),
        "Traceback (most recent call last):\r\n  File \"<string>\", line 1, in <module>\r\nModuleNotFoundError: No module named 'qwen_asr'\r\n",
    )
}

/// torch's native library failing to load (Windows, under memory pressure).
fn dll_load_failed() -> Readiness {
    Readiness::failed(
        Some(1),
        "Traceback (most recent call last):\r\n  File \"<string>\", line 1, in <module>\r\nImportError: DLL load failed while importing _C: The paging file is too small for this operation to complete.\r\n",
    )
}

/// Run `decide` with `plan` and return its verdict and how long it took.
async fn run(probe: impl FnMut() -> Ready<Readiness>, plan: RetryPlan) -> (Decision, Duration) {
    let start = Instant::now();
    let decision = decide(probe, plan).await;
    (decision, start.elapsed())
}

fn decision(path: FastPath, probes: u32) -> Decision {
    Decision { path, probes }
}

// ---- the decision ------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_probe_that_times_out_once_then_passes_installs_nothing() {
    let probe = scripted(vec![Readiness::Timeout, Readiness::Ready]);
    let (verdict, took) = run(probe, RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Ready, 2));
    assert_eq!(
        took,
        Duration::from_secs(5),
        "one retry after the first pause"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cuda_failure_is_retried_before_any_install() {
    let probe = scripted(vec![
        cuda_unavailable(),
        cuda_unavailable(),
        Readiness::Ready,
    ]);
    let (verdict, took) = run(probe, RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Ready, 3));
    assert_eq!(took, Duration::from_secs(15), "pauses of 5 s, then 10 s");
}

#[tokio::test(start_paused = true)]
async fn a_probe_with_an_import_error_installs_at_once() {
    let (verdict, took) = run(scripted(vec![qwen_asr_missing()]), RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 1));
    assert_eq!(took, Duration::ZERO);
    // numba refusing a too-new numpy is an ImportError too (the #144 repair).
    let numba = Readiness::failed(
        Some(1),
        "ImportError: Numba needs NumPy 2.4 or less. Got NumPy 2.5.\n",
    );
    let (verdict, _) = run(scripted(vec![numba]), RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 1));
}

#[tokio::test(start_paused = true)]
async fn a_missing_interpreter_installs_at_once() {
    let (verdict, took) = run(scripted(vec![Readiness::Missing]), RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 1));
    assert_eq!(took, Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_timeout_then_an_import_error_installs() {
    let probe = scripted(vec![Readiness::Timeout, qwen_asr_missing()]);
    let (verdict, took) = run(probe, RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 2));
    assert_eq!(took, Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_keeps_timing_out_never_installs() {
    let (verdict, took) = run(always(Readiness::Timeout), RETRY_PLAN).await;
    // Probes at 0, 5, 15, 35, 75 and 135 s; the next pause (60 s) would
    // start after the 180 s budget.
    assert_eq!(verdict, decision(FastPath::UseAsIs, 6));
    assert_eq!(took, Duration::from_secs(135));
}

/// Review round 2: a DLL-load failure right after a restart is retried; one
/// that passes installs nothing, one that persists installs after the budget.
#[tokio::test(start_paused = true)]
async fn a_dll_load_failure_is_retried_before_any_install() {
    let probe = scripted(vec![dll_load_failed(), Readiness::Ready]);
    let (verdict, took) = run(probe, RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Ready, 2));
    assert_eq!(took, Duration::from_secs(5));
    let (verdict, took) = run(always(dll_load_failed()), RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 6));
    assert_eq!(took, Duration::from_secs(135));
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_outlives_the_retries_installs() {
    let (verdict, took) = run(always(cuda_unavailable()), RETRY_PLAN).await;
    assert_eq!(verdict, decision(FastPath::Install, 6));
    assert_eq!(took, Duration::from_secs(135));
}

#[tokio::test(start_paused = true)]
async fn a_retry_that_ends_exactly_on_the_budget_still_runs() {
    let plan = |budget_s: u64| RetryPlan {
        budget: Duration::from_secs(budget_s),
        ..RETRY_PLAN
    };
    // 75 s + the 60 s pause = 135 s: exactly the budget, so it still probes.
    let (verdict, took) = run(always(Readiness::Timeout), plan(135)).await;
    assert_eq!(verdict, decision(FastPath::UseAsIs, 6));
    assert_eq!(took, Duration::from_secs(135));
    // One second less: that pause would end after the budget.
    let (verdict, took) = run(always(Readiness::Timeout), plan(134)).await;
    assert_eq!(verdict, decision(FastPath::UseAsIs, 5));
    assert_eq!(took, Duration::from_secs(75));
}

/// Review round 1: the loop's end is structural — at most 12 probes
/// (`MAX_PROBES`) whatever the budget, so no flipped comparison can make
/// `decide` spin (a hang fails the mutation gate like a survivor).
#[tokio::test(start_paused = true)]
async fn a_probe_is_never_repeated_more_than_twelve_times() {
    let a_day = RetryPlan {
        budget: Duration::from_secs(86_400),
        ..RETRY_PLAN
    };
    let (verdict, took) = run(always(Readiness::Timeout), a_day).await;
    // Pauses of 5, 10, 20, 40, then 60 s: probes at 0 … 75 s, then every
    // 60 s up to the 12th at 495 s.
    assert_eq!(verdict, decision(FastPath::UseAsIs, 12));
    assert_eq!(took, Duration::from_secs(495));
    let (verdict, _) = run(always(cuda_unavailable()), a_day).await;
    assert_eq!(verdict, decision(FastPath::Install, 12));
}

#[test]
fn the_production_plan_retries_for_about_three_minutes() {
    assert_eq!(
        RETRY_PLAN,
        RetryPlan {
            first_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(60),
            budget: Duration::from_secs(180),
        }
    );
    assert_eq!(PROBE_TIMEOUT, Duration::from_secs(45));
}

// ---- the reason --------------------------------------------------------------

#[test]
fn every_answer_maps_to_its_action() {
    assert_eq!(Readiness::Ready.action(), ProbeAction::FastPath);
    assert_eq!(Readiness::Missing.action(), ProbeAction::Install);
    assert_eq!(qwen_asr_missing().action(), ProbeAction::Install);
    assert_eq!(Readiness::Timeout.action(), ProbeAction::Retry);
    assert_eq!(cuda_unavailable().action(), ProbeAction::Retry);
    // A spawn error (no exit code) is retried too.
    assert_eq!(
        Readiness::failed(None, "Access is denied. (os error 5)").action(),
        ProbeAction::Retry
    );
    // Review round 2: so is a DLL that failed to load.
    assert_eq!(dll_load_failed().action(), ProbeAction::Retry);
    assert!(Readiness::Ready.is_ready());
    for other in [Readiness::Missing, Readiness::Timeout, cuda_unavailable()] {
        assert!(!other.is_ready(), "{other:?}");
    }
}

#[test]
fn an_import_failure_is_the_last_line_of_the_traceback() {
    assert!(import_failure(
        "Traceback (most recent call last):\n  File \"<string>\", line 1\nModuleNotFoundError: No module named 'audio_separator'\n"
    ));
    assert!(import_failure(
        "ImportError: Numba needs NumPy 2.4 or less. Got NumPy 2.5.\r\n\r\n"
    ));
    // Review round 2: a DLL that fails to LOAD is not a missing package. On
    // Windows it is the typical transient failure under memory pressure (the
    // paging file, WinError 1455) — the blocker's own startup case — so it is
    // retried, and installs only if it outlives the retries.
    assert!(!import_failure(
        "Traceback (most recent call last):\r\nImportError: DLL load failed while importing _C: The paging file is too small for this operation to complete.\r\n"
    ));
    // CUDA not available: exit 1 with no traceback, maybe a warning.
    assert!(!import_failure(""));
    assert!(!import_failure(
        "UserWarning: CUDA initialization: CUDA driver initialization failed"
    ));
    // An import error mentioned by an earlier line is not the failure.
    assert!(!import_failure(
        "ImportError: optional speedup not found, using the slow path\nOSError: [WinError 1455] The paging file is too small\n"
    ));
    assert!(!import_failure(
        "  warnings.warn(\"ImportError is not fatal\")\n"
    ));
    assert!(!Readiness::Timeout.is_import_failure());
    assert!(!Readiness::Missing.is_import_failure());
    assert!(qwen_asr_missing().is_import_failure());
}

#[test]
fn a_failed_probe_keeps_the_end_of_its_stderr() {
    assert_eq!(
        Readiness::failed(Some(1), "  line one\nModuleNotFoundError: x\n\n"),
        Readiness::Failed {
            code: Some(1),
            stderr_tail: "line one\nModuleNotFoundError: x".to_string(),
        }
    );
    // A long stderr keeps its last STDERR_TAIL_CHARS characters, the
    // exception included.
    let long = format!(
        "{}\nModuleNotFoundError: No module named 'numba'",
        "x".repeat(2_000)
    );
    let Readiness::Failed {
        stderr_tail: tail, ..
    } = Readiness::failed(Some(1), &long)
    else {
        panic!("a failure");
    };
    assert_eq!(tail.chars().count(), STDERR_TAIL_CHARS);
    assert!(tail.ends_with("No module named 'numba'"));
    assert!(import_failure(&tail));
    // Characters, never bytes: a multi-byte character is never split.
    assert_eq!(stderr_tail("čšžáé", 3), "žáé");
    assert_eq!(stderr_tail("abc", 10), "abc");
    assert_eq!(stderr_tail("abc", 0), "");
}
