//! #215: the transition-window half of [`ProgramCore`] — what a boundary
//! inside a window becomes. A child of `program_bus.rs` (1000-line cap), so it
//! reaches the core's private state directly.
//!
//! A window takes the outgoing source's pairs (`from_pending`) from its cut
//! boundary to its end, next to the incoming source's (`pending`). What a
//! boundary becomes depends on the window's cue (`program_transition::Cue`):
//!
//! - **Open** — the mix: ONE [`ProgramJob::Mix`] once each side is here or
//!   missed (the #209 per-source rules), a missing side mixed against the
//!   standby.
//! - **Waiting** (the cue gate) — the fade starts on the first boundary whose
//!   INCOMING pair is live (`SubmitJob::live`), or on the deadline
//!   (`CUE_WAIT_MAX_SLOTS` after the cut) without one (`cue_timeouts`). Until
//!   then each boundary is HELD: the outgoing source's own pair at full level
//!   (or the program's standby pair when it missed), and the incoming side's
//!   pair (a fill, the pre-roll black, a paused frozen frame) is dropped.
//! - **Frozen** — a later cut landed inside its span (on or before its end)
//!   while it was still waiting: every boundary is held to the window's end,
//!   and the next window fades out of the source that was really on program.
//!   A cut after its latest end leaves it waiting (`Window::truncate`).
//!
//! A held boundary waits for the incoming side like a mixed one does (it may
//! be the live pair that opens the cue); with neither side here and both
//! missed, it is filled like any boundary (the core's fill + resync path).

use sp_core::config::PROGRAM_BLANK_ID;
use tracing::{info, warn};

use super::{ProgramCore, ProgramJob};
use crate::playback::program_transition::{CUE_WAIT_MAX_SLOTS, Cue, MixJob, Window};

/// #257: whether a boundary that shows `source` carries a real source's
/// audio: a playlist, or the NDI input (-1). Blank (-2) and nothing
/// selected are silence by design, so their fills are no fault.
pub(super) fn real_source(source: Option<i64>) -> bool {
    source.is_some_and(|s| s != PROGRAM_BLANK_ID)
}

/// What a window boundary did (see [`ProgramCore::window_step`]).
pub(super) enum WindowStep {
    /// A boundary was queued, or the cue opened: go on releasing.
    Next,
    /// Nothing queued. `true` = neither side is here and both missed it: fill
    /// it like any boundary.
    Missed(bool),
}

impl ProgramCore {
    /// A source touched or offered boundary `stamp_100ns`: its progress (its
    /// liveness for the fills), and #257: it delivers since it became the
    /// owner ([`Self::delivering`]).
    pub(super) fn note_progress(&mut self, pid: i64, stamp_100ns: i64) {
        self.last_offer.insert(pid, stamp_100ns);
        self.delivered.insert(pid);
    }

    /// #257: whether `source` is a real source that touched or offered a
    /// boundary since it became the owner (a cut to it), or since the bus
    /// started: only then is a boundary it misses its audio gone. A fill of a
    /// source that has delivered nothing yet is a WAIT, never a loss: the
    /// selection restored at a SongPlayer start before its pipeline offers
    /// (SNV 12.10.2026: a `program` loss after every deploy), a cut to a
    /// source that has not reached the bus yet.
    fn delivering(&self, source: Option<i64>) -> bool {
        real_source(source) && source.is_some_and(|s| self.delivered.contains(&s))
    }

    /// Boundary `stamp_100ns` filled with the program's standby pair,
    /// showing `shows` (`fill_source`): `filled`, and #257's `filled_owned`
    /// when a real source's audio went missing there ([`Self::owned`]).
    pub(super) fn count_fill(&mut self, stamp_100ns: i64, shows: Option<i64>) {
        self.health.filled += 1;
        self.health.filled_owned += u64::from(self.owned(stamp_100ns, shows));
    }

    /// A resync: the missed boundaries from `stamp_100ns` on are skipped.
    /// `resyncs`, and #257's `resyncs_owned` when a real source's audio went
    /// missing there.
    pub(super) fn count_resync(&mut self, stamp_100ns: i64) {
        self.health.resyncs += 1;
        let shows = self.fill_source(stamp_100ns);
        self.health.resyncs_owned += u64::from(self.owned(stamp_100ns, shows));
    }

    /// #257: whether boundary `stamp_100ns`, showing `shows`, carries the
    /// audio of a real source that delivers ([`Self::delivering`]). In a
    /// running fade either side's: a fade INTO Blank shows Blank but still
    /// mixes the outgoing playlist (review round 1).
    fn owned(&self, stamp_100ns: i64, shows: Option<i64>) -> bool {
        match self.window_at(stamp_100ns) {
            Some(w) if w.cue == Cue::Open => self.delivering(Some(w.to)) || self.delivering(w.from),
            _ => self.delivering(shows),
        }
    }

    /// The window whose range covers boundary `stamp_100ns`, if any.
    pub(super) fn window_at(&self, stamp_100ns: i64) -> Option<Window> {
        self.windows.iter().find(|w| w.covers(stamp_100ns)).copied()
    }

    /// #147: the source a filled boundary shows. A boundary of a window
    /// whose cue waits or was frozen is held on its outgoing source (as
    /// `commit_held` names it); any other is its owner's (a running fade's
    /// is the incoming source, as a mix names it).
    pub(super) fn fill_source(&self, boundary_100ns: i64) -> Option<i64> {
        match self.window_at(boundary_100ns) {
            Some(w) if w.cue != Cue::Open => w.from,
            _ => self.owner_of(boundary_100ns),
        }
    }

    /// The source on program just before a cut on `boundary`: the OUTGOING
    /// source of a window that holds it there at full level
    /// (`Window::holds_on_air`: its cue waits or was frozen, and its span
    /// reaches `boundary`), else the selected source. Cut boundaries are not
    /// monotone: a source whose window was served no longer pushes a cut
    /// later, so the segment after a frozen window's end may be one this cut
    /// drops, and a window cut AFTER `boundary` is one this cut replaces —
    /// its outgoing source held nothing on program yet (review round 4).
    pub(super) fn on_air(&self, boundary_100ns: i64) -> Option<i64> {
        let holding = self.windows.iter().find(|w| w.holds_on_air(boundary_100ns));
        match holding {
            Some(window) => window.from,
            None => self.selected(),
        }
    }

    /// Release window boundary `expected` of `w` (see the module doc).
    /// `now_100ns` = the sender's wall, `None` on the clock-free offer path.
    pub(super) fn window_step(
        &mut self,
        w: &Window,
        expected: i64,
        now_100ns: Option<i64>,
    ) -> WindowStep {
        let to_here = self.pending.contains_key(&expected);
        let from_here = self.from_pending.contains_key(&expected);
        let to_done = to_here || self.source_missed(w.to, expected, now_100ns);
        let from_done = from_here
            || w.from
                .is_none_or(|f| self.source_missed(f, expected, now_100ns));
        let forced = self.overflowing();
        let deadline = match w.cue {
            Cue::Open => {
                if (to_here || from_here) && ((to_done && from_done) || forced) {
                    self.commit_mix(w, expected);
                    return WindowStep::Next;
                }
                return WindowStep::Missed(to_done && from_done);
            }
            Cue::Waiting { deadline_100ns } => Some(deadline_100ns),
            Cue::Frozen => None,
        };
        // Only a WAITING cue opens: a frozen one stays held to its end, even
        // when its incoming source goes live inside it (review round 1).
        let to_live = deadline.is_some() && self.pending.get(&expected).is_some_and(|job| job.live);
        let to_decided = to_done || forced;
        if to_live || (to_decided && deadline.is_some_and(|d| expected >= d)) {
            self.open_cue(expected, to_live);
            return WindowStep::Next;
        }
        if to_decided && (from_here || (to_here && (from_done || forced))) {
            self.commit_held(expected, w.from);
            return WindowStep::Next;
        }
        WindowStep::Missed(to_decided && from_done)
    }

    /// The cue of the waiting window at `at` opens there: the fade runs from
    /// `at` (see `Window::open`). The outgoing source's pairs past the window's
    /// new end are dropped (they belonged to the wait's worst case and would
    /// otherwise count toward the reorder bound forever). `live` = the
    /// incoming source's live pair opened it; else the wait ran out.
    fn open_cue(&mut self, at: i64, live: bool) {
        let Some(w) = self.windows.iter_mut().find(|w| w.covers(at)) else {
            return;
        };
        let latest_end = w.end_100ns;
        let waited = w.open(at);
        let (from, to, end) = (w.from, w.to, w.end_100ns);
        self.from_pending
            .retain(|&stamp, _| stamp < end || stamp >= latest_end);
        self.counters.cue_wait_boundaries = u64::from(waited);
        if live {
            info!(
                ?from,
                to, waited, "program transition: the incoming source is live — the fade starts"
            );
        } else {
            self.counters.cue_timeouts += 1;
            warn!(
                ?from,
                to,
                waited,
                max = CUE_WAIT_MAX_SLOTS,
                "program transition: the incoming source sent no live pair in time — the fade starts anyway"
            );
        }
    }

    /// A held window boundary: the outgoing source's own pair at full level,
    /// or the program's standby pair when it missed; the incoming side's pair
    /// (never on program) is dropped. Either shows `from` (#147).
    fn commit_held(&mut self, stamp: i64, from: Option<i64>) {
        self.pending.remove(&stamp);
        match self.from_pending.remove(&stamp) {
            Some(job) => {
                self.health.forwarded += 1;
                self.commit(ProgramJob::Source(job), from);
            }
            None => {
                self.count_fill(stamp, from);
                self.commit(ProgramJob::Standby { stamp_100ns: stamp }, from);
            }
        }
    }

    /// Queue window boundary `stamp` as one mixed pair (a side that is not
    /// here is the standby).
    fn commit_mix(&mut self, w: &Window, stamp: i64) {
        let to = self.pending.remove(&stamp);
        let from = self.from_pending.remove(&stamp);
        let side_missing = to.is_none() || (from.is_none() && w.from.is_some());
        // #257: a delivering source's side missing is its audio gone at its weight.
        let owned_missing = (to.is_none() && self.delivering(Some(w.to)))
            || (from.is_none() && self.delivering(w.from));
        self.counters.mixed_boundaries += 1;
        self.counters.side_fills += u64::from(side_missing);
        self.health.filled_owned += u64::from(owned_missing);
        let mix = MixJob {
            stamp_100ns: stamp,
            from,
            to,
            slot: w.slot(stamp).unwrap_or(0),
            n_slots: w.n_slots,
        };
        self.commit(ProgramJob::Mix(mix), Some(w.to)); // #147: shows the incoming side
    }
}

#[cfg(test)]
#[path = "program_bus_window_tests.rs"]
mod tests;
