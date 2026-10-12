//! #243 follow-up: Spout's sender list kept by another program, and the
//! sentinel that wins it back (the design of #243's FINDING of 12.10.2026
//! ~12:58, parts 2 and 3; part 1, the deploy's quiesce, is
//! `program_max_quiesce.rs`).
//!
//! Spout 2.007.017 never releases a named mutex it was handed with
//! `WAIT_ABANDONED` (`gpu-max.md`). SongPlayer gives back what ITS threads
//! are handed, but when a program dies holding `SpoutSenderNames_mutex`
//! (SongPlayer at a deploy's `taskkill /F`, a crashed receiver), the next
//! waiter may be a RECEIVER, Arena, which then keeps the list for good:
//! every SongPlayer sender is refused `UNREADABLE` until that Arena exits
//! (PP, 12.10.2026: MAX and `SP-program` down after a deploy until a manual
//! rescue).
//!
//! - **Visible.** The `program-max` thread reads who holds the list after
//!   every boundary ([`SpoutList::holder`]: another program's thread, else
//!   none). The same program (pid) on every read for [`LIST_HELD_AFTER`]
//!   (one sender backoff, 3 s) is named: `max.list_holder {image, pid,
//!   since_s}` and one WARN. A healthy receiver holds the list for a read,
//!   never for seconds. That program is the one to restart.
//! - **Won back.** At that moment a sentinel starts, one at a time
//!   ([`SpoutList::start_sentinel`], sp-gpu's `ListSentinel`): a thread
//!   queued on the list's lock. When the holder exits, Windows hands the
//!   lock to its next waiter — the sentinel, queued before any program
//!   started later (a relaunched Arena) — which gives it back at once and
//!   ends (one INFO). The next sender, after its backoff, reads the list
//!   again: the recovery after an Arena restart no longer races the new
//!   Arena for the abandoned lock.
//!
//! Every decision is [`ListWatch`] (pure); the reads and the sentinel are
//! sp-gpu's, behind [`SpoutList`], so the tests drive a fake. The thread's
//! part ([`ListSide`]) also stops the sentinel when it quiesces.

use std::time::{Duration, Instant};

use serde::Serialize;
use sp_gpu::{GpuError, LockHolder, SentinelEnd};
use tracing::{debug, info, warn};

use crate::playback::program_max::MaxOut;
use crate::playback::program_max_worker::MAX_RETRY_BACKOFF;

/// How long one program must keep Spout's list, on every read, before it
/// is named and the sentinel starts: one sender backoff
/// (`MAX_RETRY_BACKOFF`), the time a refused sender waits anyway.
pub const LIST_HELD_AFTER: Duration = MAX_RETRY_BACKOFF;

/// `GET /api/v1/program` → `max.list_holder`: the program that keeps
/// Spout's sender list, once it kept it [`LIST_HELD_AFTER`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListHolderStatus {
    /// Its program's file name (`Arena.exe`), `None` when it cannot be read.
    pub image: Option<String>,
    pub pid: u32,
    /// How long it has held the list, whole seconds (from the first read
    /// that saw it).
    pub since_s: u64,
}

/// How the `program-max` thread reads Spout's list and waits on it.
pub trait SpoutList {
    /// The program holding Spout's list now: another thread's, `None` when
    /// it is free, the reading thread's own, or not readable.
    fn holder(&mut self) -> Option<LockHolder>;
    /// Start the sentinel on the list's lock.
    fn start_sentinel(&mut self) -> Result<(), GpuError>;
    /// How the started sentinel ended, once (`None`: none started, or it
    /// still waits).
    fn sentinel_ended(&mut self) -> Option<SentinelEnd>;
    /// Stop the sentinel and wait until it ended (its stop event ends its
    /// one wait at once).
    fn stop_sentinel(&mut self);
}

/// The production list: sp-gpu's reader and its `ListSentinel` (off
/// Windows, unsupported: no holder, no sentinel).
#[derive(Default)]
pub struct SpoutListLock {
    sentinel: Option<sp_gpu::ListSentinel>,
}

impl SpoutList for SpoutListLock {
    /// `mutants::skip`: one call; off Windows (the mutation runner) the
    /// reader is unsupported, read as `None` either way. The read is proven
    /// on the Windows job by sp-gpu `tests/spout_list.rs` (no GPU needed).
    #[cfg_attr(test, mutants::skip)]
    fn holder(&mut self) -> Option<LockHolder> {
        sp_gpu::spout_list_holder().ok().flatten()
    }

    fn start_sentinel(&mut self) -> Result<(), GpuError> {
        self.sentinel = Some(sp_gpu::ListSentinel::start()?);
        Ok(())
    }

    /// `mutants::skip`: off Windows no sentinel can start, so there is
    /// never one to ask; the sentinel is proven on the Windows job by sp-gpu
    /// `tests/spout_list.rs`.
    #[cfg_attr(test, mutants::skip)]
    fn sentinel_ended(&mut self) -> Option<SentinelEnd> {
        let ended = self.sentinel.as_ref()?.ended();
        if ended.is_some() {
            self.sentinel = None;
        }
        ended
    }

    /// `mutants::skip`: as `sentinel_ended`.
    #[cfg_attr(test, mutants::skip)]
    fn stop_sentinel(&mut self) {
        if let Some(mut sentinel) = self.sentinel.take() {
            sentinel.stop();
        }
    }
}

/// One program seen holding the list on every read since `since`.
#[derive(Debug)]
struct Held {
    holder: LockHolder,
    since: Instant,
    /// It was named (one WARN; `max.list_holder` shows it).
    named: bool,
}

/// What a read names ([`ListWatch::observe`]): the holder (one WARN) and
/// whether the sentinel starts with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    pub holder: LockHolder,
    pub start_sentinel: bool,
}

/// Who keeps Spout's list and since when, and whether a sentinel waits
/// (pure: the thread hands it each read).
#[derive(Debug)]
pub struct ListWatch {
    own_pid: u32,
    held: Option<Held>,
    sentinel: bool,
}

impl ListWatch {
    /// Nothing read yet. `own_pid` = this process: a thread of its own is
    /// no other program.
    pub fn new(own_pid: u32) -> Self {
        Self {
            own_pid,
            held: None,
            sentinel: false,
        }
    }

    /// The read at `now`: `holder` = who holds the list (`None`: nobody,
    /// or it could not be read). A program seen on every read for
    /// [`LIST_HELD_AFTER`] is named once, until it lets go; the sentinel
    /// starts with it unless one already waits. A free read, this process,
    /// or another program starts the count again.
    pub fn observe(&mut self, now: Instant, holder: Option<LockHolder>) -> Option<Named> {
        let Some(holder) = holder.filter(|holder| holder.pid != self.own_pid) else {
            self.held = None;
            return None;
        };
        let mut held = match self.held.take() {
            Some(held) if held.holder.pid == holder.pid => held,
            _ => Held {
                holder,
                since: now,
                named: false,
            },
        };
        let mut named = None;
        if !held.named && now.saturating_duration_since(held.since) >= LIST_HELD_AFTER {
            held.named = true;
            named = Some(Named {
                holder: held.holder.clone(),
                start_sentinel: !self.sentinel,
            });
            self.sentinel = true;
        }
        self.held = Some(held);
        named
    }

    /// `max.list_holder` at `now`: the named holder and how long it has
    /// held the list; `None` before it is named.
    pub fn shown(&self, now: Instant) -> Option<ListHolderStatus> {
        let held = self.held.as_ref().filter(|held| held.named)?;
        Some(ListHolderStatus {
            image: held.holder.image.clone(),
            pid: held.holder.pid,
            since_s: now.saturating_duration_since(held.since).as_secs(),
        })
    }

    /// The sentinel ended (or did not start): the next naming starts one.
    pub fn sentinel_ended(&mut self) {
        self.sentinel = false;
    }

    /// Nothing is read for a while (MAX off) or any more (quiesced): forget
    /// the holder. A waiting sentinel stays counted.
    pub fn forget(&mut self) {
        self.held = None;
    }
}

/// The `program-max` thread's watch over Spout's list: a read after each
/// boundary, the sentinel, `max.list_holder` and their log lines.
pub struct ListSide {
    list: Option<Box<dyn SpoutList>>,
    watch: ListWatch,
}

impl ListSide {
    /// `list` = how the list is read and waited on (`None`: not at all, a
    /// worker of the tests); `own_pid` = this process.
    pub fn new(list: Option<Box<dyn SpoutList>>, own_pid: u32) -> Self {
        Self {
            list,
            watch: ListWatch::new(own_pid),
        }
    }

    /// After a boundary at `now`: take a finished sentinel's end, read
    /// who holds the list, name a program that kept it [`LIST_HELD_AFTER`]
    /// (and start the sentinel), and record `max.list_holder`.
    pub fn boundary(&mut self, now: Instant, out: &MaxOut) {
        let Some(list) = self.list.as_mut() else {
            return;
        };
        if let Some(end) = list.sentinel_ended() {
            self.watch.sentinel_ended();
            log_sentinel_end(&end);
        }
        if let Some(named) = self.watch.observe(now, list.holder()) {
            let started = named.start_sentinel.then(|| list.start_sentinel());
            if started.as_ref().is_some_and(Result::is_err) {
                self.watch.sentinel_ended();
            }
            log_named(&named.holder, started.as_ref());
        }
        out.record_list_holder(self.watch.shown(now));
    }

    /// MAX is off, or the thread ends: the holder is forgotten
    /// (`max.list_holder` null); a waiting sentinel keeps waiting.
    pub fn forget(&mut self, out: &MaxOut) {
        self.watch.forget();
        out.record_list_holder(None);
    }

    /// The thread quiesces: stop the sentinel (no wait on a Spout lock any
    /// more) and forget the holder.
    pub fn stop(&mut self, out: &MaxOut) {
        if let Some(list) = self.list.as_mut() {
            list.stop_sentinel();
        }
        self.forget(out);
    }
}

/// The one WARN naming the program that keeps Spout's list.
/// `mutants::skip`: it only writes the log line ([`ListWatch`] decides).
#[cfg_attr(test, mutants::skip)]
fn log_named(holder: &LockHolder, sentinel: Option<&Result<(), GpuError>>) {
    let program = holder.image.as_deref().unwrap_or("?");
    let sentinel = match sentinel {
        Some(Ok(())) => "a sentinel waits to give it back when that program exits".to_string(),
        Some(Err(e)) => format!("the sentinel did not start: {e}"),
        None => "a sentinel already waits".to_string(),
    };
    warn!(
        program,
        pid = holder.pid,
        held_at_least_s = LIST_HELD_AFTER.as_secs(),
        sentinel = %sentinel,
        "program max: another program keeps Spout's sender list - no SongPlayer sender can register until it lets go; restarting that program frees it"
    );
}

/// The one INFO when the sentinel gave the list back (abandoned or not).
/// `mutants::skip`: it only writes the log line.
#[cfg_attr(test, mutants::skip)]
fn log_sentinel_end(end: &SentinelEnd) {
    match end {
        SentinelEnd::GivenBack { abandoned } => info!(
            abandoned,
            "program max: the sentinel was handed Spout's sender list and gave it back at once"
        ),
        SentinelEnd::Stopped => debug!("program max: the Spout list sentinel stopped"),
        SentinelEnd::Failed(why) => {
            warn!(%why, "program max: the Spout list sentinel ended without the lock");
        }
    }
}

#[cfg(test)]
#[path = "program_max_list_tests.rs"]
pub(crate) mod tests;
