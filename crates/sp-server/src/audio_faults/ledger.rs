//! #257: the ledger's decisions, pure: what one tick's [`Probe`]s write.
//!
//! - A path seen for the first time (in this process, or again after it was
//!   gone) or REBUILT writes a `start` row (severity info). That tick is its
//!   BASELINE: its counters, events and states are taken, none is written.
//!   So a counter SongPlayer keeps across a rebuild (an ASIO output's own
//!   queue drops and unsent blocks across a host restart) is never counted
//!   twice, and an output's start-up drops are no fault.
//! - A path is rebuilt when its run counter or any counter or meter went
//!   DOWN since the last tick. Rebuilt while it was gated in (an ASIO host
//!   that restarted between two ticks, an entry or a pipeline rebuilt while
//!   it ran): one `interrupted` loss row next to its start, its audio broke
//!   off there.
//! - A step is recorded only when the path was gated in at the tick before
//!   (and not rebuilt since): the tick a path comes in (a pipeline cut to
//!   program, an output that starts running) is its baseline, and the tick
//!   it goes out still records what grew until then (a host gone down while
//!   its output ran). Faults while it is out are taken, never written later.
//! - A counter that grew writes one row, the step as its amount.
//! - An event (its own UTC) writes one row at its own time, once: its
//!   `(path, kind, utc_ms, amount)` is remembered ([`SEEN_KEPT`] keys), so
//!   two VBAN packets late in the same ms are two rows unless they were late
//!   to the same µs (no packet index is served to tell them apart).
//! - A state writes one row when it starts to hold.
//! - A meter's growth is summed and written at most once a
//!   [`METER_FLUSH_MS`] per path (a rebuilt path's sum first); meters are
//!   never gated and count from the path's build.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use super::model::{FaultRow, Probe, Severity, kinds};

/// Event keys remembered for the dedup. A path lists at most 32 events at a
/// time (the loop check's and VBAN's rings), so this outlasts every list.
pub const SEEN_KEPT: usize = 4096;

/// A path's meter rows are written at most once a minute.
pub const METER_FLUSH_MS: i64 = 60_000;

/// `(path, kind, utc_ms, amount)`.
type EventKey = (String, &'static str, i64, i64);

/// What the ledger keeps of one path between ticks.
#[derive(Debug, Default)]
struct PathState {
    run: u64,
    counters: BTreeMap<&'static str, u64>,
    states: BTreeMap<&'static str, bool>,
    meters: BTreeMap<&'static str, u64>,
    /// Meter growth not written yet.
    pending: BTreeMap<&'static str, u64>,
    /// When the meters were last written (or the path started), UTC ms.
    flushed_ms: i64,
    gated_in: bool,
}

/// The ledger: the last tick of every path, and the events already written.
#[derive(Debug, Default)]
pub struct Ledger {
    paths: HashMap<String, PathState>,
    seen: HashSet<EventKey>,
    seen_order: VecDeque<EventKey>,
}

/// A step as a row amount.
fn amount(step: u64) -> i64 {
    i64::try_from(step).unwrap_or(i64::MAX)
}

/// Whether `probe` shows its path rebuilt since `state`.
fn rebuilt(state: &PathState, probe: &Probe) -> bool {
    let went_down = |kept: &BTreeMap<&'static str, u64>, kind: &str, now: u64| {
        kept.get(kind).is_some_and(|&before| now < before)
    };
    probe.run < state.run
        || probe
            .counters
            .iter()
            .any(|c| went_down(&state.counters, c.kind, c.value))
        || probe
            .meters
            .iter()
            .any(|m| went_down(&state.meters, m.kind, m.value_ms))
}

impl Ledger {
    /// The rows one tick writes at `now_ms` (UTC). A path no probe names any
    /// more is forgotten.
    pub fn tick(&mut self, probes: &[Probe], now_ms: i64) -> Vec<FaultRow> {
        let mut rows = Vec::new();
        self.paths
            .retain(|path, _| probes.iter().any(|p| &p.path == path));
        for probe in probes {
            let before = self.paths.remove(&probe.path);
            let state = self.step(probe, before, now_ms, &mut rows);
            self.paths.insert(probe.path.clone(), state);
        }
        rows
    }

    /// One path's rows, and what is kept of it.
    fn step(
        &mut self,
        probe: &Probe,
        before: Option<PathState>,
        now_ms: i64,
        rows: &mut Vec<FaultRow>,
    ) -> PathState {
        let row = |kind, severity, value, ts_ms| FaultRow {
            ts_ms,
            path: probe.path.clone(),
            kind,
            severity,
            amount: value,
            detail: probe.detail.clone(),
        };
        let was_gated = before.as_ref().is_some_and(|s| s.gated_in);
        let restarted = before.as_ref().is_none_or(|s| rebuilt(s, probe));
        let mut base = before.unwrap_or_default();
        if restarted {
            for (kind, sum) in std::mem::take(&mut base.pending) {
                if sum > 0 {
                    rows.push(row(kind, Severity::Meter, amount(sum), now_ms));
                }
            }
            base = PathState {
                flushed_ms: now_ms,
                ..PathState::default()
            };
            rows.push(row(kinds::START, Severity::Info, 1, now_ms));
            if was_gated {
                rows.push(row(kinds::INTERRUPTED, Severity::Loss, 1, now_ms));
            }
        }
        let record = was_gated && !restarted;
        for c in &probe.counters {
            let step = c.value - base.counters.get(c.kind).copied().unwrap_or(0);
            if step > 0 && record {
                rows.push(row(c.kind, c.severity, amount(step), now_ms));
            }
        }
        for e in &probe.events {
            if self.remember((probe.path.clone(), e.kind, e.utc_ms, e.amount)) && record {
                rows.push(row(e.kind, e.severity, e.amount, e.utc_ms));
            }
        }
        for s in &probe.states {
            let held = base.states.get(s.kind).copied().unwrap_or(false);
            if s.active && !held && record {
                rows.push(row(s.kind, s.severity, 1, now_ms));
            }
        }
        for m in &probe.meters {
            let step = m.value_ms - base.meters.get(m.kind).copied().unwrap_or(0);
            *base.pending.entry(m.kind).or_default() += step;
        }
        if now_ms - base.flushed_ms >= METER_FLUSH_MS {
            for (kind, sum) in std::mem::take(&mut base.pending) {
                if sum > 0 {
                    rows.push(row(kind, Severity::Meter, amount(sum), now_ms));
                }
            }
            base.flushed_ms = now_ms;
        }
        PathState {
            run: probe.run,
            counters: probe.counters.iter().map(|c| (c.kind, c.value)).collect(),
            states: probe.states.iter().map(|s| (s.kind, s.active)).collect(),
            meters: probe.meters.iter().map(|m| (m.kind, m.value_ms)).collect(),
            pending: base.pending,
            flushed_ms: base.flushed_ms,
            gated_in: probe.gated_in,
        }
    }

    /// Remember an event; `false` when it was written already. The oldest
    /// key goes once more than [`SEEN_KEPT`] are kept.
    fn remember(&mut self, key: EventKey) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.seen_order.push_back(key);
        if self.seen_order.len() > SEEN_KEPT
            && let Some(oldest) = self.seen_order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        true
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
