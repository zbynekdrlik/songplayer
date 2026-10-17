//! The `program-max` thread's log gate (#223 S2), a child module of
//! `program_max_worker.rs` (moved out for the 1000-line cap, #279 P1b): the
//! WARN when boundaries stop going out, the INFO when they go out again, at
//! most one line per [`MAX_LOG_EVERY_100NS`] of the thread's time, and a
//! change held back written by the first boundary after the window. Pure:
//! the worker logs the line [`LogGate::observe`] returns.

use std::time::Instant;

use crate::playback::stat_window::StateLog;

/// At most one log line of the thread's state (a failure WARN or a
/// recovery INFO) per 5 s of its time (100 ns).
pub const MAX_LOG_EVERY_100NS: i64 = 50_000_000;

/// The thread's time at `now` since `since`, in 100 ns (the unit of
/// `WarnLimiter`); 0 before `since`.
pub fn elapsed_100ns(since: Instant, now: Instant) -> i64 {
    let ns = now.saturating_duration_since(since).as_nanos();
    i64::try_from(ns / 100).unwrap_or(i64::MAX)
}

/// The line the log writes for one boundary ([`LogGate::observe`]).
#[derive(Debug, PartialEq, Eq)]
pub enum LogLine {
    /// Boundaries do not go out (a WARN with the reason).
    Failing { held_back: u64 },
    /// Boundaries go out again (an INFO).
    Recovered { held_back: u64 },
}

/// What the log last said about the thread, and its rate limit (a
/// `StateLog`). Pure: the worker logs the line it returns.
#[derive(Debug, Default)]
pub struct LogGate {
    /// What the log last said: `None` = boundaries go out (also the start),
    /// `Some(why)` = they do not, because of `why`.
    log: StateLog<Option<String>>,
}

impl LogGate {
    /// The state after a boundary at `at_100ns` (`failing` = why boundaries
    /// do not go out, `None` = they do). A line when the state differs from
    /// what the log last said, at most one per [`MAX_LOG_EVERY_100NS`]; a
    /// difference held back is written by the first boundary after the
    /// window, as the state is then (`held_back` = the boundaries held
    /// back since the last line).
    pub fn observe(&mut self, at_100ns: i64, failing: Option<&str>) -> Option<LogLine> {
        let differs = self.log.logged().as_deref() != failing;
        let owned = || failing.map(str::to_owned);
        let held_back = self
            .log
            .change(at_100ns, MAX_LOG_EVERY_100NS, differs, owned)?;
        Some(match failing {
            Some(_) => LogLine::Failing { held_back },
            None => LogLine::Recovered { held_back },
        })
    }
}
