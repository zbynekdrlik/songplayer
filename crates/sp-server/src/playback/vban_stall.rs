//! #210 part 2: the VBAN thread's own late packets, per packet. Design
//! record: #210 comment 5916097259 (Approach 1, item 2).
//!
//! On the box single packets went out 11–20 ms late on a fixed 10 s grid,
//! the next one on time (finding 5915907311), visible only in a dev1
//! capture. So the `vban-output` thread times every packet it sends against
//! its planned instant, `due + L + k/240 s` (`VbanSender::send_block`), and
//! folds it into a [`VbanStallLog`] (pure, Linux-tested, the
//! `BoundaryTiming` pattern of `program_output_timing.rs`):
//!
//! - a packet sent more than [`VBAN_STALL_EVENT_US`] after its planned
//!   instant is an event, `(utc_ms, late_us)` ([`VbanLateEvent`]), kept in a
//!   ring of the last [`VBAN_STALL_RING`] and served oldest first as
//!   `vban.late_events` on `GET /api/v1/program`;
//! - `vban.late_max_us` is the worst packet of the last 60–120 s of sending:
//!   two buckets of [`VBAN_STALL_BUCKET_PACKETS`] packets, 60 s each at 240
//!   packets/s;
//! - ONE WARN per packet over [`VBAN_STALL_WARN_US`], at most one per
//!   [`VBAN_STALL_WARN_EVERY_100NS`] of VBAN's timeline, the next one
//!   carrying how many it skipped ([`VbanStallWarn`]).
//!
//! `utc_ms` is the fleet label of the send reading (`t + D(K_F)`,
//! `VbanClock::label_100ns`): UTC, the instant a dev1 capture lines up
//! with. Every packet the thread sends counts, so a block that reached the
//! thread after its first packet was due (see `health.timing`'s
//! `vban_feed_late_over_budget`) shows here too, as a run of events.

use std::collections::VecDeque;

use serde::Serialize;

use crate::playback::program_output_timing::us_after;

/// A packet more than this late (µs) is an event in the ring.
pub const VBAN_STALL_EVENT_US: u64 = 5_000;

/// A packet more than this late (µs) is WARNed (rate-limited).
pub const VBAN_STALL_WARN_US: u64 = 10_000;

/// At most one WARN per this much of VBAN's timeline (100 ns; 5 s), so the
/// 10 s grid of the measured stalls is never thinned.
pub const VBAN_STALL_WARN_EVERY_100NS: i64 = 50_000_000;

/// Events kept for `vban.late_events`.
pub const VBAN_STALL_RING: usize = 32;

/// Packets per bucket of the `late_max_us` window: 60 s at 240 packets/s.
pub const VBAN_STALL_BUCKET_PACKETS: u32 = 14_400;

/// One packet sent more than [`VBAN_STALL_EVENT_US`] late, as served under
/// `vban.late_events`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct VbanLateEvent {
    /// When it went out: UTC, ms since the Unix epoch.
    pub utc_ms: i64,
    /// How long after its planned instant it went out (µs).
    pub late_us: u64,
}

/// A packet to WARN about: over [`VBAN_STALL_WARN_US`] late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VbanStallWarn {
    pub event: VbanLateEvent,
    /// Packets over [`VBAN_STALL_WARN_US`] the rate limit skipped since the
    /// WARN before this one.
    pub suppressed: u64,
}

/// The VBAN thread's per-packet lateness window.
#[derive(Debug, Default)]
pub struct VbanStallLog {
    /// The last [`VBAN_STALL_RING`] events, oldest first.
    events: VecDeque<VbanLateEvent>,
    /// The worst packet of the bucket being filled and of the last full one.
    current_max_us: u64,
    previous_max_us: u64,
    /// Packets in the bucket being filled.
    in_current: u32,
    /// VBAN's timeline at the last WARN; `None` before the first.
    last_warn_100ns: Option<i64>,
    /// Packets over the WARN limit skipped since the last WARN.
    suppressed: u64,
}

impl VbanStallLog {
    /// Fold in one packet planned at `planned_100ns` and sent at
    /// `sent_100ns` (VBAN's timeline), whose send reading is labelled
    /// `sent_label_100ns` (UTC). Returns it when it is to be WARNed: over
    /// [`VBAN_STALL_WARN_US`] late and no WARN in the
    /// [`VBAN_STALL_WARN_EVERY_100NS`] before it.
    pub fn observe(
        &mut self,
        planned_100ns: i64,
        sent_100ns: i64,
        sent_label_100ns: i64,
    ) -> Option<VbanStallWarn> {
        let late_us = us_after(planned_100ns, sent_100ns);
        self.current_max_us = self.current_max_us.max(late_us);
        self.in_current += 1;
        if self.in_current == VBAN_STALL_BUCKET_PACKETS {
            self.previous_max_us = std::mem::take(&mut self.current_max_us);
            self.in_current = 0;
        }
        if late_us <= VBAN_STALL_EVENT_US {
            return None;
        }
        let event = VbanLateEvent {
            utc_ms: sent_label_100ns.div_euclid(10_000),
            late_us,
        };
        if self.events.len() == VBAN_STALL_RING {
            self.events.pop_front();
        }
        self.events.push_back(event);
        if late_us <= VBAN_STALL_WARN_US {
            return None;
        }
        let quiet = self
            .last_warn_100ns
            .is_none_or(|last| sent_100ns >= last + VBAN_STALL_WARN_EVERY_100NS);
        if !quiet {
            self.suppressed += 1;
            return None;
        }
        self.last_warn_100ns = Some(sent_100ns);
        Some(VbanStallWarn {
            event,
            suppressed: std::mem::take(&mut self.suppressed),
        })
    }

    /// The ring, oldest first (`vban.late_events`).
    pub fn late_events(&self) -> Vec<VbanLateEvent> {
        self.events.iter().copied().collect()
    }

    /// The worst packet of the last 60–120 s of sending, µs
    /// (`vban.late_max_us`).
    pub fn late_max_us(&self) -> u64 {
        self.current_max_us.max(self.previous_max_us)
    }
}

#[cfg(test)]
#[path = "vban_stall_tests.rs"]
mod tests;
