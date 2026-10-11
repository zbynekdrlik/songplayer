//! #233 (PP's Waves SoundGrid output pops): what an ASIO driver tells the
//! worker besides its callbacks, a child module of `asio_out.rs` (its
//! 1000-line cap).
//!
//! A resync request (`kAsioResyncRequest`, answered "supported" by
//! `asio_state::reply`): the driver's sample position jumped — a lost or a
//! repeated buffer, a re-locked clock. This build MEASURES it only: every
//! resync is counted (`outputs[i].asio.resyncs`) and WARNed, at most once
//! per 5 s with the number held back; the drift servo, the ring and the
//! driver run on untouched (no rebase, no priming, never a close and a
//! reopen — a SoundGrid driver takes about 10 s to open again). What to do
//! about one is decided on PP's measured evidence (#233).
//!
//! The driver's callback timing (the glue times every buffer switch,
//! `asio_state::callback_gap_kind`): the longest gap between two since the
//! output was built, the late and the fast ones counted across runs, and
//! the last late one with its age, so an operator can line it up with a
//! pop (`outputs[i].asio.callback_gap_max_ms`, `late_callbacks`,
//! `fast_callbacks`, `last_late_callback`).

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{AsioOut, AsioStatus, Run};
use crate::playback::asio_state::DeviceEvents;
use crate::playback::stat_window::WarnLimiter;

/// The last late buffer switch as the status shows it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LateCallback {
    /// The longest gap between two buffer switches in the poll that saw it,
    /// ms.
    pub gap_ms: f64,
    /// How long ago, s (as of the status's last update).
    pub ago_s: f64,
}

/// The driver's callback timing since the output was built.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CallbackTiming {
    gap_max_100ns: i64,
    /// The poll's longest gap and its instant, of the last poll that saw a
    /// new late buffer switch.
    last_late: Option<(i64, i64)>,
}

impl CallbackTiming {
    /// One poll of a run whose driver had counted `late_before` late buffer
    /// switches at the poll before.
    pub(super) fn observe(&mut self, late_before: u64, ev: &DeviceEvents, now_100ns: i64) {
        self.gap_max_100ns = self.gap_max_100ns.max(ev.callback_gap_max_100ns);
        if ev.late_callbacks > late_before {
            self.last_late = Some((ev.callback_gap_max_100ns, now_100ns));
        }
    }

    /// Into the status, the last late buffer switch aged to `now_100ns`.
    pub(super) fn write(&self, s: &mut AsioStatus, now_100ns: i64) {
        s.callback_gap_max_ms = self.gap_max_100ns as f64 / 10_000.0;
        s.last_late_callback = self.last_late.map(|(gap, at)| LateCallback {
            gap_ms: gap as f64 / 10_000.0,
            ago_s: (now_100ns - at) as f64 / 1e7,
        });
    }
}

impl Run {
    /// The driver's per-run counters of one poll (its device counts per
    /// run; `Closed` adds up the closed runs').
    pub(super) fn count(&mut self, ev: &DeviceEvents) {
        self.overloads = ev.overloads;
        self.late_callbacks = ev.late_callbacks;
        self.fast_callbacks = ev.fast_callbacks;
    }
}

/// At most one resync WARN per this much of the wall (5 s, 100 ns).
const RESYNC_WARN_EVERY_100NS: i64 = 50_000_000;

/// The driver's resync requests since the output was built, and their
/// WARN's limit.
#[derive(Default)]
pub(super) struct Resyncs {
    count: u64,
    warns: WarnLimiter,
}

impl Resyncs {
    /// A resync request of `out`'s driver at `now_100ns` (see the module
    /// doc): counted and shown, a WARN unless one went out in the last 5 s.
    /// Nothing else changes: the servo, the ring and the driver run on.
    pub(super) fn note(&mut self, out: &AsioOut, now_100ns: i64) {
        self.count += 1;
        let count = self.count;
        if let Some(held_back) = self.warns.admit(now_100ns, RESYNC_WARN_EVERY_100NS) {
            log_resync(&out.id, &out.driver, count, held_back);
        }
        out.update(|l| l.status.resyncs = count);
    }
}

/// A resync's WARN (`held_back`: those since the last WARN).
#[cfg_attr(test, mutants::skip)] // logging only; the decision is WarnLimiter's (tested)
fn log_resync(id: &str, driver: &str, resyncs: u64, held_back: u64) {
    warn!(
        id,
        driver,
        resyncs,
        held_back,
        "asio output: the driver asked for a resync (kAsioResyncRequest: its sample position jumped) - counted only; the drift servo and the driver run on"
    );
}
