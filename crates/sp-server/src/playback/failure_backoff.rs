//! #229: the pause after videos that fail to open in a row (pure,
//! Linux-tested, mutation-gated).
//!
//! When every file of a playlist fails to open (a box with no VP9/AV1
//! decoder, an offline cache disk, an ACL change), each failure used to
//! select the next song at once: ~6 songs a second at PP, 675 errors in
//! ~35 s. The engine now counts the failures of a playlist in a row
//! ([`FailureRun`]) and asks [`next_attempt`] how long to wait before the
//! next song. A song that starts ends the run. The timer, its id and the
//! resets live with the engine (`failure_retry.rs`).
//!
//! A song is recorded as played only when it starts (#229), so a song that
//! cannot be opened stays "unplayed": the selection must leave it out, or at
//! the end of a rotation it is the one song left and is picked for good
//! ([`pick_pool`], fed by [`FailureRun::avoid`]).
//!
//! A pipeline's `Started` and `Error` name no Play, so only the answer to
//! the LAST Play sent records a play or counts a failure ([`PlayAnswers`]).

use std::collections::BTreeSet;
use std::time::Duration;

use sp_core::playback::OpenFailures;

/// The first failure in a row that waits before the next attempt: one or
/// two bad files must not stall a playlist.
const FIRST_PAUSED: u32 = 3;

/// The pauses from [`FIRST_PAUSED`] on, in seconds; the last one repeats.
const PAUSES_S: [u64; 4] = [5, 30, 120, 300];

/// The pause before the next attempt after `consecutive` failed opens in a
/// row: `None` = select the next song at once (failures 1 and 2), then
/// 5 s, 30 s, 120 s, and 300 s for every later one.
pub fn next_attempt(consecutive: u32) -> Option<Duration> {
    let step = usize::try_from(consecutive.checked_sub(FIRST_PAUSED)?).ok()?;
    let secs = PAUSES_S[step.min(PAUSES_S.len() - 1)];
    Some(Duration::from_secs(secs))
}

/// The UTC instant (ms since the epoch) `wait` after `now_utc_ms`: when a
/// pending retry is due, for the health row (`retry_at_ms`).
pub fn utc_ms_after(now_utc_ms: i64, wait: Duration) -> i64 {
    now_utc_ms.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX))
}

/// Where a youtube playlist's next random pick comes from (#229).
#[derive(Debug, PartialEq, Eq)]
pub enum PickPool {
    /// Pick among these unplayed songs.
    Unplayed(Vec<i64>),
    /// The rotation restarts: clear the play history, then pick among these
    /// (none = nothing to play).
    Restart(Vec<i64>),
}

/// The songs a youtube playlist's next pick takes from: its `unplayed`
/// songs minus `avoid` (the song just sent, and the songs that failed to
/// open since the last start, [`FailureRun::avoid`]). With none left, the
/// rotation restarts from `all` its songs minus `avoid`. Only when every
/// song is to be avoided (a one-song playlist, or every open failed) does it
/// pick as before #229: the unplayed songs, else a restart from all of them;
/// the pause after failed opens paces those attempts.
pub fn pick_pool(unplayed: &[i64], all: &[i64], avoid: &[i64]) -> PickPool {
    let fresh = without(unplayed, avoid);
    if !fresh.is_empty() {
        return PickPool::Unplayed(fresh);
    }
    let rest = without(all, avoid);
    if !rest.is_empty() {
        return PickPool::Restart(rest);
    }
    if !unplayed.is_empty() {
        return PickPool::Unplayed(unplayed.to_vec());
    }
    PickPool::Restart(all.to_vec())
}

/// `songs` without the ones in `avoid`, in order.
fn without(songs: &[i64], avoid: &[i64]) -> Vec<i64> {
    songs
        .iter()
        .copied()
        .filter(|song| !avoid.contains(song))
        .collect()
}

/// A playlist's failed opens in a row since its last song started.
#[derive(Debug, Default)]
pub struct FailureRun {
    /// How many Plays failed in a row.
    consecutive_failures: u32,
    /// The last one's error; `None` while the run is empty.
    last_failure: Option<String>,
    /// The songs that failed in this run (the selection leaves them out).
    failed: BTreeSet<i64>,
}

impl FailureRun {
    /// One more failed open, with its `error`: the pause before the next
    /// attempt ([`next_attempt`]); `None` = at once.
    pub fn fail(&mut self, error: &str) -> Option<Duration> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure = Some(error.to_owned());
        next_attempt(self.consecutive_failures)
    }

    /// Failed opens in a row.
    pub fn count(&self) -> u32 {
        self.consecutive_failures
    }

    /// The song whose open failed (the playlist's current one), left out of
    /// the selections until the run ends.
    pub fn note_failed(&mut self, video_id: Option<i64>) {
        self.failed.extend(video_id);
    }

    /// The songs the next selection leaves out: every song that failed in
    /// this run, and the song just sent (`current`), which is not recorded as
    /// played before it starts.
    pub fn avoid(&self, current: Option<i64>) -> Vec<i64> {
        self.failed.iter().copied().chain(current).collect()
    }

    /// A song started: the run is over. Returns what ended (`None` = no
    /// open had failed), for the log.
    pub fn reset(&mut self) -> Option<OpenFailures> {
        std::mem::take(self).view(None)
    }

    /// The health row's `open_failures`, with the pending `retry` (its due
    /// instant, `retry_at_ms`, and whether it belongs to SP-program's
    /// source); `None` while no open failed. With no retry pending the row
    /// claims no program (`on_program` false).
    pub fn view(&self, retry: Option<RetryView>) -> Option<OpenFailures> {
        let last_error = self.last_failure.clone()?;
        Some(OpenFailures {
            count: self.consecutive_failures,
            last_error,
            retry_at_ms: retry.map(|retry| retry.at_ms),
            retry_in_ms: None, // filled at the read (`NdiHealthRegistry::snapshots`)
            on_program: retry.is_some_and(|retry| retry.on_program),
        })
    }
}

/// A pending retry as the health row tells it (#229 follow-up, ROZHODNUTÉ
/// 6029773698).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryView {
    /// When it is due (UTC ms since the epoch).
    pub at_ms: i64,
    /// It belongs to SP-program's source (`PendingRetry`'s `on_program`).
    pub on_program: bool,
}

/// The Plays a playlist's pipeline was sent and has not answered yet (#229
/// follow-up, design record 6029071745). The pipeline answers every Play
/// exactly once and in order: `Started` when its song opened, `Error` when it
/// did not (a Play's pre-roll reads no command, so a later Play waits for the
/// earlier one's answer). Neither event names its Play, so after a quick
/// Play → Play (a skip or a pick in the first song's pre-roll) the first
/// song's answer can come after the second Play went out. Only the answer
/// that brings the count to 0 answers the LAST Play sent: only that one
/// records a play, ends the run of failed opens or counts a failure.
#[derive(Debug, Default)]
pub struct PlayAnswers {
    /// Plays sent and not answered yet.
    unanswered: u32,
}

impl PlayAnswers {
    /// A Play was sent.
    pub fn sent(&mut self) {
        self.unanswered = self.unanswered.saturating_add(1);
    }

    /// A `Started` or an `Error` came: whether it answers the last Play sent.
    /// An answer with no Play pending answers the last one too, and the count
    /// stays 0 (the pipeline never answers more than it was sent; a test may
    /// inject one).
    pub fn answered(&mut self) -> bool {
        self.unanswered = self.unanswered.saturating_sub(1);
        self.unanswered == 0
    }

    /// The Plays still waiting for their answer, for the log of an ignored
    /// answer: a count that never comes back to 0 means a Play that was
    /// never answered (the pipeline's one-answer rule broken).
    pub fn pending(&self) -> u32 {
        self.unanswered
    }
}

#[cfg(test)]
#[path = "failure_backoff_tests.rs"]
mod tests;
