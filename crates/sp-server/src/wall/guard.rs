//! #254 (SNV 14.10.2026): the wall report's guard — it ends a crashed or
//! hung Resolume Arena so the operator's relaunch script brings it back.
//!
//! Why: Arena's render thread crashed, its crash reporter (BugSplat, the
//! child `BugSplatMonitor.exe`) showed a window "Crash Report" and waited
//! for a click, and the crashed thread kept the locks the main thread waits
//! on. On a PC with no screen, keyboard or mouse nobody clicks, so Arena
//! never exits, and the operator's hotkey script relaunches Arena only once
//! its process is gone. The wall picture froze for good.
//!
//! **The owner's exception** (ROZHODNUTÉ 14.10.2026, #254 comment
//! 6169316862): the wall report reads and reports only, except this — the
//! guard ends a DEAD Arena by itself. It never touches Arena's setup, a
//! display, or a clip (clips run only from the operator's Stream Deck); the
//! relaunch stays the operator's script.
//!
//! The rule ([`decide`], pure, at each 5 s wall check):
//!
//! - an Arena whose own crash reporter shows its "Crash Report" window, or
//!   whose visible windows do not answer while its CPU time stands still
//!   for [`HUNG_FOR_MS`], is ended: its crash reporter's processes, then
//!   Arena (the Windows glue `guard_win.rs`, each process's image name —
//!   and Arena's start time — checked first);
//! - never while its CPU time moves (Arena loading a composition does not
//!   answer either, and it works): the watch starts again at every move;
//! - at most once per Arena process (its id AND its start time: a process
//!   id is reused);
//! - a check that read nothing ends nothing and starts every watch anew.
//!
//! After an end the status names it until a NEW Arena runs ([`GuardPhase`]
//! `relaunching`); one that has not come back [`RELAUNCH_WAIT_MS`] after the
//! end is `not_back`, with one WARN. The vocabulary (reasons, phases, Slovak
//! texts) is `sp_core::wall`.

use sp_core::wall::{GuardEnd, GuardPhase, GuardWhy, WallGuard};

/// How long an Arena that does not answer must keep its CPU time still
/// before it is ended.
pub const HUNG_FOR_MS: i64 = 30_000;

/// How long after an end a new Arena is waited for before the status says
/// it did not come back.
pub const RELAUNCH_WAIT_MS: i64 = 60_000;

/// Arena's crash reporter (BugSplat's monitor; Arena starts it as its
/// child).
pub const CRASH_MONITOR_IMAGE: &str = "BugSplatMonitor.exe";

/// The title of the crash reporter's window that waits for a click.
pub const CRASH_REPORT_TITLE: &str = "Crash Report";

/// A process's times (`GetProcessTimes`), in 100 ns units: when it started
/// (its identity next to its id) and the CPU time it used so far (kernel +
/// user).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessTimes {
    pub created_100ns: u64,
    pub cpu_100ns: u64,
}

impl ProcessTimes {
    /// The times from `GetProcessTimes`' FILETIMEs (`(low, high)` each).
    pub fn from_filetimes(created: (u32, u32), kernel: (u32, u32), user: (u32, u32)) -> Self {
        Self {
            created_100ns: filetime_100ns(created.0, created.1),
            cpu_100ns: filetime_100ns(kernel.0, kernel.1)
                .saturating_add(filetime_100ns(user.0, user.1)),
        }
    }
}

/// A FILETIME's two halves as one count of 100 ns.
pub fn filetime_100ns(low: u32, high: u32) -> u64 {
    (u64::from(high) << 32) + u64::from(low)
}

/// A process's image (`C:\…\Arena.exe`, or a bare file name) is `image`:
/// its file name, ASCII case ignored.
pub fn is_image(path: &str, image: &str) -> bool {
    path.rsplit(['\\', '/'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case(image))
}

/// One Arena process as the guard reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArenaProcess {
    pub pid: u32,
    /// Its start and CPU time (why they could not be read).
    pub times: Result<ProcessTimes, String>,
    /// A visible top-level window of it does not answer
    /// (`IsHungAppWindow`).
    pub hung: Result<bool, String>,
    /// Its crash reporter's processes: each `BugSplatMonitor.exe` it
    /// started (`read::guard_facts`: its parent, and started after it), by
    /// id and start time.
    pub monitors: Vec<ProcessKey>,
    /// One of them shows a visible window titled [`CRASH_REPORT_TITLE`].
    pub crash_report: Result<bool, String>,
}

/// What one check read for the guard: every Arena process, or why the
/// process list could not be read.
pub type GuardFacts = Result<Vec<ArenaProcess>, String>;

/// One process: its id and its start time (an id is reused, the pair is
/// not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessKey {
    pub pid: u32,
    pub created_100ns: u64,
}

impl ArenaProcess {
    /// Its identity (none while its times cannot be read).
    pub fn key(&self) -> Option<ProcessKey> {
        self.times.as_ref().ok().map(|t| ProcessKey {
            pid: self.pid,
            created_100ns: t.created_100ns,
        })
    }
}

/// The end of one Arena the guard orders: its crash reporter's processes
/// first, then Arena itself, each only while it is still the process
/// started at the time read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndOrder {
    pub pid: u32,
    pub created_100ns: u64,
    pub why: GuardWhy,
    pub monitors: Vec<ProcessKey>,
}

/// What the guard logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardEvent {
    /// It ended Arena, or tried to (`error`): one ERROR.
    Ended(GuardEnd),
    /// No new Arena [`RELAUNCH_WAIT_MS`] after the end: one WARN.
    NotBack(GuardEnd),
    /// A new Arena runs after the end: one INFO.
    Back { pid: u32 },
}

/// An Arena that does not answer: its CPU time when it was first seen
/// still, and since when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Still {
    key: ProcessKey,
    cpu_100ns: u64,
    since_ms: i64,
}

/// The guard's memory between checks.
#[derive(Debug, Default)]
pub struct Guard {
    /// Each Arena that does not answer, watched.
    still: Vec<Still>,
    /// The Arena processes ended (each once), while they are listed.
    ended: Vec<ProcessKey>,
    /// What the status shows.
    record: WallGuard,
}

impl Guard {
    /// The guard as the status shows it.
    pub fn view(&self) -> WallGuard {
        self.record.clone()
    }
}

/// What one check calls for: the events of the relaunch's watch, and the
/// end to run (at most one per check).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub events: Vec<GuardEvent>,
    pub end: Option<EndOrder>,
}

/// The guard's step on one check's facts at `now_ms`: the relaunch's watch
/// first, then the first Arena the facts call to end (marked ended at once:
/// it is ordered once, whatever the end's result).
pub fn decide(guard: &mut Guard, facts: &GuardFacts, now_ms: i64) -> Step {
    let mut step = Step::default();
    let Ok(arenas) = facts else {
        // Nothing read: no end, and a hang's watch starts again.
        guard.still.clear();
        return step;
    };
    guard
        .ended
        .retain(|key| arenas.iter().any(|a| a.pid == key.pid));
    guard
        .still
        .retain(|s| arenas.iter().any(|a| a.key() == Some(s.key)));
    follow(guard, arenas, now_ms, &mut step.events);
    for arena in arenas {
        let Some(key) = arena.key() else {
            continue; // its identity unknown: nothing to act on
        };
        if guard.ended.contains(&key) {
            continue;
        }
        if let Some(why) = why_end(guard, arena, key, now_ms) {
            guard.ended.push(key);
            step.end = Some(EndOrder {
                pid: key.pid,
                created_100ns: key.created_100ns,
                why,
                monitors: arena.monitors.clone(),
            });
            break;
        }
    }
    step
}

/// Why `arena` (`key`) is ended now, if it is: its crash reporter's window,
/// or no answer with its CPU time still for [`HUNG_FOR_MS`] since the check
/// that first saw it so (a move of its CPU time starts the watch again).
fn why_end(
    guard: &mut Guard,
    arena: &ArenaProcess,
    key: ProcessKey,
    now_ms: i64,
) -> Option<GuardWhy> {
    if arena.crash_report == Ok(true) {
        return Some(GuardWhy::CrashReport);
    }
    let (Ok(true), Ok(times)) = (&arena.hung, &arena.times) else {
        guard.still.retain(|s| s.key != key);
        return None;
    };
    let cpu_100ns = times.cpu_100ns;
    match guard.still.iter_mut().find(|s| s.key == key) {
        Some(still) if still.cpu_100ns == cpu_100ns => {
            (now_ms - still.since_ms >= HUNG_FOR_MS).then_some(GuardWhy::Hung)
        }
        Some(still) => {
            *still = Still {
                key,
                cpu_100ns,
                since_ms: now_ms,
            };
            None
        }
        None => {
            guard.still.push(Still {
                key,
                cpu_100ns,
                since_ms: now_ms,
            });
            None
        }
    }
}

/// The relaunch's watch after an end: a new Arena (one not ended) runs →
/// `idle`, one INFO; none [`RELAUNCH_WAIT_MS`] after the end → `not_back`,
/// one WARN.
fn follow(guard: &mut Guard, arenas: &[ArenaProcess], now_ms: i64, events: &mut Vec<GuardEvent>) {
    if guard.record.phase == GuardPhase::Idle {
        return;
    }
    let Some(last) = guard.record.last.clone() else {
        return;
    };
    let back = arenas
        .iter()
        .find(|a| a.key().is_some_and(|key| !guard.ended.contains(&key)));
    if let Some(back) = back {
        guard.record.phase = GuardPhase::Idle;
        events.push(GuardEvent::Back { pid: back.pid });
    } else if guard.record.phase == GuardPhase::Relaunching
        && now_ms - last.at_ms >= RELAUNCH_WAIT_MS
    {
        guard.record.phase = GuardPhase::NotBack;
        events.push(GuardEvent::NotBack(last));
    }
}

/// Records the end `order` ran at `now_ms` with `result` (the glue's: why a
/// process could not be ended), and the event to log.
pub fn record_end(
    guard: &mut Guard,
    order: &EndOrder,
    result: Result<(), String>,
    now_ms: i64,
) -> GuardEvent {
    let end = GuardEnd {
        at_ms: now_ms,
        why: order.why,
        pid: order.pid,
        monitors: order.monitors.iter().map(|m| m.pid).collect(),
        error: result.err(),
    };
    guard.record.ends = guard.record.ends.saturating_add(1);
    guard.record.last = Some(end.clone());
    guard.record.phase = GuardPhase::Relaunching;
    GuardEvent::Ended(end)
}

/// One check's guard: [`decide`], then the end it orders run through `end`
/// (the Windows glue in production, a recorder in a test) and recorded.
/// Answers what to log, in order.
pub fn step(
    guard: &mut Guard,
    facts: &GuardFacts,
    now_ms: i64,
    end: &mut dyn FnMut(&EndOrder) -> Result<(), String>,
) -> Vec<GuardEvent> {
    let Step {
        mut events,
        end: order,
    } = decide(guard, facts, now_ms);
    if let Some(order) = order {
        let result = end(&order);
        events.push(record_end(guard, &order, result, now_ms));
    }
    events
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
