//! #221: where a receiver is expected (pure). The engine-level effect on a
//! playlist output's health (the dark-wall reason, the ladder, the #196
//! self-check, the lock) is pinned in `ndi_health_tests_expect.rs`;
//! SP-program's reason on `GET /api/v1/program` in `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_expect_tests.rs"] mod tests;`.

use super::{
    PROGRAM_NO_RECEIVER_REASON, judged_connections, program_degraded_reason, receiver_log,
};

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

/// #221 review round 4: SP-program's receiver log turns dark — a source on
/// program, no receiver — once per dark stretch: on the first poll, when the
/// last receiver goes, and when a source comes on program while none is
/// connected; a receiver found on the first poll or back is the INFO.
#[test]
fn sp_program_s_receiver_log_turns_dark_once_and_names_a_receiver_found() {
    use super::ReceiverLog::{Nothing, ReceiverFound, TurnedDark};
    // The first poll.
    assert_eq!(receiver_log(Some(7), None, 0, false), (TurnedDark, true));
    assert_eq!(
        receiver_log(Some(7), None, 2, false),
        (ReceiverFound, false)
    );
    assert_eq!(receiver_log(None, None, 0, false), (Nothing, false));
    // Still dark: no repeat.
    assert_eq!(receiver_log(Some(7), Some(0), 0, true), (Nothing, true));
    // The last receiver went.
    assert_eq!(receiver_log(Some(7), Some(2), 0, false), (TurnedDark, true));
    // A source came on program while none was connected.
    assert_eq!(receiver_log(Some(7), Some(0), 0, false), (TurnedDark, true));
    // The first receiver came back.
    assert_eq!(
        receiver_log(Some(7), Some(0), 1, true),
        (ReceiverFound, false)
    );
    assert_eq!(
        receiver_log(None, Some(-1), 1, false),
        (ReceiverFound, false)
    );
    // Receivers all along.
    assert_eq!(receiver_log(Some(7), Some(1), 1, false), (Nothing, false));
    assert_eq!(receiver_log(Some(7), Some(2), 3, false), (Nothing, false));
    // Nothing on program and no receiver is not dark.
    assert_eq!(receiver_log(None, Some(0), 0, true), (Nothing, false));
}
