//! #330: the process's decode priority (one test: the values are statics).

use super::{Applied, note_applied, note_error, requested, set_requested, status};

#[test]
fn the_decode_priority_is_kept_reported_and_counted() {
    assert!(set_requested(-5), "0 → -5 is a change");
    assert!(!set_requested(-5), "the same value is no change");
    assert_eq!(requested(), -5);
    let before = status();
    assert_eq!(before.requested, -5);

    note_applied(-5, -4);
    note_error();
    note_error();
    let after = status();
    assert_eq!(
        after.last,
        Some(Applied {
            requested: -5,
            read_back: -4
        })
    );
    assert_eq!(after.errors, before.errors + 2);

    assert!(set_requested(3));
    assert_eq!(status().requested, 3);
    assert!(set_requested(0));
}
