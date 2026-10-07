//! #229: the pause table after failed opens in a row, and the run's
//! bookkeeping (`failure_backoff.rs`).

use std::time::Duration;

use sp_core::playback::OpenFailures;

use super::{FailureRun, PlayAnswers, next_attempt, utc_ms_after};

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
            retry_in_ms: None,
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
            retry_in_ms: None,
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

use super::{PickPool, pick_pool};

/// The review of the lane: a song recorded only once it starts must not be
/// picked again while another song can be. The pick leaves out the songs to
/// avoid; with only those left unplayed, the rotation restarts without them;
/// only when every song is to be avoided does it pick as before.
#[test]
fn the_pick_leaves_out_the_songs_to_avoid() {
    assert_eq!(
        pick_pool(&[11, 12, 13], &[11, 12, 13, 14], &[12]),
        PickPool::Unplayed(vec![11, 13]),
        "the unplayed songs that are not avoided, in order"
    );
    assert_eq!(
        pick_pool(&[11, 13], &[11, 12, 13], &[]),
        PickPool::Unplayed(vec![11, 13]),
        "nothing to avoid: every unplayed song"
    );
    assert_eq!(
        pick_pool(&[12], &[11, 12, 13], &[12]),
        PickPool::Restart(vec![11, 13]),
        "only an avoided song unplayed: the rotation restarts without it"
    );
    assert_eq!(
        pick_pool(&[], &[11, 12], &[12]),
        PickPool::Restart(vec![11]),
        "every song played: a restart, without the song just sent"
    );
    assert_eq!(
        pick_pool(&[12], &[11, 12], &[11, 12]),
        PickPool::Unplayed(vec![12]),
        "every song avoided: the unplayed ones, as before"
    );
    assert_eq!(
        pick_pool(&[], &[11, 12], &[12, 11]),
        PickPool::Restart(vec![11, 12]),
        "every song avoided and played: a restart from all of them"
    );
    assert_eq!(
        pick_pool(&[], &[], &[]),
        PickPool::Restart(vec![]),
        "no song: nothing to pick"
    );
}

/// The run leaves out every song that failed in it, once each, and the song
/// just sent; a start ends the list.
#[test]
fn a_run_avoids_its_failed_songs_and_the_song_just_sent() {
    let mut run = FailureRun::default();
    assert_eq!(run.avoid(None), Vec::<i64>::new());
    assert_eq!(run.avoid(Some(7)), vec![7]);
    run.note_failed(Some(9));
    run.note_failed(Some(3));
    run.note_failed(Some(9));
    run.note_failed(None);
    assert_eq!(run.avoid(Some(7)), vec![3, 9, 7]);
    assert_eq!(run.avoid(None), vec![3, 9]);
    run.fail("x");
    run.reset();
    assert_eq!(run.avoid(Some(7)), vec![7], "a start ends the run's list");
}

/// #229 follow-up (design record 6029071745): a `Started` or an `Error`
/// names no Play, and the pipeline answers its Plays one by one, in order.
/// Only the answer that brings the count of unanswered Plays to 0 is the
/// LAST Play's: Play, Play, answer (the first, late), answer (the second).
/// Whether it is a start or a failure does not matter: an Error after a
/// newer Play is just as late.
#[test]
fn only_the_answer_to_the_last_play_sent_acts() {
    let mut plays = PlayAnswers::default();
    assert_eq!(plays.pending(), 0);
    plays.sent();
    assert_eq!(plays.pending(), 1);
    assert!(plays.answered(), "one Play: its answer is the last one's");
    assert_eq!(plays.pending(), 0);

    plays.sent(); // A
    plays.sent(); // B, a skip in A's pre-roll
    assert!(!plays.answered(), "A's answer, after B was sent");
    assert_eq!(plays.pending(), 1, "B still waits");
    assert!(plays.answered(), "B's answer");

    plays.sent(); // A
    plays.sent(); // B
    plays.sent(); // C
    assert_eq!(plays.pending(), 3);
    assert!(!plays.answered(), "A's Error, after B and C were sent");
    assert!(!plays.answered(), "B's answer, after C was sent");
    plays.sent(); // D, while C is under way
    assert_eq!(plays.pending(), 2, "C and D wait");
    assert!(!plays.answered(), "C's answer, after D was sent");
    assert!(plays.answered(), "D's answer");
    assert_eq!(plays.pending(), 0);
}

/// An answer with no Play pending (a test injects one; the pipeline never
/// answers more than it was sent) answers the last Play, as before the
/// count: the count stays at 0, so the next Play's answer is its own.
#[test]
fn an_answer_with_no_play_pending_acts_and_leaves_the_count_at_zero() {
    let mut plays = PlayAnswers::default();
    assert!(plays.answered());
    assert!(plays.answered());
    assert_eq!(plays.pending(), 0, "saturated, never below 0");
    plays.sent();
    assert!(plays.answered(), "the next Play's answer owes nothing");
}
