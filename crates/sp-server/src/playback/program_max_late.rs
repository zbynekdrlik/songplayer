//! #317: the late MAX boundaries, timestamped — a child module of
//! `program_max.rs` (its ring lives under the hand-off's stats lock).
//!
//! `max.compose_late` and `max.send_late` count how often a boundary's
//! compose started after its compose instant, or its Spout send after its
//! due instant; a counter alone cannot say WHEN. Measuring song changes on
//! SNV's wall (#317) needs each miss placed on the new song's open or on the
//! title text-set (1.6 s apart) to within a boundary (33 ms), next to
//! wall-bridge's `slow_shows`. So, like `ndi.readback_late_events`, the last
//! [`MAX_LATE_EVENTS`] late ones are kept, oldest first, as
//! `max.late_events`: the boundary's stamp (`utc_ms`, the internal timeline
//! `program/trace` and NDI's ring are on), which step was late (`kind`), by
//! how much (`late_us`) and when it started after the boundary's offer
//! (`after_offer_us`: the stamp + that = when it really happened). The
//! counters stay as they were.

use std::collections::VecDeque;
use std::time::Instant;

use serde::Serialize;

use super::MaxOut;

/// How many late MAX boundaries `max.late_events` keeps (the newest), as
/// many as `ndi.readback_late_events` (`ndi_feed::NDI_LATE_EVENTS`).
pub const MAX_LATE_EVENTS: usize = 32;

/// Which step of a MAX boundary came after its instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LateKind {
    /// The compose started after its compose instant (`max.compose_late`).
    Compose,
    /// The Spout send started after its due instant (`max.send_late`).
    Send,
}

/// One late step of a MAX boundary (`max.late_events`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MaxLateEvent {
    /// The boundary, ms: its internal stamp / 10 000, on the line of
    /// `program/trace` and `ndi.readback_late_events`.
    pub utc_ms: i64,
    pub kind: LateKind,
    /// µs from the step's instant to when it started.
    pub late_us: u64,
    /// µs from the boundary's offer to when the late step started.
    pub after_offer_us: u64,
}

/// A sent MAX boundary's late steps: for each, (its instant, when it
/// started); `None` when it was on time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lateness {
    pub offered: Instant,
    pub compose: Option<(Instant, Instant)>,
    pub send: Option<(Instant, Instant)>,
}

impl Lateness {
    /// The boundary stamped `stamp_100ns`'s late steps, compose first.
    pub fn events(&self, stamp_100ns: i64) -> Vec<MaxLateEvent> {
        let event = |kind, (at, started): (Instant, Instant)| MaxLateEvent {
            utc_ms: stamp_100ns / 10_000,
            kind,
            late_us: micros(started.saturating_duration_since(at)),
            after_offer_us: micros(started.saturating_duration_since(self.offered)),
        };
        let compose = self.compose.map(|c| event(LateKind::Compose, c));
        let send = self.send.map(|s| event(LateKind::Send, s));
        compose.into_iter().chain(send).collect()
    }
}

fn micros(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The last [`MAX_LATE_EVENTS`] late steps, oldest first.
#[derive(Clone, Debug, Default)]
pub struct LateRing {
    events: VecDeque<MaxLateEvent>,
}

impl LateRing {
    /// Keep `event`, dropping the oldest when full.
    pub fn push(&mut self, event: MaxLateEvent) {
        if self.events.len() == MAX_LATE_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// The steps kept, oldest first.
    pub fn events(&self) -> Vec<MaxLateEvent> {
        self.events.iter().copied().collect()
    }
}

impl MaxOut {
    /// #317: a sent boundary stamped `stamp_100ns` was late in `late`'s
    /// steps (nothing kept when it was on time).
    pub fn record_late(&self, stamp_100ns: i64, late: &Lateness) {
        let events = late.events(stamp_100ns);
        if events.is_empty() {
            return;
        }
        let mut stats = self.lock_stats();
        for event in events {
            stats.late_events.push(event);
        }
    }

    /// `max.late_events`, copied out under the stats lock.
    pub(super) fn late_events(&self) -> Vec<MaxLateEvent> {
        self.lock_stats().late_events.events()
    }
}

#[cfg(test)]
#[path = "program_max_late_tests.rs"]
mod tests;
