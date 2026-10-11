//! #233: when SongPlayer starts the ASIO host again. A host that could not
//! be started (its exe missing, its pipe never connected) or whose session
//! ended is restarted after the ASIO outputs' own backoff
//! (`asio_state::backoff_100ns`: 2 / 10 / 30 / 60 / 60 … s), and a session
//! that ran 60 s resets it (`failures_after_close`), so a host that dies
//! once after hours is back in 2 s and one that cannot start is tried once
//! a minute.

use crate::playback::asio_state::{backoff_100ns, failures_after_close};

/// After a host that ran `ran_100ns` (0 when it never started) with
/// `failures` failures before it: the failures now and the wait before the
/// next start (100 ns).
pub fn after_host_end(failures: u32, ran_100ns: i64) -> (u32, i64) {
    let failures = failures_after_close(failures, ran_100ns);
    (failures, backoff_100ns(failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 10_000_000;

    /// A host that keeps failing at once waits 2, 10, 30, then 60 s for good.
    #[test]
    fn a_host_that_keeps_failing_waits_the_outputs_backoff() {
        let mut failures = 0;
        let mut waits = Vec::new();
        for _ in 0..6 {
            let (f, wait) = after_host_end(failures, 0);
            failures = f;
            waits.push(wait / S);
        }
        assert_eq!(waits, [2, 10, 30, 60, 60, 60]);
        assert_eq!(failures, 6);
    }

    /// A session of 60 s resets the backoff, one a tick shorter does not.
    #[test]
    fn a_session_of_a_minute_resets_the_backoff() {
        assert_eq!(after_host_end(4, 60 * S), (1, 2 * S));
        assert_eq!(after_host_end(4, 60 * S - 1), (5, 60 * S));
        assert_eq!(after_host_end(1, 59 * S), (2, 10 * S));
    }
}
