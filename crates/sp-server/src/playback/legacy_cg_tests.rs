//! #221 L4a: SongPlayer's record of what it told cg OBS (pure). The switch
//! path that feeds it is tested end to end in `remote/session_tests_legacy.rs`
//! (the facade) and `api/program_tests_switch.rs` (the dashboard).
//! Wired via `#[cfg(test)] #[path = "legacy_cg_tests.rs"] mod tests;`.

use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::remote::Upstream;

#[test]
fn nothing_is_shown_before_anything_was_told() {
    let legacy = LegacyCg::default();
    assert_eq!(legacy.shown_now(), None);
    assert_eq!(legacy.status(), LegacyCgStatus { shown: None });
    assert_eq!(
        serde_json::to_value(legacy.status()).unwrap(),
        json!({ "shown": null })
    );
}

#[test]
fn an_accepted_mirror_shows_its_playlist_and_an_accepted_manual_scene_none() {
    let legacy = LegacyCg::default();
    let mirror = legacy.ticket();
    assert!(legacy.confirmed(mirror, Some(7)));
    assert_eq!(legacy.shown_now(), Some(7));
    assert_eq!(
        serde_json::to_value(legacy.status()).unwrap(),
        json!({ "shown": 7 })
    );
    let manual = legacy.ticket();
    assert!(legacy.confirmed(manual, None));
    assert_eq!(legacy.shown_now(), None);
}

#[test]
fn a_command_without_an_answer_changes_nothing() {
    let legacy = LegacyCg::default();
    let first = legacy.ticket();
    assert!(legacy.confirmed(first, Some(7)));
    // A later command cg OBS refused (or never answered) is never confirmed.
    let _refused = legacy.ticket();
    assert_eq!(legacy.shown_now(), Some(7));
}

/// A late answer to an OLDER command never overwrites a newer one's, and an
/// answer is applied once.
#[test]
fn only_an_answer_newer_than_every_applied_one_counts() {
    let legacy = LegacyCg::default();
    let older = legacy.ticket();
    let newer = legacy.ticket();
    assert_ne!(older, newer);
    assert!(legacy.confirmed(newer, Some(3)));
    assert!(
        !legacy.confirmed(older, Some(7)),
        "the older answer is late"
    );
    assert_eq!(legacy.shown_now(), Some(3));
    assert!(
        !legacy.confirmed(newer, None),
        "an answer is applied only once"
    );
    assert_eq!(legacy.shown_now(), Some(3));
    // The next command after the newest applied one counts again.
    let next = legacy.ticket();
    assert!(legacy.confirmed(next, None));
    assert_eq!(legacy.shown_now(), None);
}

/// Answers applied out of order, but each newer than the last applied one,
/// all count (no answer is waited for).
#[test]
fn a_skipped_ticket_does_not_block_a_newer_answer() {
    let legacy = LegacyCg::default();
    let first = legacy.ticket();
    let _unanswered = legacy.ticket();
    let third = legacy.ticket();
    assert!(legacy.confirmed(first, Some(7)));
    assert!(legacy.confirmed(third, Some(3)));
    assert_eq!(legacy.shown_now(), Some(3));
}

#[test]
fn a_restored_playlist_is_shown_and_the_restored_input_is_not() {
    let legacy = LegacyCg::default();
    legacy.restored(sp_core::config::PROGRAM_INPUT_ID);
    assert_eq!(legacy.shown_now(), None);
    legacy.restored(7);
    assert_eq!(legacy.shown_now(), Some(7));
    // The first command after the restore is still newer than it.
    let ticket = legacy.ticket();
    assert!(legacy.confirmed(ticket, Some(3)));
    assert_eq!(legacy.shown_now(), Some(3));
}

#[tokio::test]
async fn a_receiver_sees_every_change() {
    let legacy = LegacyCg::default();
    legacy.restored(7); // before anyone subscribed: kept
    let mut shown = legacy.shown();
    assert_eq!(*shown.borrow_and_update(), Some(7));
    let ticket = legacy.ticket();
    legacy.confirmed(ticket, None);
    assert!(shown.has_changed().unwrap());
    assert_eq!(*shown.borrow_and_update(), None);
}

#[tokio::test]
async fn the_dashboards_link_is_unlinked_until_one_is_attached() {
    let legacy = LegacyCg::default();
    assert!(
        legacy
            .link()
            .enqueue("SetCurrentProgramScene", None, true)
            .is_none(),
        "no link: nothing reaches cg OBS"
    );
    let (tx, mut rx) = mpsc::channel(4);
    let (events, _) = broadcast::channel(4);
    assert!(legacy.attach(Upstream::new(Some(tx), events.clone())));
    assert!(
        !legacy.attach(Upstream::new(None, events)),
        "the first link stays"
    );
    assert!(
        legacy
            .link()
            .enqueue("SetCurrentProgramScene", None, true)
            .is_some()
    );
    assert!(rx.try_recv().is_ok(), "the call reached the attached link");
}
