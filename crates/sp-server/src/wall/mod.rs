//! #259 (SNV, 13.10.2026): the LED wall report, lane 1 — read and report.
//!
//! **The owner's rule (ROZHODNUTÉ 13.10.2026, #259 comment 6142937258):**
//! our code never enforces a fixed display or Resolume configuration.
//! Whatever the operator sets up — cabling, NVIDIA Surround, Windows'
//! layout, Arena's outputs — is the truth, and it may change at any time; a
//! deploy to another box (PP) changes nothing on its displays. So this
//! module keeps no layout of its own and acts on nothing: it compares the
//! box with Resolume Arena's OWN saved output setup and says what it sees.
//!
//! Why: SNV's wall is three screens the GPU joins into one NVIDIA Surround
//! span that Windows shows as one display, and Arena keeps its output
//! window on it. A screen switched off drops its hot-plug: the GeForce card
//! dissolves the span and does not form it again by itself, and Arena's
//! output window closes and does not reopen. The operator found that, not
//! SongPlayer.
//!
//! The report runs at once and every [`CHECK_EVERY`] and publishes the
//! wall's state ([`global`], `GET /api/v1/wall`, the health bar's "Stena"
//! segment; the post-deploy gate fails only when SongPlayer cannot read):
//!
//! - the facts, read through a [`read::WallReader`] — Arena's process and
//!   windows, its saved output setup (`resolume.rs`), Windows' displays,
//!   and NVAPI from `System32\nvapi64.dll` loaded at run time (`win.rs`,
//!   Windows only) only when a display Arena wants is gone or has another
//!   size — behind the bounded reader (`prod_env::Reader`, [`READ_BOUND`]):
//!   a hung driver never freezes the report;
//! - the pure decision ([`decide`]).
//!
//! One WARN when the state turns into a problem, one INFO when it holds
//! again ([`Reporter`]). The vocabulary (states, codes, Slovak texts, the
//! wire shape) is `sp_core::wall`.
//!
//! #254 (the owner's ONE exception to "acts on nothing", ROZHODNUTÉ
//! 14.10.2026, #254 comment 6169316862): the guard ([`guard`]) ends a
//! crashed or hung Arena at each check, so the operator's relaunch script
//! brings it back. Its only act on the box lives in `guard_win.rs`; it never
//! touches Arena's setup, a display or a clip.

use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use sp_core::wall::{WallGuard, WallProblem, WallProblemView, WallState, WallStatus};
use tracing::{error, info, warn};

use crate::prod_env::Reader;

pub mod decide;
pub mod guard;
pub mod nvapi;
pub mod read;
pub mod resolume;

pub use read::WallFacts;

/// How often the wall is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(5);

/// How long a read of the box may take before it counts as unreadable.
pub const READ_BOUND: Duration = Duration::from_secs(10);

/// The read's name in its errors ("the wall read did not finish within …").
pub const READ_WHAT: &str = "wall read";

/// Each problem as the status carries it.
fn views(problems: &[WallProblem]) -> Vec<WallProblemView> {
    problems.iter().map(WallProblem::view).collect()
}

/// The published status of one check's facts with the guard's record, and
/// its problems.
pub fn status_of(
    facts: &WallFacts,
    guard: WallGuard,
    now_ms: i64,
) -> (WallStatus, Vec<WallProblem>) {
    let verdict = decide::decide(facts);
    let status = WallStatus {
        checked: true,
        state: verdict.state,
        problems: views(&verdict.problems),
        read: decide::read_view(facts),
        read_errors: decide::read_errors(facts),
        checked_at_ms: Some(now_ms),
        guard,
        // #275: the Resolume driver's fact, put on by the route
        // (`api/routes_status.rs::wall`), never a part of the wall's verdict.
        songplayer_clip: Default::default(),
    };
    (status, verdict.problems)
}

/// What a check logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStep {
    /// The same state and problems as the last check: nothing.
    Quiet,
    /// The first check, and the wall holds: one INFO.
    Holds,
    /// A problem appeared or changed: one WARN naming every problem.
    Problem,
    /// The wall holds again after a problem: one INFO.
    Cleared,
    /// From one holding state to another (the screens switched off or on,
    /// Arena started or quit): one INFO.
    Changed,
}

/// Remembers the last check's state and problems, so each change is logged
/// once.
#[derive(Debug, Default)]
pub struct Reporter {
    last: Option<(WallState, Vec<WallProblem>)>,
}

impl Reporter {
    /// What this check logs, given the last one.
    pub fn step(&mut self, state: WallState, problems: &[WallProblem]) -> LogStep {
        let Some((last_state, last_problems)) = self.last.replace((state, problems.to_vec()))
        else {
            return if state.holds() {
                LogStep::Holds
            } else {
                LogStep::Problem
            };
        };
        if last_state == state && last_problems == problems {
            LogStep::Quiet
        } else if !state.holds() {
            LogStep::Problem
        } else if !last_state.holds() {
            LogStep::Cleared
        } else {
            LogStep::Changed
        }
    }
}

/// The published status (one per process: [`global`]; a test makes its own).
#[derive(Debug, Default)]
pub struct WallStore(RwLock<WallStatus>);

impl WallStore {
    /// The status now (a copy: no lock is held after the call).
    pub fn current(&self) -> WallStatus {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the status.
    pub fn publish(&self, status: WallStatus) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = status;
    }
}

/// The process's status, read by `GET /api/v1/wall`: not checked until
/// [`run`]'s first check.
pub fn global() -> &'static WallStore {
    static STORE: LazyLock<WallStore> = LazyLock::new(WallStore::default);
    &STORE
}

/// One check: publishes its status (with the guard's record) into `store`
/// and logs what changed.
pub fn apply(
    store: &WallStore,
    reporter: &mut Reporter,
    guard: WallGuard,
    facts: &WallFacts,
    now_ms: i64,
) -> LogStep {
    let (status, problems) = status_of(facts, guard, now_ms);
    let step = reporter.step(status.state, &problems);
    log_step(step, &status);
    store.publish(status);
    step
}

/// The log line of a step.
#[cfg_attr(test, mutants::skip)] // log text only; the step is pinned by `Reporter`'s tests
fn log_step(step: LogStep, status: &WallStatus) {
    let state = status.state.name();
    let codes: Vec<&str> = status.problems.iter().map(|p| p.code.as_str()).collect();
    let texts: Vec<&str> = status.problems.iter().map(|p| p.text_sk.as_str()).collect();
    match step {
        LogStep::Quiet => {}
        LogStep::Holds => info!(state, "wall: first check ({state})"),
        LogStep::Problem => warn!(
            state,
            problems = %codes.join(", "),
            read_errors = ?status.read_errors,
            "wall: {state} - {}",
            texts.join("; ")
        ),
        LogStep::Cleared => info!(state, "wall: holds again ({state})"),
        LogStep::Changed => info!(state, "wall: now {state}"),
    }
}

/// #254: one check's guard — its step over the facts (an end it orders
/// runs through `end`), each event logged — and its record for the status.
pub fn guard_check(
    guard: &mut guard::Guard,
    facts: &WallFacts,
    now_ms: i64,
    end: &mut dyn FnMut(&guard::EndOrder) -> Result<(), String>,
) -> WallGuard {
    for event in guard::step(guard, &facts.guard, now_ms, end) {
        log_guard(&event);
    }
    guard.view()
}

/// The log line of a guard's event.
#[cfg_attr(test, mutants::skip)] // log text only; the events are pinned by `guard`'s tests
fn log_guard(event: &guard::GuardEvent) {
    use sp_core::wall::GuardWhy;
    match event {
        guard::GuardEvent::Ended(end) => error!(
            pid = end.pid,
            why = end.why.name(),
            monitors = ?end.monitors,
            failed = ?end.error,
            "wall guard: {} Resolume Arena (pid {}): {}",
            if end.error.is_none() { "ended" } else { "could not end" },
            end.pid,
            match end.why {
                GuardWhy::CrashReport => "its crash reporter waited on its \"Crash Report\" window",
                GuardWhy::Hung => "its windows did not answer and its CPU time stood still for 30 s",
            }
        ),
        guard::GuardEvent::NotBack(end) => warn!(
            pid = end.pid,
            why = end.why.name(),
            "wall guard: no new Resolume Arena runs {} s after the guard ended pid {}",
            guard::RELAUNCH_WAIT_MS / 1000,
            end.pid
        ),
        guard::GuardEvent::Back { pid } => {
            info!(pid, "wall guard: Resolume Arena runs again (pid {pid})")
        }
    }
}

/// This check's facts, read within `bound` (a read past it reads as
/// unreadable, and the next check waits for that same read:
/// `prod_env::Reader`).
pub async fn next_check(reader: &mut Reader<WallFacts>, bound: Duration) -> WallFacts {
    match reader.next(bound).await {
        Ok(facts) => facts,
        Err(why) => WallFacts::unread(&why),
    }
}

/// The box's facts now (Windows: `win.rs`).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // platform wiring; the walk is tested over `fake::FakeReader`
fn read_now() -> WallFacts {
    read::read_facts(&win::WinReader)
}

/// Off Windows nothing can be read.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // platform wiring
fn read_now() -> WallFacts {
    WallFacts::unread("not Windows: the wall is read on Windows only")
}

/// #254: the guard's end of an Arena (Windows: `guard_win.rs`).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // platform wiring; the order is tested over a recorder
fn end_now(order: &guard::EndOrder) -> Result<(), String> {
    guard_win::end(order)
}

/// Off Windows nothing is read, so nothing is ever ordered to end.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // platform wiring
fn end_now(_order: &guard::EndOrder) -> Result<(), String> {
    Err("not Windows: nothing is ended".to_string())
}

/// Checks the wall at once and every [`CHECK_EVERY`], publishing into
/// [`global`], until the shutdown.
#[cfg_attr(test, mutants::skip)] // the loop; `next_check`, `apply` and the reader are tested
pub async fn run(mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let mut reader = Reader::named(read_now, READ_WHAT);
    let mut reporter = Reporter::default();
    let mut guard = guard::Guard::default();
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let facts = next_check(&mut reader, READ_BOUND).await;
                let now = chrono::Utc::now().timestamp_millis();
                let record = guard_check(&mut guard, &facts, now, &mut end_now);
                apply(global(), &mut reporter, record, &facts, now);
            }
            _ = shutdown.recv() => break,
        }
    }
}

#[cfg(windows)]
#[path = "win.rs"]
mod win;

#[cfg(windows)]
#[path = "guard_win.rs"]
mod guard_win;

#[cfg(test)]
#[path = "read_fake.rs"]
pub mod fake;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
