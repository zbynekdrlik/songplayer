//! #229: the pause table after failed opens in a row, and the run's
//! bookkeeping (`failure_backoff.rs`).

use std::time::Duration;

use sp_core::playback::OpenFailures;

use super::{FailureRun, next_attempt, utc_ms_after};

fn secs(s: u64) -> Option<Duration> {
    Some(Duration::from_secs(s))
}

/// The design record's table, with its exact boundaries: the 2nd failure
/// still selects at once, the 3rd waits 5 s, then 30 s and 120 s, and
/// every one from the 6th waits the 300 s cap (the 7th is the first past
/// the table's last entry).
#[test]
fn the_pause_table_has_its_exact_boundaries() {
    assert_eq!(next_attempt(0), None, "no failure: no pause");
    assert_eq!(next_attempt(1), None, "one bad file: the next song at once");
    assert_eq!(next_attempt(2), None, "the 2nd failure: still at once");
    assert_eq!(next_attempt(3), secs(5), "the 3rd failure waits 5 s");
    assert_eq!(next_attempt(4), secs(30));
    assert_eq!(next_attempt(5), secs(120));
    assert_eq!(next_attempt(6), secs(300), "the 6th reaches the cap");
    assert_eq!(next_attempt(7), secs(300), "the cap repeats");
    assert_eq!(next_attempt(100), secs(300));
    assert_eq!(next_attempt(u32::MAX), secs(300));
}

#[test]
fn a_retry_is_due_its_wait_after_now_in_utc_ms() {
    assert_eq!(utc_ms_after(1_000, Duration::from_secs(5)), 6_000);
    assert_eq!(utc_ms_after(1_000, Duration::from_millis(1_500)), 2_500);
    assert_eq!(utc_ms_after(1_000, Duration::ZERO), 1_000);
    assert_eq!(
        utc_ms_after(i64::MAX - 1, Duration::from_secs(1)),
        i64::MAX,
        "saturates instead of wrapping"
    );
}

/// The run counts every failure, keeps the last error and answers each
/// failure with the table's pause for its count.
#[test]
fn a_run_counts_the_failures_and_keeps_the_last_error() {
    let mut run = FailureRun::default();
    assert_eq!(run.count(), 0);
    assert_eq!(run.view(None), None, "null while no open failed");

    assert_eq!(run.fail("first"), None);
    assert_eq!(run.fail("second"), None);
    assert_eq!(run.fail("No suitable transform"), secs(5));
    assert_eq!(run.fail("No suitable transform"), secs(30));
    assert_eq!(run.count(), 4);
    assert_eq!(
        run.view(Some(1_234)),
        Some(OpenFailures {
            count: 4,
            last_error: "No suitable transform".into(),
            retry_at_ms: Some(1_234),
        })
    );
    assert_eq!(
        run.view(None).and_then(|v| v.retry_at_ms),
        None,
        "no retry pending: no due instant"
    );
}

/// A started song ends the run: it reports what ended, and the next
/// failure counts from one again (at once, no pause).
#[test]
fn a_reset_ends_the_run_and_the_next_failure_counts_from_one() {
    let mut run = FailureRun::default();
    for _ in 0..3 {
        run.fail("broken");
    }
    assert_eq!(
        run.reset(),
        Some(OpenFailures {
            count: 3,
            last_error: "broken".into(),
            retry_at_ms: None,
        }),
        "the reset reports the run that ended"
    );
    assert_eq!(run.count(), 0);
    assert_eq!(run.view(None), None, "null again after a start");
    assert_eq!(run.reset(), None, "an empty run ends nothing");
    assert_eq!(
        run.fail("again"),
        None,
        "counted from one: the next song at once"
    );
    assert_eq!(run.count(), 1);
}
