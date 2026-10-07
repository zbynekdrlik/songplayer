//! #210 part 2: the VBAN thread's own late packets, per packet. Design
//! record: #210 comment 5916097259 (Approach 1, item 2).
//!
//! On the box single packets went out 11–20 ms late on a fixed 10 s grid,
//! the next one on time (finding 5915907311), visible only in a dev1
//! capture. So the `vban-output` thread times every packet it sends against
//! its planned instant, `due + L + k/240 s` (`VbanSender::send_block`), and
//! folds it into a [`VbanStallLog`] (pure, Linux-tested; its window and
//! its WARN rate limit are `stat_window.rs`'s, shared with the program
//! sender's `BoundaryTiming`):
//!
//! - a packet sent more than [`VBAN_STALL_EVENT_US`] after its planned
//!   instant is an event, `(utc_ms, late_us)` ([`VbanLateEvent`]), kept in a
//!   ring of the last [`VBAN_STALL_RING`] and served oldest first as
//!   `vban.late_events` (#233: each VBAN output's `outputs[i].vban` on
//!   `GET /api/v1/program`);
//! - `vban.late_max_us` is the worst packet of the last 60–120 s of sending
//!   at 48 kHz INT24: two buckets of [`VBAN_STALL_BUCKET_PACKETS`] packets,
//!   60 s each at 240 packets/s (#233: the buckets count packets, so a
//!   destination sending more packets a second covers less time, e.g. 30 s
//!   each at 96 kHz INT24's 480);
//! - ONE WARN per packet over [`VBAN_STALL_WARN_US`], at most one per
//!   [`VBAN_STALL_WARN_EVERY_100NS`] of VBAN's timeline, the next one
//!   carrying how many it skipped ([`VbanStallWarn`]).
//!
//! `utc_ms` is the fleet label of the send reading (`t + D(K_F)`,
//! `VbanClock::label_100ns`): UTC, the instant a dev1 capture lines up
//! with. In the ~14 min after a fleet date step VBAN's clock still owes
//! the step's movement (`vban.slew_owed_us`), and `utc_ms` is off UTC by
//! that much: before it after a forward follow (≤ one slot), after it
//! after a residue hold (≤ ~4 ms). Every packet the thread sends counts,
//! so a block that reached the thread more than 5 ms after its first packet
//! was due shows here too, as a run of events: its packets still over 5 ms
//! late, packet k about X − 4.167·k ms for a block X ms past due. (A block
//! 0–5 ms past due is counted only by `health.timing`'s
//! `vban_feed_late_over_budget` and by `late_sends`.)

use std::collections::VecDeque;

use serde::Serialize;

use crate::playback::program_output_timing::us_after;
use crate::playback::stat_window::{TwoBucketWorst, WarnLimiter};

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
#[derive(Clone, Debug, Default)]
pub struct VbanStallLog {
    /// The last [`VBAN_STALL_RING`] events, oldest first.
    events: VecDeque<VbanLateEvent>,
    /// The worst packet of the last one to two buckets of
    /// [`VBAN_STALL_BUCKET_PACKETS`].
    window: TwoBucketWorst<u64, VBAN_STALL_BUCKET_PACKETS>,
    /// At most one WARN per [`VBAN_STALL_WARN_EVERY_100NS`] of VBAN's
    /// timeline.
    limiter: WarnLimiter,
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
        self.window.push(late_us);
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
        let suppressed = self
            .limiter
            .admit(sent_100ns, VBAN_STALL_WARN_EVERY_100NS)?;
        Some(VbanStallWarn { event, suppressed })
    }

    /// The ring, oldest first (`vban.late_events`).
    pub fn late_events(&self) -> Vec<VbanLateEvent> {
        self.events.iter().copied().collect()
    }

    /// The worst packet of the last two buckets of packets (60–120 s of
    /// sending at 48 kHz INT24), µs (`vban.late_max_us`).
    pub fn late_max_us(&self) -> u64 {
        self.window.worst()
    }
}

#[cfg(test)]
#[path = "vban_stall_tests.rs"]
mod tests;
