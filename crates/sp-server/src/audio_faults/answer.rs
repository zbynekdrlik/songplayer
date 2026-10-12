//! #257: `GET /api/v1/audio/faults?hours=N` — the answer, built purely from
//! what the store read (`store.rs`) and the paths that run now.
//!
//! Per path: `clean_since_ms` = its last `loss` row in the window, else its
//! latest `start` row in it, else the window's start (a path with no row
//! in the window was clean for all of it); `last_loss_ms` / `last_risk_ms`;
//! `losses` / `risks` = the window's loss / risk rows (each one fault the
//! ledger recorded at its time: the dashboard's count);
//! `counts` = the faults per kind (loss and risk rows: a counter's steps
//! summed, one per event or state row, `model::occurrences`); `events` =
//! its rows that are no meter, oldest first; a loop check's `loop`
//! `{checked_s, blind_s}` from its meters. At most [`MAX_EVENTS`] events in
//! the whole answer, the newest (`truncated`). `worst` = the path with the
//! most recent loss.
//!
//! The answer's types are `sp_core::audio_faults`'s (lane 2): the dashboard
//! reads the very structs the server writes.

use std::collections::BTreeMap;

pub use sp_core::audio_faults::{FaultEvent, FaultsAnswer, LoopSeconds, PathFaults};

use super::model::{Severity, kinds, occurrences};

/// Events in one answer at most (the newest).
pub const MAX_EVENTS: usize = 500;
/// The window when `hours` is not given.
pub const DEFAULT_HOURS: u32 = 24;
/// The longest window: the 90 days the ledger keeps.
pub const MAX_HOURS: u32 = 2160;
/// The 400 text of an `hours` that is not 1..=2160 (never the value sent).
pub const HOURS_REFUSAL: &str = "hours must be a whole number from 1 to 2160";
const HOUR_MS: i64 = 3_600_000;

/// The window asked for: `hours` (1..=2160, default 24).
pub fn parse_hours(raw: Option<&str>) -> Result<u32, &'static str> {
    let Some(text) = raw else {
        return Ok(DEFAULT_HOURS);
    };
    let digits = text.bytes().all(|b| b.is_ascii_digit());
    let hours = if digits {
        text.parse::<u32>().ok()
    } else {
        None
    };
    hours
        .filter(|h| (1..=MAX_HOURS).contains(h))
        .ok_or(HOURS_REFUSAL)
}

/// The window's first instant (UTC ms).
pub fn window_start(now_ms: i64, hours: u32) -> i64 {
    now_ms - i64::from(hours) * HOUR_MS
}

/// The oldest instant the ledger keeps at `now_ms` (its 90 days).
pub fn kept_from(now_ms: i64) -> i64 {
    now_ms - super::RETENTION_MS
}

/// One path's marks over every row the ledger keeps
/// (`store::clean_marks`): its newest loss and its first start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanMark {
    pub path: String,
    pub last_loss_ms: Option<i64>,
    pub first_start_ms: Option<i64>,
}

/// One `(path, kind, severity)` group of the window (`store::totals`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindTotal {
    pub path: String,
    pub kind: String,
    pub severity: String,
    /// The amounts summed.
    pub amount: i64,
    pub rows: i64,
    /// The newest row's time.
    pub last_ms: i64,
}

/// One stored row that is no meter (`store::newest_events`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub ts_ms: i64,
    pub path: String,
    pub kind: String,
    pub severity: String,
    pub amount: i64,
}

/// What one path gathers before it is answered.
#[derive(Debug, Default)]
struct PathAcc {
    detail: String,
    live: bool,
    last_loss_ms: Option<i64>,
    last_risk_ms: Option<i64>,
    last_start_ms: Option<i64>,
    losses: i64,
    risks: i64,
    counts: BTreeMap<String, i64>,
    events: Vec<FaultEvent>,
    checked_ms: i64,
    blind_ms: i64,
}

impl PathAcc {
    fn add(&mut self, t: &KindTotal) {
        let newest = |kept: Option<i64>| Some(kept.map_or(t.last_ms, |k| k.max(t.last_ms)));
        let faults = occurrences(&t.kind, t.rows, t.amount);
        if t.severity == Severity::Loss.as_str() {
            self.last_loss_ms = newest(self.last_loss_ms);
            self.losses += t.rows;
            *self.counts.entry(t.kind.clone()).or_default() += faults;
        } else if t.severity == Severity::Risk.as_str() {
            self.last_risk_ms = newest(self.last_risk_ms);
            self.risks += t.rows;
            *self.counts.entry(t.kind.clone()).or_default() += faults;
        } else if t.kind == kinds::START {
            self.last_start_ms = newest(self.last_start_ms);
        } else if t.kind == kinds::CHECKED {
            self.checked_ms += t.amount;
        } else if t.kind == kinds::BLIND {
            self.blind_ms += t.amount;
        }
    }

    fn answer(self, path: String, from_ms: i64) -> PathFaults {
        let loop_check = path.starts_with("loop:").then(|| LoopSeconds {
            checked_s: self.checked_ms as f64 / 1000.0,
            blind_s: self.blind_ms as f64 / 1000.0,
        });
        PathFaults {
            clean_since_ms: self.last_loss_ms.or(self.last_start_ms).unwrap_or(from_ms),
            path,
            detail: self.detail,
            live: self.live,
            last_loss_ms: self.last_loss_ms,
            last_risk_ms: self.last_risk_ms,
            losses: self.losses,
            risks: self.risks,
            counts: self.counts,
            events: self.events,
            loop_check,
        }
    }
}

/// The answer at `now_ms` over `hours`, from the window's totals, its
/// newest events (newest first, up to [`MAX_EVENTS`] + 1), the name each
/// stored path's newest row carries, and the paths that run now (their own
/// name wins).
pub fn build_answer(
    now_ms: i64,
    hours: u32,
    totals: &[KindTotal],
    newest: Vec<StoredEvent>,
    details: &[(String, String)],
    live: &[(String, String)],
    _marks: &[CleanMark],
) -> FaultsAnswer {
    let from_ms = window_start(now_ms, hours);
    let mut paths: BTreeMap<String, PathAcc> = BTreeMap::new();
    for (path, detail) in details {
        paths.entry(path.clone()).or_default().detail = detail.clone();
    }
    for (path, detail) in live {
        let acc = paths.entry(path.clone()).or_default();
        acc.detail = detail.clone();
        acc.live = true;
    }
    for t in totals {
        paths.entry(t.path.clone()).or_default().add(t);
    }
    let truncated = newest.len() > MAX_EVENTS;
    for e in newest.into_iter().take(MAX_EVENTS).rev() {
        paths.entry(e.path).or_default().events.push(FaultEvent {
            ts_ms: e.ts_ms,
            kind: e.kind,
            severity: e.severity,
            amount: e.amount,
        });
    }
    let paths: Vec<PathFaults> = paths
        .into_iter()
        .map(|(path, acc)| acc.answer(path, from_ms))
        .collect();
    let worst = paths
        .iter()
        .filter_map(|p| p.last_loss_ms.map(|t| (t, &p.path)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, path)| path.clone());
    FaultsAnswer {
        now_ms,
        from_ms,
        hours,
        kept_from_ms: kept_from(now_ms),
        clean_since_ms: from_ms,
        worst,
        truncated,
        paths,
    }
}

#[cfg(test)]
#[path = "answer_tests.rs"]
mod tests;
