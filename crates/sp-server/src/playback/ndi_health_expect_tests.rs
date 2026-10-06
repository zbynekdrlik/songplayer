//! #221: where a receiver is expected (pure): `SP-program`, the one NDI
//! sender. Its reason on `GET /api/v1/program` is pinned in
//! `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_expect_tests.rs"] mod tests;`.

use super::{PROGRAM_NO_RECEIVER_REASON, program_degraded_reason, receiver_log};

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
