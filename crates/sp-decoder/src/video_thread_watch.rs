//! A dropped video reader whose thread is still inside a read (#319).
//!
//! Dropping a [`ThreadedVideo`](super::ThreadedVideo) never waits for a
//! blocked read (#318): its thread ends once the read returns. A read that
//! NEVER returns (a hung driver call) would keep the thread, the reader's
//! file handle and, GPU-resident, its producer's ring slot for good, with no
//! trace. So the drop hands the thread's "done" signal to a watcher
//! ([`watch`]), which waits [`DETACH_WARN_AFTER`]: a thread that ended by
//! then is the normal case and says nothing; one still alive is reported
//! ONCE (a WARN naming the file) and counted in [`detached_readers`]
//! (`GET /api/v1/status` `video_decode.stuck_video_readers` /
//! `_now`). If its read ever returns, one INFO says so and `stuck_now`
//! drops again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use tracing::{info, warn};

/// How long a dropped reader's thread may still be inside its read before
/// it is reported (#319): far above any read SNV measured (0.5–0.75 s D3D11
/// stalls) and [`SEEK_WAIT`](super::SEEK_WAIT) (2 s), under the song's 15 s
/// stall bound.
pub const DETACH_WARN_AFTER: Duration = Duration::from_secs(10);

/// The readers whose thread outlived their owner (#319): process-wide in
/// [`detached_readers`]; a test builds its own.
#[derive(Debug, Default)]
pub struct DetachedReaders {
    stuck: AtomicU64,
    stuck_now: AtomicU64,
}

/// What [`DetachedReaders`] counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetachedCounts {
    /// Readers reported stuck since the process started.
    pub stuck: u64,
    /// Of those, the ones whose read has not returned yet.
    pub stuck_now: u64,
}

impl DetachedReaders {
    /// No reader counted.
    pub const fn new() -> Self {
        Self {
            stuck: AtomicU64::new(0),
            stuck_now: AtomicU64::new(0),
        }
    }

    /// The counts now. `stuck_now` is read first and both counts are
    /// `SeqCst` (the watcher adds to `stuck` before `stuck_now`), so a read
    /// never shows `stuck_now` above `stuck`.
    pub fn snapshot(&self) -> DetachedCounts {
        let stuck_now = self.stuck_now.load(Ordering::SeqCst);
        DetachedCounts {
            stuck: self.stuck.load(Ordering::SeqCst),
            stuck_now,
        }
    }
}

static DETACHED: DetachedReaders = DetachedReaders::new();

/// The process's [`DetachedReaders`] (every dropped reader's watcher counts
/// here).
pub fn detached_readers() -> &'static DetachedReaders {
    &DETACHED
}

/// What a dropped reader's thread did within the watcher's wait (#319).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detached {
    /// It ended (its "done" sender is gone): nothing to report.
    Ended,
    /// It is still alive: inside a read that has not returned.
    Stuck,
}

/// The verdict of the watcher's wait on the thread's "done" signal: only a
/// wait that ran out is a stuck thread (nothing is ever sent on it, so its
/// end is the disconnection).
pub(crate) fn detached_verdict(waited: Result<(), RecvTimeoutError>) -> Detached {
    match waited {
        Err(RecvTimeoutError::Timeout) => Detached::Stuck,
        Ok(()) | Err(RecvTimeoutError::Disconnected) => Detached::Ended,
    }
}

/// Watch a dropped reader's thread (#319): wait `after` for its end
/// (`done` disconnects); still alive = one WARN naming `file`, counted in
/// `counts`, then wait for the read to return (one INFO, `stuck_now` −1).
/// Returns the verdict of the first wait.
pub(crate) fn watch(
    done: &Receiver<()>,
    after: Duration,
    file: &str,
    counts: &DetachedReaders,
) -> Detached {
    let verdict = detached_verdict(done.recv_timeout(after));
    if verdict == Detached::Ended {
        return verdict;
    }
    counts.stuck.fetch_add(1, Ordering::SeqCst);
    counts.stuck_now.fetch_add(1, Ordering::SeqCst);
    warn!(
        file,
        after_ms = after.as_millis() as u64,
        "video reader: a dropped reader's thread is still inside a read - it keeps \
         its file (and, GPU-resident, one ring slot) until the read returns (#319)"
    );
    // Nothing is sent: this returns when the thread ends, if ever.
    let _ = done.recv();
    counts.stuck_now.fetch_sub(1, Ordering::SeqCst);
    info!(
        file,
        "video reader: a stuck reader's read returned - its thread ended (#319)"
    );
    verdict
}

#[cfg(test)]
#[path = "video_thread_watch_tests.rs"]
mod tests;
