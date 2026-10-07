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

/// A playlist's failed opens in a row since its last song started.
#[derive(Debug, Default)]
pub struct FailureRun {
    /// How many Plays failed in a row.
    consecutive_failures: u32,
    /// The last one's error; `None` while the run is empty.
    last_failure: Option<String>,
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

    /// A song started: the run is over. Returns what ended (`None` = no
    /// open had failed), for the log.
    pub fn reset(&mut self) -> Option<OpenFailures> {
        std::mem::take(self).view(None)
    }

    /// The health row's `open_failures`, with the pending retry's due
    /// instant (`retry_at_ms`); `None` while no open failed.
    pub fn view(&self, retry_at_ms: Option<i64>) -> Option<OpenFailures> {
        let last_error = self.last_failure.clone()?;
        Some(OpenFailures {
            count: self.consecutive_failures,
            last_error,
            retry_at_ms,
        })
    }
}

#[cfg(test)]
#[path = "failure_backoff_tests.rs"]
mod tests;
