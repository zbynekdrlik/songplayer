//! SongPlayer's record of what it TOLD cg OBS to show (#221 L4a, design
//! record 5873773896 §1e). Deleted at B4 step 6, together with the legacy
//! mirror.
//!
//! Until B4 step 6 the legacy consumers (Arena, FOH, lv1, strih) still take
//! cg OBS's program, and SongPlayer drives it: a playlist switch is mirrored
//! to cg OBS, a manual scene is forwarded to it first. This is SongPlayer's
//! record of its OWN commands, never cg OBS tracking (the owner's ruling: no
//! more work goes into following cg OBS):
//!
//! - cg OBS answered a playlist mirror OK → `shown = Some(pid)`;
//! - cg OBS answered a manual scene OK → `shown = None` (no playlist);
//! - a refusal, no answer, or a command never sent → unchanged.
//!
//! Every command takes a [`Ticket`] under the bus's `switch_order` (so
//! tickets are in switch order); an answer applies only when its ticket is
//! newer than every answer applied before, so a late answer to an older
//! command never overwrites a newer one's.
//!
//! At startup `shown` is the restored program source when it is a playlist
//! (`restore_selected_source`). It is served as `legacy_cg.shown` on
//! `GET /api/v1/program` and keys the dark-wall expectation
//! (`ndi_health_expect`): a receiver is expected on a playlist's NDI output
//! only while cg OBS was told to show it.
//!
//! It also holds the dashboard's way to cg OBS ([`LegacyCg::link`]): the
//! OBS client's command channel, attached by `start_program`. `AppState` is
//! built before the OBS client exists, and the dashboard needs cg OBS only
//! for the mirror, which B4 step 6 deletes with this record.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde::Serialize;
use sp_core::config::PROGRAM_INPUT_ID;
use tokio::sync::watch;

use crate::remote::Upstream;

/// A command's place in the switch order, taken when it is sent to cg OBS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket(u64);

/// The ticket counters.
#[derive(Debug, Default)]
struct Tickets {
    /// The last ticket handed out.
    issued: u64,
    /// The ticket of the last answer applied to `shown` (0 = none yet).
    applied: u64,
}

/// The `legacy_cg` block of `GET /api/v1/program`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LegacyCgStatus {
    /// The playlist SongPlayer last told cg OBS to show (and cg OBS
    /// accepted); `null` for a manual scene, or before anything was told.
    pub shown: Option<i64>,
}

/// SongPlayer's record of what it told cg OBS (the module doc). Lives on the
/// program bus (`ProgramBus::legacy_cg`).
pub struct LegacyCg {
    shown: watch::Sender<Option<i64>>,
    tickets: Mutex<Tickets>,
    link: OnceLock<Upstream>,
}

impl Default for LegacyCg {
    fn default() -> Self {
        Self {
            shown: watch::channel(None).0,
            tickets: Mutex::new(Tickets::default()),
            link: OnceLock::new(),
        }
    }
}

impl LegacyCg {
    fn tickets(&self) -> MutexGuard<'_, Tickets> {
        self.tickets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next command's ticket. Taken under the bus's `switch_order`, so
    /// tickets follow the switch order.
    pub fn ticket(&self) -> Ticket {
        let mut tickets = self.tickets();
        tickets.issued += 1;
        Ticket(tickets.issued)
    }

    /// cg OBS answered command `ticket` OK and now shows `shown` (a
    /// playlist's scene → `Some(pid)`, a manual scene → `None`). Applied only
    /// when `ticket` is newer than every answer applied before; returns
    /// whether it was.
    pub fn confirmed(&self, ticket: Ticket, shown: Option<i64>) -> bool {
        let mut tickets = self.tickets();
        tickets.applied = tickets.applied.max(ticket.0);
        self.shown.send_replace(shown);
        true
    }

    /// Startup: the restored program `source` is what cg OBS was last told,
    /// when it is a playlist (the NDI input "OBS manuál" names no playlist).
    /// Always published with `send_replace`: nobody subscribed yet.
    pub fn restored(&self, source: i64) {
        if source != PROGRAM_INPUT_ID {
            self.shown.send_replace(Some(source));
        }
    }

    /// The playlist cg OBS was last told to show, now.
    pub fn shown_now(&self) -> Option<i64> {
        *self.shown.borrow()
    }

    /// A receiver of `shown` (#221 L4b's playback authority).
    pub fn shown(&self) -> watch::Receiver<Option<i64>> {
        self.shown.subscribe()
    }

    /// The `legacy_cg` block of `GET /api/v1/program`.
    pub fn status(&self) -> LegacyCgStatus {
        LegacyCgStatus {
            shown: self.shown_now(),
        }
    }

    /// Attach the dashboard's link to cg OBS (once, from `start_program`);
    /// `false` when one is already attached (the first one stays).
    pub fn attach(&self, link: Upstream) -> bool {
        self.link.set(link).is_ok()
    }

    /// The dashboard's link to cg OBS: the attached one, else an unlinked one
    /// (before `start_program`, and in tests), whose calls never reach cg OBS.
    pub fn link(&self) -> Upstream {
        self.link.get().cloned().unwrap_or_else(Upstream::unlinked)
    }
}

#[cfg(test)]
#[path = "legacy_cg_tests.rs"]
mod tests;
