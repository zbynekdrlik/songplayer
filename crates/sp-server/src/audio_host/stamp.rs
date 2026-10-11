//! #233: a program block's boundary, moved from SongPlayer's timeline onto
//! the ASIO host's.
//!
//! Both processes read the same clocks (`WallClock::system`: the box's
//! realtime clock, scheduled on the monotonic one), and every wall's
//! TIMELINE is UTC − D(K), K the process's fleet relabel in whole slots
//! (`fleet_shift.rs`, #224 part 2). K is per PROCESS: a host started after
//! SongPlayer followed a date step starts at K = 0 while SongPlayer runs at
//! its K, so the same instant reads D(K_sp) − D(K_host) apart on the two
//! timelines. A stamp crosses the pipe with SongPlayer's K, and the host adds
//! that offset.
//!
//! Each side follows a later date step at its own wall's tick, a few slots
//! apart, and each moves its timeline by the remainder only (≤ one slot) —
//! the same transient two walls of ONE process have. So the offset is
//! SETTLED: the first block takes it at once, and a different one is adopted
//! only after it has held for [`SETTLE_100NS`] (1 s); a step's split second
//! between the two follows never adds whole slots to a stamp (a label-based
//! mapping would carry the whole step S there, and a block S ahead is a hard
//! re-centre). Two sides that disagree for good (one followed an epoch the
//! other did not) settle on the new offset after that second.

use crate::playback::fleet_shift::shift_100ns;

/// How long a new timeline offset must hold before the host adopts it.
pub const SETTLE_100NS: i64 = 10_000_000;

/// The host's mapping of SongPlayer's stamps (one per host connection).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StampMap {
    /// The offset in force (100 ns); `None` before the first block.
    offset_100ns: Option<i64>,
    /// A different offset seen, and since when (the host's clock).
    pending: Option<(i64, i64)>,
}

impl StampMap {
    /// The host-timeline boundary of a block stamped `due_100ns` on
    /// SongPlayer's timeline at its fleet relabel `sp_slots`, handled at
    /// `now_100ns` on a host whose relabel is `host_slots`.
    pub fn map(&mut self, due_100ns: i64, sp_slots: i64, host_slots: i64, now_100ns: i64) -> i64 {
        let wanted = shift_100ns(sp_slots) - shift_100ns(host_slots);
        let offset = *self.offset_100ns.get_or_insert(wanted);
        let held = match self.pending {
            Some((p, since)) if p == wanted => now_100ns - since,
            _ => 0,
        };
        if wanted == offset {
            self.pending = None;
        } else if held >= SETTLE_100NS {
            self.offset_100ns = Some(wanted);
            self.pending = None;
        } else if held == 0 {
            self.pending = Some((wanted, now_100ns));
        }
        due_100ns + self.offset_100ns.unwrap_or(offset)
    }

    /// The offset in force (100 ns), `None` before the first block.
    pub fn offset_100ns(&self) -> Option<i64> {
        self.offset_100ns
    }
}

#[cfg(test)]
#[path = "stamp_tests.rs"]
mod tests;
