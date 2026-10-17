//! #257: `GET /api/v1/audio/faults?hours=N` — the answer, built purely from
//! what the store read (`store.rs`) and the paths that run now.
//!
//! Per path: `clean_since_ms` = its newest `loss` row the ledger keeps,
//! looked up BEYOND the asked window (`store::clean_marks`), else its FIRST
//! `start` row it keeps (a planned restart is no fault: the main session's
//! ruling on #257), else the ledger's oldest instant (`kept_from_ms`) when
//! the window has rows of the path, else now (the ledger has not seen it
//! yet); the answer's own `clean_since_ms` is the whole chain's (the newest
//! loss of any path kept, else the first start kept); `last_loss_ms` /
//! `last_risk_ms` (the window's);
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
use sp_core::audio_faults_timeline::BUCKETS;

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

/// The window asked for: `hours` (1..=2160, default 24), by the one
/// whole-count parse (`sp_core::query_param`, the usage summary's `days`
/// too).
pub fn parse_hours(raw: Option<&str>) -> Result<u32, &'static str> {
    sp_core::query_param::whole_count(raw, DEFAULT_HOURS, MAX_HOURS).ok_or(HOURS_REFUSAL)
}

/// The window's first instant (UTC ms).
pub fn window_start(now_ms: i64, hours: u32) -> i64 {
    now_ms - i64::from(hours) * HOUR_MS
}

/// The oldest instant the ledger keeps at `now_ms` (its 90 days).
pub fn kept_from(now_ms: i64) -> i64 {
    now_ms - super::RETENTION_MS
}

/// One path's marks (`store::clean_marks`): its newest loss over the 90
/// days kept and its first start ever (the prune keeps each path's
/// earliest start).
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
    /// The window has a row of the path.
    seen: bool,
    /// Its newest loss the ledger keeps (`CleanMark`).
    kept_loss_ms: Option<i64>,
    /// Its first start the ledger keeps (`CleanMark`).
    first_start_ms: Option<i64>,
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
        self.seen = true;
        if t.severity == Severity::Loss.as_str() {
            self.last_loss_ms = newest(self.last_loss_ms);
            self.losses += t.rows;
            *self.counts.entry(t.kind.clone()).or_default() += faults;
        } else if t.severity == Severity::Risk.as_str() {
            self.last_risk_ms = newest(self.last_risk_ms);
            self.risks += t.rows;
            *self.counts.entry(t.kind.clone()).or_default() += faults;
        } else if t.kind == kinds::CHECKED {
            self.checked_ms += t.amount;
        } else if t.kind == kinds::BLIND {
            self.blind_ms += t.amount;
        }
    }

    fn answer(self, path: String, now_ms: i64, kept_from_ms: i64) -> PathFaults {
        let unseen = if self.seen { kept_from_ms } else { now_ms };
        // A first start older than the 90 days kept reads the ledger's edge:
        // "more than 90 d", never a time no loss row can still disprove.
        let first_start = self.first_start_ms.map(|s| s.max(kept_from_ms));
        let loop_check = path.starts_with("loop:").then(|| LoopSeconds {
            checked_s: self.checked_ms as f64 / 1000.0,
            blind_s: self.blind_ms as f64 / 1000.0,
        });
        PathFaults {
            clean_since_ms: self
                .kept_loss_ms
                .max(self.last_loss_ms)
                .or(first_start)
                .unwrap_or(unseen),
            path,
            detail: self.detail,
            live: self.live,
            last_loss_ms: self.last_loss_ms,
            last_risk_ms: self.last_risk_ms,
            losses: self.losses,
            risks: self.risks,
            counts: self.counts,
            events: self.events,
            loss_buckets: Vec::new(),
            risk_buckets: Vec::new(),
            starts: Vec::new(),
            loop_check,
        }
    }
}

/// The whole chain's clean time: the newest loss of any path the ledger
/// keeps (a path gone since counts too), else the first start it recorded
/// (no older than `kept_from_ms`), else `unseen`.
fn chain_since(paths: &[PathFaults], marks: &[CleanMark], kept_from_ms: i64, unseen: i64) -> i64 {
    let kept = marks.iter().filter_map(|m| m.last_loss_ms).max();
    let window = paths.iter().filter_map(|p| p.last_loss_ms).max();
    kept.max(window)
        .or_else(|| {
            marks
                .iter()
                .filter_map(|m| m.first_start_ms)
                .min()
                .map(|s| s.max(kept_from_ms))
        })
        .unwrap_or(unseen)
}

/// The answer at `now_ms` over `hours`, from the window's totals, its
/// newest events (newest first, up to [`MAX_EVENTS`] + 1), the name each
/// stored path's newest row carries, the paths that run now (their own
/// name wins), and every path's marks over all the ledger keeps.
pub fn build_answer(
    now_ms: i64,
    hours: u32,
    totals: &[KindTotal],
    newest: Vec<StoredEvent>,
    details: &[(String, String)],
    live: &[(String, String)],
    marks: &[CleanMark],
) -> FaultsAnswer {
    let from_ms = window_start(now_ms, hours);
    let kept_from_ms = kept_from(now_ms);
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
    for m in marks {
        if let Some(acc) = paths.get_mut(&m.path) {
            acc.kept_loss_ms = m.last_loss_ms;
            acc.first_start_ms = m.first_start_ms;
        }
    }
    let unseen = if paths.values().any(|a| a.seen) {
        kept_from_ms
    } else {
        now_ms
    };
    let paths: Vec<PathFaults> = paths
        .into_iter()
        .map(|(path, acc)| acc.answer(path, now_ms, kept_from_ms))
        .collect();
    let worst = paths
        .iter()
        .filter_map(|p| p.last_loss_ms.map(|t| (t, &p.path)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, path)| path.clone());
    let clean_since_ms = chain_since(&paths, marks, kept_from_ms, unseen);
    FaultsAnswer {
        now_ms,
        from_ms,
        hours,
        kept_from_ms,
        clean_since_ms,
        worst,
        truncated,
        paths,
    }
}

/// One `(path, severity, bucket)` count of the window
/// (`store::bucket_counts`): `bucket` 0..BUCKETS by sp-core's edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketCount {
    pub path: String,
    pub severity: String,
    pub bucket: i64,
    pub rows: i64,
}

/// The (re)starts a path's timeline carries at most (the newest).
pub const MAX_STARTS: usize = 100;

/// Each path's timeline (#257 review: the bars and marks never come from
/// the capped event list): its loss and risk rows per bucket of the window
/// (`counts`, every row), and its (re)starts in it (`starts`, the newest
/// [`MAX_STARTS`], oldest first). A path no count names reads zeros; a
/// bucket outside 0..BUCKETS is no bucket.
pub fn add_timeline(answer: &mut FaultsAnswer, counts: &[BucketCount], starts: &[(String, i64)]) {
    for p in &mut answer.paths {
        let mut loss = vec![0u32; BUCKETS];
        let mut risk = vec![0u32; BUCKETS];
        for c in counts.iter().filter(|c| c.path == p.path) {
            let Some(slot) = usize::try_from(c.bucket).ok().filter(|b| *b < BUCKETS) else {
                continue;
            };
            let rows = u32::try_from(c.rows).unwrap_or(u32::MAX);
            if c.severity == Severity::Loss.as_str() {
                loss[slot] = loss[slot].saturating_add(rows);
            } else if c.severity == Severity::Risk.as_str() {
                risk[slot] = risk[slot].saturating_add(rows);
            }
        }
        let mut mine: Vec<i64> = starts
            .iter()
            .filter(|(path, _)| *path == p.path)
            .map(|(_, at)| *at)
            .collect();
        mine.sort_unstable();
        p.starts = mine.split_off(mine.len().saturating_sub(MAX_STARTS));
        p.loss_buckets = loss;
        p.risk_buckets = risk;
    }
}

#[cfg(test)]
#[path = "answer_tests.rs"]
mod tests;
