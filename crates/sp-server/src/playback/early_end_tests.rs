//! #277: the early end's rule and its `Error` text.

use super::*;

#[test]
fn an_end_at_0_1_s_of_a_250_s_video_is_an_early_end() {
    assert_eq!(
        EarlyEnd::of(100, 250_965),
        Some(EarlyEnd {
            at_ms: 100,
            duration_ms: 250_965
        })
    );
    assert_eq!(
        EarlyEnd::of(0, 250_965).map(|e| e.at_ms),
        Some(0),
        "no picture at all"
    );
}

#[test]
fn the_ten_second_edge_is_pinned_on_both_sides() {
    // Exactly 10 s before the end is the natural end; a millisecond earlier
    // is an early one.
    assert_eq!(EarlyEnd::of(240_000, 250_000), None);
    assert_eq!(
        EarlyEnd::of(239_999, 250_000),
        Some(EarlyEnd {
            at_ms: 239_999,
            duration_ms: 250_000
        })
    );
    assert_eq!(
        EarlyEnd::of(249_960, 250_000),
        None,
        "the last frame's own time"
    );
    assert_eq!(EarlyEnd::of(300_000, 250_000), None, "past the end");
}

#[test]
fn a_video_of_unknown_length_never_ends_early() {
    assert_eq!(EarlyEnd::of(0, 0), None);
    assert_eq!(EarlyEnd::of(0, EARLY_EOS_SLACK_MS), None);
    assert_eq!(EarlyEnd::of(u64::MAX, u64::MAX), None, "no overflow");
}

#[test]
fn the_reason_names_the_file_and_both_positions() {
    let early = EarlyEnd {
        at_ms: 66,
        duration_ms: 250_965,
    };
    let reason = early.reason("Remembrance_J Jessup_abcdefghijk_normalized_video.mp4");
    assert_eq!(
        reason,
        "early_end: Remembrance_J Jessup_abcdefghijk_normalized_video.mp4 \
         ended at 66 ms of 250965 ms"
    );
    assert!(is_early_end(&reason));
    assert!(!is_after_start(&reason), "sent before Started: an answer");
}

#[test]
fn an_early_end_after_its_start_is_marked_as_such() {
    let early = EarlyEnd {
        at_ms: 100_000,
        duration_ms: 250_000,
    };
    let after = after_start(&early.reason("x.mp4"));
    assert_eq!(
        after,
        "early_end: x.mp4 ended at 100000 ms of 250000 ms (after its start)"
    );
    assert!(is_early_end(&after));
    assert!(is_after_start(&after));
}

#[test]
fn other_errors_are_no_early_end() {
    for error in [
        "No video: SetCurrentMediaType failed: No suitable transform",
        "failed to open video C:\\x.mp4: early_end:",
        "a decode error (after its start)",
        "",
    ] {
        assert!(!is_early_end(error), "{error}");
        assert!(!is_after_start(error), "{error}");
    }
}
