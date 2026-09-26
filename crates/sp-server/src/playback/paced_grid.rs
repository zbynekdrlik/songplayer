//! The paced output's own boundary clock (#147, design record 5845527884,
//! Approach 1 (a)): the PURE bookkeeping behind the pipeline-lifetime submit
//! consumer.
//!
//! A song change, a stop or the end of an idle stretch used to leave the
//! genlock grid unserviced between the old scope's submit-thread join and the
//! next pre-roll: 51–84 ms on the box, 1–3 skipped slots, each one a
//! camera-box `stamp_gap`. The submit consumer now lives for the whole
//! pipeline and services those boundaries itself. While NO pacer is attached
//! (between two scopes) and no job arrived by `strict_next(last_serviced) +`
//! [`fill_grace_100ns`], it fills that boundary with the held picture + one
//! silent block. The next pacer continues at `last_serviced + 1 slot`
//! (`Pacer::continue_grid_after`), so the stamps stay contiguous.
//!
//! While a pacer IS attached it owns every boundary (its catch-up and its
//! `> 8`-slot resync are unchanged): a fill there would steal the boundary of
//! an emit that is merely late, and its aligned audio block would be lost.
//!
//! [`PacedGrid`] is that bookkeeping: the last serviced stamp, whether a pacer
//! is attached, when the next fill is due, and the telemetry. No clock reads,
//! no locks: the caller passes `now`. The `Mutex`/`Condvar` glue is
//! `paced_output.rs`.

use sp_core::genlock::{
    floor_boundary_100ns, genlock_emit_gate_100ns, interval_100ns, lag_slots_100ns,
    strict_next_boundary_100ns,
};

/// A detached boundary is filled this fraction of a slot after it: a quarter
/// slot (83 333 × 100 ns ≈ 8.3 ms at 30 fps) is the grace the previous
/// pacer's last job gets to arrive.
pub const FILL_GRACE_DIVISOR: i64 = 4;

/// The fill grace on a `fps` grid, in 100-ns units (0 when genlock is off).
pub fn fill_grace_100ns(fps: i64) -> i64 {
    interval_100ns(fps) / FILL_GRACE_DIVISOR
}

/// What the consumer does about the grid when no job is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridStep {
    /// No fill will come due: a pacer is attached, nothing was serviced yet,
    /// or genlock is off. Wait for a job.
    Idle,
    /// Wait for a job until this wall time (100 ns); then the next boundary
    /// is filled.
    WaitUntil(i64),
    /// Fill this boundary (a stamp, 100 ns) now.
    Fill(i64),
}

/// The paced output's grid bookkeeping (see the module doc).
#[derive(Clone, Debug)]
pub struct PacedGrid {
    fps: i64,
    /// The newest stamp the output serviced: a job taken or a fill decided.
    last_serviced_100ns: Option<i64>,
    /// A pacer (a song or an idle scope) is feeding the handoff.
    attached: bool,
    /// Set on detach, cleared by the first job of the NEXT pacer (a stamp after
    /// `continue_after`): the window in which a stamp gap is a song-change hole.
    in_transition: bool,
    /// What the attached pacer was told to continue after: the newest stamp
    /// serviced or still queued at attach. A job at or before it is the
    /// previous pacer's queued tail, which never closes the window.
    continue_after_100ns: Option<i64>,
    fill_pairs: u64,
    unserviced_slots: u64,
}

impl PacedGrid {
    /// A grid on `fps` with nothing serviced and no pacer attached.
    pub fn new(fps: i64) -> Self {
        Self {
            fps,
            last_serviced_100ns: None,
            attached: false,
            in_transition: false,
            continue_after_100ns: None,
            fill_pairs: 0,
            unserviced_slots: 0,
        }
    }

    /// The newest serviced stamp, if any.
    pub fn last_serviced_100ns(&self) -> Option<i64> {
        self.last_serviced_100ns
    }

    /// Whether a pacer is attached.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Boundaries the consumer filled itself (cumulative), `consumer_fill_pairs`.
    pub fn fill_pairs(&self) -> u64 {
        self.fill_pairs
    }

    /// Grid slots nobody serviced across a detach→attach window (cumulative),
    /// `song_change_unserviced_slots`. Must read 0.
    pub fn unserviced_slots(&self) -> u64 {
        self.unserviced_slots
    }

    /// A pacer starts feeding with nothing queued: [`attach_with_queued`]
    /// (`None`).
    ///
    /// [`attach_with_queued`]: Self::attach_with_queued
    pub fn attach(&mut self) -> Option<i64> {
        self.attach_with_queued(None)
    }

    /// A pacer starts feeding while the previous pacer's newest job
    /// `newest_queued` may still be queued. Returns the newest stamp serviced
    /// or queued: the pacer continues on the boundary right after it. From
    /// here no fill is decided.
    pub fn attach_with_queued(&mut self, newest_queued: Option<i64>) -> Option<i64> {
        self.attached = true;
        self.continue_after_100ns = self.last_serviced_100ns.max(newest_queued);
        self.continue_after_100ns
    }

    /// The pacer stopped feeding. From here the grid is the consumer's.
    pub fn detach(&mut self) {
        self.attached = false;
        self.in_transition = true;
    }

    /// A job a pacer handed over, stamped `stamp_100ns`. Returns `false` when
    /// the stamp is at or before the last serviced one: that job is never sent
    /// (the output's stamps only ever increase). Inside a detach→attach window
    /// a gap before it counts as unserviced slots.
    pub fn accept_job(&mut self, stamp_100ns: i64) -> bool {
        if self
            .last_serviced_100ns
            .is_some_and(|last| stamp_100ns <= last)
        {
            return false;
        }
        if self.in_transition {
            self.count_gap(stamp_100ns);
        }
        let from_new_pacer = self
            .continue_after_100ns
            .is_none_or(|after| stamp_100ns > after);
        if self.attached && from_new_pacer {
            self.in_transition = false;
        }
        self.last_serviced_100ns = Some(stamp_100ns);
        true
    }

    /// The grid decision at wall time `now_100ns` with no job queued. Only a
    /// DETACHED grid that has serviced a stamp fills: the next boundary once
    /// `now` reaches it + [`fill_grace_100ns`], or — when the consumer woke
    /// more than `GENLOCK_MAX_CATCHUP_INTERVALS` (8) slots late — the current
    /// boundary ([`fill_boundary`], a resync the caller WARNs about).
    pub fn step(&self, now_100ns: i64) -> GridStep {
        let Some(last) = self.last_serviced_100ns else {
            return GridStep::Idle;
        };
        if self.attached || interval_100ns(self.fps) == 0 {
            return GridStep::Idle;
        }
        let due = strict_next_boundary_100ns(last, self.fps);
        let deadline = due + fill_grace_100ns(self.fps);
        if now_100ns < deadline {
            GridStep::WaitUntil(deadline)
        } else {
            GridStep::Fill(fill_boundary(due, now_100ns, self.fps))
        }
    }

    /// Record the fill of `stamp_100ns` (a [`GridStep::Fill`]). Returns the
    /// slots it skipped: 0 for the normal next boundary, more after a resync.
    pub fn commit_fill(&mut self, stamp_100ns: i64) -> u64 {
        let skipped = self.count_gap(stamp_100ns);
        self.fill_pairs += 1;
        self.last_serviced_100ns = Some(stamp_100ns);
        skipped
    }

    /// Count the grid slots strictly between the last serviced stamp and
    /// `stamp_100ns` as unserviced; returns them.
    fn count_gap(&mut self, stamp_100ns: i64) -> u64 {
        let Some(last) = self.last_serviced_100ns else {
            return 0;
        };
        let between = lag_slots_100ns(last, stamp_100ns, self.fps) - 1;
        let skipped = between.max(0) as u64;
        self.unserviced_slots += skipped;
        skipped
    }
}

/// The boundary a fill services at `now` for the overdue boundary `due`: `due`
/// itself (a catch-up, one slot per fill), unless it is more than
/// `GENLOCK_MAX_CATCHUP_INTERVALS` (8) slots behind `now` — then the grid
/// boundary at or before `now`, a resync. It is the pacer's own rule (the
/// exact-grid emit gate with nothing buffered, as `Pacer::resolve_emit_boundary`
/// applies it), so the two can never drift apart.
pub fn fill_boundary(due_100ns: i64, now_100ns: i64, fps: i64) -> i64 {
    let (_, next) = genlock_emit_gate_100ns(now_100ns, due_100ns, fps, false);
    if next > strict_next_boundary_100ns(due_100ns, fps) {
        floor_boundary_100ns(now_100ns, fps)
    } else {
        due_100ns
    }
}

#[cfg(test)]
#[path = "paced_grid_tests.rs"]
mod tests;
