//! #243 follow-up: the deploy's quiesce of the `program-max` thread, and the
//! wait for it at a normal shutdown. A child module of `program_max.rs` (it
//! reads the hand-off's private queue and stats).
//!
//! A kill that lands while SongPlayer holds one of Spout's locks leaves it
//! abandoned, and the receiver it is handed to (Arena) keeps Spout's sender
//! list for good (PP, 12.10.2026). So:
//!
//! - **The deploy's quiesce** (`POST /api/v1/program/spout/quiesce`,
//!   [`MaxOut::quiesce`]). Nothing is offered any more, and the thread's
//!   next step is `MaxNext::Quiesce`: it drops both senders, stops the list
//!   sentinel and makes no Spout call while the quiesce holds. It holds
//!   [`QUIESCE_EXPIRY`] from its last ask (an ask while it holds renews
//!   it). A deploy kills the process within seconds; one that does not (a
//!   run cancelled between the ask and the kill, a `taskkill` that failed)
//!   must not leave MAX dark until a restart. At the deadline, with the
//!   process still alive, the thread's step is `MaxNext::Resume`: one WARN,
//!   and the next job builds the senders again. `max.state` reads
//!   `quiesced` only in between.
//! - **The shutdown's wait** ([`stop_and_wait`], awaited by
//!   `sp_server::start` before it returns, which the tray's Exit awaits).
//!   The thread is stopped and the shutdown waits, at most
//!   [`MAX_STOP_WAIT`], until it has dropped both senders and ended: its
//!   stop path takes Spout's list lock (`ReleaseSender`), so the process
//!   must not end under it.
//!
//! Every decision takes its instant (`MaxOut::quiesce_at`,
//! `MaxOut::try_next_at`); `MaxOut::next` waits until the deadline by
//! itself.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::{MaxNext, MaxOut, MaxPhase, Queue};

/// How long a quiesce holds after its last ask: a deploy kills the process
/// within seconds of it.
pub const QUIESCE_EXPIRY: Duration = Duration::from_secs(60);

/// How long a shutdown waits for the `program-max` thread to drop its
/// senders and end.
pub const MAX_STOP_WAIT: Duration = Duration::from_secs(3);

impl Queue {
    /// The thread's step at `now` while a quiesce was asked until `until`:
    /// the quiesce itself, once (also when the thread sees the ask only
    /// after `until`: the senders are dropped either way); then nothing
    /// (the thread waits until `until`); from `until`, the resume, and the
    /// quiesce is over.
    pub(super) fn quiesce_step(&mut self, until: Instant, now: Instant) -> Option<MaxNext> {
        if !self.quiesce_done {
            self.quiesce_done = true;
            return Some(MaxNext::Quiesce);
        }
        if now >= until {
            self.quiesced_until = None;
            self.quiesce_done = false;
            return Some(MaxNext::Resume);
        }
        None
    }
}

impl MaxOut {
    /// The deploy's quiesce, asked now ([`quiesce_at`](Self::quiesce_at)).
    pub fn quiesce(&self) -> bool {
        self.quiesce_at(Instant::now())
    }

    /// Ask the quiesce at `now`. Until `now` + [`QUIESCE_EXPIRY`] nothing
    /// is offered and the thread quiesces once; an ask while one holds
    /// moves its end. Returns whether none held before.
    pub fn quiesce_at(&self, now: Instant) -> bool {
        let first = {
            let mut queue = self.lock_queue();
            let first = queue.quiesced_until.is_none();
            queue.quiesced_until = Some(now + queue.quiesce_expiry * 1_000);
            while queue.jobs.take().is_some() {}
            first
        };
        self.ready.notify_all();
        first
    }

    /// Tests: a quiesce holds `expiry` instead of [`QUIESCE_EXPIRY`].
    #[cfg(test)]
    pub(crate) fn set_quiesce_expiry(&self, expiry: Duration) {
        self.lock_queue().quiesce_expiry = expiry;
    }

    /// The thread quiesced: both senders are dropped and it makes no Spout
    /// call while the quiesce holds (MAX and the FHD sender read
    /// `quiesced`).
    pub fn record_quiesced(&self) {
        self.lock_stats().phase = MaxPhase::Quiesced;
        self.set_fhd_phase(MaxPhase::Quiesced);
    }

    /// The quiesce expired with the process alive: the thread serves again
    /// (the next job builds the senders).
    pub fn record_resumed(&self) {
        self.lock_stats().phase = MaxPhase::Running;
        self.set_fhd_phase(MaxPhase::Running);
    }

    /// Wait until no `program-max` thread takes jobs (it ends after it
    /// released every Spout object), at most `bound`; whether none does.
    pub fn wait_ended(&self, bound: Duration) -> bool {
        let queue = self.lock_queue();
        let (queue, _) = self
            .ended
            .wait_timeout_while(queue, bound, |queue| queue.consumer)
            .unwrap_or_else(|p| p.into_inner());
        !queue.consumer
    }
}

/// The shutdown: stop the `program-max` thread and wait, at most `bound`,
/// until it has dropped both senders and ended (one INFO; a WARN when it
/// did not end in time). Returns whether it ended.
pub async fn stop_and_wait(max: Arc<MaxOut>, bound: Duration) -> bool {
    max.stop();
    let waited = tokio::task::spawn_blocking(move || max.wait_ended(Duration::ZERO)).await;
    let ended = waited.unwrap_or(false);
    if ended {
        info!("program max: the thread ended before the process - no Spout lock is left held");
    } else {
        let bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        warn!(
            bound_ms,
            "program max: the thread did not end in time - the process ends anyway"
        );
    }
    ended
}

#[cfg(test)]
#[path = "program_max_quiesce_tests.rs"]
mod tests;
