//! WHY the lyrics venv is (not) ready, and what `bootstrap::ensure_ready`
//! does about it (#221 BLOCKER, comment 5877957738, main-session decisions 1
//! and 2).
//!
//! Before: `is_ready` answered a bare bool, so a 45 s timeout, a failed CUDA
//! init and a missing package all read "not ready", and ANY "not ready" ran
//! the full reinstall (qwen-asr, `audio-separator[gpu]`, the cu124 torch
//! `--force-reinstall`, the numpy repair). From 28.9 every restart after a
//! deploy did that (~6 min of pip on the live box) and raced the post-deploy
//! A/V gate, which shared the venv's numpy.
//!
//! - `bootstrap::is_ready` answers a [`Readiness`]: `Ready`, `Missing` (no
//!   interpreter), `Timeout`, or `Failed { code, stderr_tail }`.
//! - [`Readiness::action`]: `Ready` → the fast path; a PROVEN import failure
//!   (the probe's last stderr line is a `ModuleNotFoundError` /
//!   `ImportError`, [`import_failure`]) or a missing interpreter → install; a
//!   timeout or any other failure (CUDA not available: the probe exits 1 with
//!   no traceback; a driver / init error; an `OSError` loading torch's DLLs
//!   under memory pressure) → retry.
//! - [`decide`] probes and retries with backoff ([`RETRY_PLAN`]: 5 s,
//!   doubling to 60 s, for about 3 minutes) BEFORE any install. After the
//!   budget, a probe that still times out uses the venv AS IT IS (a timeout
//!   never triggers the torch force-reinstall); one that still fails without
//!   an import error installs (the CUDA-torch repair of a CPU-only torch).
//! - Every probe is logged with its reason (INFO when ready, WARN otherwise),
//!   so the next slow or failed cold probe is diagnosable from the log.

use std::future::Future;
use std::time::Duration;

use tracing::{info, warn};

/// How long one probe may take. A cold CUDA init alone can take 10–20 s.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// How much of a failed probe's stderr is kept: its END, where a traceback
/// names its exception.
pub const STDERR_TAIL_CHARS: usize = 600;

/// What the venv probe (`bootstrap::IS_READY_PROBE`) answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Every package imports and CUDA is available.
    Ready,
    /// There is no venv interpreter (the venv was never created, or lost it).
    Missing,
    /// The probe did not finish within [`PROBE_TIMEOUT`] (it is killed).
    Timeout,
    /// The probe exited non-zero (`code`), or could not run (`code: None`,
    /// the error as the tail). `stderr_tail`: the last [`STDERR_TAIL_CHARS`]
    /// characters of its stderr.
    Failed {
        code: Option<i32>,
        stderr_tail: String,
    },
}

/// What to do after one probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    /// The venv is ready: no install.
    FastPath,
    /// The venv needs the install path.
    Install,
    /// Probe again after a pause (the answer may be transient).
    Retry,
}

impl Readiness {
    /// A failed probe from its exit code and its whole stderr (the tail is
    /// kept).
    pub fn failed(code: Option<i32>, stderr: &str) -> Self {
        Self::Failed {
            code,
            stderr_tail: stderr_tail(stderr, STDERR_TAIL_CHARS),
        }
    }

    /// The probe passed.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The probe proved a required package missing or unimportable.
    pub fn is_import_failure(&self) -> bool {
        match self {
            Self::Failed { stderr_tail, .. } => import_failure(stderr_tail),
            _ => false,
        }
    }

    /// What `decide` does after this answer (the module doc).
    pub fn action(&self) -> ProbeAction {
        match self {
            Self::Ready => ProbeAction::FastPath,
            Self::Missing => ProbeAction::Install,
            _ if self.is_import_failure() => ProbeAction::Install,
            Self::Timeout | Self::Failed { .. } => ProbeAction::Retry,
        }
    }
}

/// Whether a Python stderr ends in an import failure: its last non-empty
/// line starts with `ModuleNotFoundError` or `ImportError` — the uncaught
/// exception an `import` raised (numba's "needs NumPy 2.4 or less" is an
/// `ImportError` too, the #144 repair case). A warning printed before the
/// probe's `sys.exit(1)` (CUDA not available) is never one.
pub fn import_failure(stderr: &str) -> bool {
    let last = stderr.lines().map(str::trim).rfind(|line| !line.is_empty());
    last.is_some_and(|line| {
        line.starts_with("ModuleNotFoundError") || line.starts_with("ImportError")
    })
}

/// The last `max` characters of `stderr`, trimmed (never splits a character).
pub fn stderr_tail(stderr: &str, max: usize) -> String {
    let trimmed = stderr.trim();
    let skip = trimmed.chars().count().saturating_sub(max);
    trimmed.chars().skip(skip).collect()
}

/// How long `decide` keeps retrying before it acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPlan {
    /// The pause before the first retry; doubled after each retry.
    pub first_delay: Duration,
    /// The longest pause.
    pub max_delay: Duration,
    /// No retry starts after this much time since the first probe.
    pub budget: Duration,
}

/// The production plan: probes at 0, 5, 15, 35, 75 and 135 s when each
/// answers at once (a probe that times out takes its 45 s on top), about 3
/// minutes in all.
pub const RETRY_PLAN: RetryPlan = RetryPlan {
    first_delay: Duration::from_secs(5),
    max_delay: Duration::from_secs(60),
    budget: Duration::from_secs(180),
};

/// What `ensure_ready` does with the venv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastPath {
    /// The probe passed: use the venv, no install.
    Ready,
    /// Run the install path: a proven import failure, a missing interpreter,
    /// or a failure that outlived the retries.
    Install,
    /// The probe kept timing out: use the venv as it is (a timeout never
    /// triggers the torch force-reinstall).
    UseAsIs,
}

/// `decide`'s verdict and how many probes it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub path: FastPath,
    pub probes: u32,
}

/// Probe until the venv is ready, a probe proves an install is needed, or
/// `plan`'s budget runs out (the module doc). `probe` runs one probe.
pub async fn decide<P, F>(mut probe: P, plan: RetryPlan) -> Decision
where
    P: FnMut() -> F,
    F: Future<Output = Readiness>,
{
    let start = tokio::time::Instant::now();
    let mut delay = plan.first_delay;
    let mut probes = 0;
    loop {
        let outcome = probe().await;
        probes += 1;
        log_probe(&outcome, probes);
        match outcome.action() {
            ProbeAction::FastPath => {
                return Decision {
                    path: FastPath::Ready,
                    probes,
                };
            }
            ProbeAction::Install => {
                return Decision {
                    path: FastPath::Install,
                    probes,
                };
            }
            ProbeAction::Retry => {}
        }
        if start.elapsed() + delay > plan.budget {
            let path = if outcome == Readiness::Timeout {
                FastPath::UseAsIs
            } else {
                FastPath::Install
            };
            return Decision { path, probes };
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(plan.max_delay);
    }
}

/// One probe's log line. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_probe(outcome: &Readiness, probe: u32) {
    match outcome {
        Readiness::Ready => info!(probe, "lyrics bootstrap: the venv probe passed"),
        Readiness::Missing => info!(probe, "lyrics bootstrap: no venv interpreter yet"),
        Readiness::Timeout => warn!(
            probe,
            timeout_s = PROBE_TIMEOUT.as_secs(),
            "lyrics bootstrap: the venv probe timed out"
        ),
        Readiness::Failed { code, stderr_tail } => warn!(
            probe,
            ?code,
            import_failure = outcome.is_import_failure(),
            stderr_tail = %stderr_tail,
            "lyrics bootstrap: the venv probe failed"
        ),
    }
}

#[cfg(test)]
#[path = "bootstrap_probe_tests.rs"]
mod tests;
