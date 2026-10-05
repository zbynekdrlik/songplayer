//! #221: where a receiver is expected (pure). The engine-level effect on a
//! playlist output's health (the dark-wall reason, the ladder, the #196
//! self-check, the lock) is pinned in `ndi_health_tests_expect.rs`;
//! SP-program's reason on `GET /api/v1/program` in `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_expect_tests.rs"] mod tests;`.

use super::{PROGRAM_NO_RECEIVER_REASON, judged_connections, program_degraded_reason};

/// #221 B4 step 6: a playlist's own output expects no receiver, so its 0
/// receivers (or the SDK's -1) is judged satisfied, and a real count passes
/// as it is.
#[test]
fn a_playlist_output_s_count_is_judged_satisfied() {
    assert_eq!(judged_connections(0), 1);
    assert_eq!(judged_connections(-1), 1);
    assert_eq!(judged_connections(1), 1);
    assert_eq!(judged_connections(3), 3);
}

/// SP-program expects a receiver while any source is on program (a
/// playlist, or -1 "OBS manuál"): 0 or fewer receivers is the degraded
/// reason, one or more is none, nothing on program is none, and so is a
/// count the sender never polled (review round 1).
#[test]
fn sp_program_expects_a_receiver_while_a_source_is_on_program() {
    assert_eq!(PROGRAM_NO_RECEIVER_REASON, "no NDI receiver on SP-program");
    let dark = Some(PROGRAM_NO_RECEIVER_REASON);
    assert_eq!(program_degraded_reason(Some(7), Some(0)), dark);
    assert_eq!(
        program_degraded_reason(Some(-1), Some(0)),
        dark,
        "OBS manuál"
    );
    assert_eq!(
        program_degraded_reason(Some(7), Some(-1)),
        dark,
        "the SDK's error value"
    );
    assert_eq!(
        program_degraded_reason(Some(7), Some(1)),
        None,
        "one receiver"
    );
    assert_eq!(program_degraded_reason(Some(7), Some(3)), None);
    assert_eq!(
        program_degraded_reason(Some(7), None),
        None,
        "not polled yet"
    );
    assert_eq!(
        program_degraded_reason(None, Some(0)),
        None,
        "nothing on program"
    );
    assert_eq!(program_degraded_reason(None, Some(2)), None);
}
