//! #221: where a receiver is expected (pure). The engine-level effect on a
//! playlist output's dark-wall reason, the ladder and the #196 self-check is
//! pinned in `ndi_health_tests_expect.rs`; SP-program's reason on
//! `GET /api/v1/program` in `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_expect_tests.rs"] mod tests;`.

use super::{
    PLAYLIST_RECEIVER_EXPECTED, PROGRAM_NO_RECEIVER_REASON, expected_reason,
    program_degraded_reason,
};
use crate::playback::ndi_health::DARK_WALL_REASON;

/// Only the dark-wall reason keys on the expectation; any other reason (an
/// underrun, no frames) passes through either way.
#[test]
fn only_the_dark_wall_reason_is_dropped_where_no_receiver_is_expected() {
    let dark = || Some(DARK_WALL_REASON.to_string());
    let underrun = || Some("underrunning (10/30 fps)".to_string());
    assert_eq!(expected_reason(dark(), false), None);
    assert_eq!(expected_reason(dark(), true), dark());
    assert_eq!(expected_reason(underrun(), false), underrun());
    assert_eq!(expected_reason(underrun(), true), underrun());
    assert_eq!(expected_reason(None, false), None);
    assert_eq!(expected_reason(None, true), None);
}

/// #221 B4 step 6: a playlist's own output expects no receiver, so its
/// dark-wall reason is always dropped.
#[test]
fn a_playlist_output_expects_no_receiver() {
    let dark = Some(DARK_WALL_REASON.to_string());
    assert_eq!(expected_reason(dark, PLAYLIST_RECEIVER_EXPECTED), None);
}

/// SP-program expects a receiver while any source is on program (a
/// playlist, or -1 "OBS manuál"): 0 or fewer receivers is the degraded
/// reason, one or more is none, and nothing on program is none.
#[test]
fn sp_program_expects_a_receiver_while_a_source_is_on_program() {
    assert_eq!(PROGRAM_NO_RECEIVER_REASON, "no NDI receiver on SP-program");
    let dark = Some(PROGRAM_NO_RECEIVER_REASON);
    assert_eq!(program_degraded_reason(Some(7), 0), dark);
    assert_eq!(program_degraded_reason(Some(-1), 0), dark, "OBS manuál");
    assert_eq!(program_degraded_reason(Some(7), -1), dark, "not polled yet");
    assert_eq!(program_degraded_reason(Some(7), 1), None, "one receiver");
    assert_eq!(program_degraded_reason(Some(7), 3), None);
    assert_eq!(program_degraded_reason(None, 0), None, "nothing on program");
    assert_eq!(program_degraded_reason(None, 2), None);
}
