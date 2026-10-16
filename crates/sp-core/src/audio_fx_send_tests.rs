//! #278: the EQ panel's send gate — live, at most one send per 50 ms, the
//! latest value winning, one request in flight, a drag's end at once.
//! Every step was derived from a scratch model of the gate, which also ran
//! each listed mutant against these scripts.
//! Wired via `#[cfg(test)] #[path = "audio_fx_send_tests.rs"] mod tests;`.

use super::{
    ANSWER_TIMEOUT_MS, LateAnswer, NO_ANSWER_SK, SAVED_SK, SAVING_SK, SendGate, SendStep,
    THROTTLE_MS, answer_sk, late_answer,
};

/// The clock starts away from 0 so a time that is wrongly multiplied or
/// subtracted shows.
const T: f64 = 1000.0;

#[test]
fn the_throttle_is_fifty_ms_and_an_answer_is_awaited_ten_seconds() {
    assert_eq!(THROTTLE_MS, 50.0);
    assert_eq!(ANSWER_TIMEOUT_MS, 10_000);
}

/// Nothing changed: nothing is sent, whatever wakes the gate.
#[test]
fn nothing_is_sent_before_a_change() {
    let mut gate = SendGate::default();
    assert!(!gate.busy());
    assert_eq!(gate.flush(T), SendStep::Idle);
    assert_eq!(gate.tick(T), SendStep::Idle);
    assert_eq!(gate.landed(T), SendStep::Idle);
    assert!(!gate.busy());
}

/// The first change goes out at once; its answer leaves the gate clean.
#[test]
fn the_first_change_is_sent_at_once() {
    let mut gate = SendGate::default();
    assert_eq!(gate.changed(T), SendStep::Send);
    assert!(gate.busy(), "a request is in flight");
    assert_eq!(gate.landed(T + 5.0), SendStep::Idle);
    assert!(!gate.busy());
    // A change long after the last send goes out at once again.
    assert_eq!(gate.changed(T + 200.0), SendStep::Send);
}

/// While a request is in flight nothing else is sent, however many
/// changes come and however late (no timer either: the answer wakes the
/// gate); the latest settings go out when it is answered.
#[test]
fn one_request_in_flight_and_the_latest_change_follows_it() {
    let mut gate = SendGate::default();
    assert_eq!(gate.changed(T), SendStep::Send);
    assert_eq!(gate.changed(T + 10.0), SendStep::Idle);
    assert_eq!(gate.changed(T + 20.0), SendStep::Idle);
    assert_eq!(gate.tick(T + 50.0), SendStep::Idle);
    assert_eq!(gate.changed(T + 300.0), SendStep::Idle);
    assert!(gate.busy());
    // Answered: the waiting change (the latest settings) goes out.
    assert_eq!(gate.landed(T + 400.0), SendStep::Send);
    assert!(gate.busy());
    assert_eq!(gate.landed(T + 420.0), SendStep::Idle);
    assert!(!gate.busy());
}

/// #278 review rounds 2-3: a send unanswered for 10 s is given up as
/// landed: the gate is free, nothing is sent again unless something changed
/// meanwhile (the panel itself watches the given-up request).
#[test]
fn an_unanswered_send_given_up_frees_the_gate() {
    let mut gate = SendGate::default();
    assert_eq!(gate.changed(T), SendStep::Send);
    assert_eq!(gate.landed(T + 10_000.0), SendStep::Idle);
    assert!(!gate.busy());
    // A change made while it hung goes out at the give-up.
    assert_eq!(gate.changed(T + 10_100.0), SendStep::Send);
    assert_eq!(gate.changed(T + 10_200.0), SendStep::Idle);
    assert_eq!(gate.landed(T + 20_100.0), SendStep::Send);
}

/// A change within 50 ms of the last send's START waits for the rest of
/// the window: one timer, and its tick sends; a tick at exactly the
/// window's end sends.
#[test]
fn a_change_inside_the_window_waits_for_its_end() {
    let mut gate = SendGate::default();
    assert_eq!(gate.changed(T), SendStep::Send);
    assert_eq!(gate.landed(T + 5.0), SendStep::Idle);
    assert_eq!(gate.changed(T + 20.0), SendStep::WaitMs(30.0));
    assert!(gate.busy(), "a change waits");
    assert_eq!(gate.changed(T + 30.0), SendStep::Idle, "the timer is armed");
    // A tick before the window's end (a stale timer) re-arms for the rest.
    assert_eq!(gate.tick(T + 45.0), SendStep::WaitMs(5.0));
    assert_eq!(gate.tick(T + 50.0), SendStep::Send);
    assert_eq!(gate.landed(T + 60.0), SendStep::Idle);
    // The window counts from the LAST send's start (T + 50).
    assert_eq!(gate.changed(T + 70.0), SendStep::WaitMs(30.0));
    assert_eq!(gate.tick(T + 100.0), SendStep::Send);
}

/// A drag's end sends its last value at once, inside the window too; with
/// a request in flight, the moment it is answered. A flush with nothing
/// changed sends nothing and leaves no mark behind.
#[test]
fn a_drag_end_flushes_its_last_value() {
    let mut gate = SendGate::default();
    assert_eq!(gate.changed(T), SendStep::Send);
    assert_eq!(gate.landed(T + 5.0), SendStep::Idle);
    assert_eq!(gate.changed(T + 10.0), SendStep::WaitMs(40.0));
    assert_eq!(gate.flush(T + 12.0), SendStep::Send, "inside the window");
    assert_eq!(gate.landed(T + 15.0), SendStep::Idle);
    // The armed timer fires later and finds nothing to send.
    assert_eq!(gate.tick(T + 50.0), SendStep::Idle);

    // In flight: the flush waits for the answer, then skips the window.
    assert_eq!(gate.changed(T + 100.0), SendStep::Send);
    assert_eq!(gate.changed(T + 110.0), SendStep::Idle);
    assert_eq!(gate.flush(T + 111.0), SendStep::Idle);
    assert_eq!(
        gate.landed(T + 115.0),
        SendStep::Send,
        "T + 150 not yet reached"
    );
    assert_eq!(gate.landed(T + 120.0), SendStep::Idle);

    // A flush with nothing changed: no send, and the next change waits for
    // the window as usual (the flush did not stick).
    assert_eq!(gate.flush(T + 130.0), SendStep::Idle);
    assert_eq!(gate.changed(T + 140.0), SendStep::WaitMs(25.0));
    assert_eq!(gate.flush(T + 141.0), SendStep::Send);
    assert_eq!(gate.landed(T + 142.0), SendStep::Idle);
    // The flushed send cleared its mark: the next change waits (its timer
    // is the one still armed), and the window counts from T + 141.
    assert_eq!(gate.changed(T + 143.0), SendStep::Idle);
    assert_eq!(gate.tick(T + 165.0), SendStep::WaitMs(26.0));
    assert_eq!(gate.tick(T + 191.0), SendStep::Send);
}

/// The status line names an answer's error; an answered send reads
/// "Ukladám…" while the gate is still busy, else "Uložené".
#[test]
fn the_status_line_follows_the_answer_and_the_gate() {
    assert_eq!(SAVING_SK, "Ukladám…");
    assert_eq!(SAVED_SK, "Uložené");
    assert_eq!(NO_ANSWER_SK, "Uloženie zlyhalo: server neodpovedá");
    assert_eq!(answer_sk(&Ok(()), false), "Uložené");
    assert_eq!(answer_sk(&Ok(()), true), "Ukladám…");
    let refused = Err("PUT /api/v1/playlists/1/audio → 500".to_string());
    assert_eq!(
        answer_sk(&refused, false),
        "Uloženie zlyhalo: PUT /api/v1/playlists/1/audio → 500"
    );
    assert_eq!(
        answer_sk(&refused, true),
        "Uloženie zlyhalo: PUT /api/v1/playlists/1/audio → 500"
    );
}

/// A given-up request that lands later: with the settings changed since,
/// they go out once more, whether it landed ok or with an error (the row
/// may hold its older value either way); over unchanged settings, an ok
/// landing with nothing going out was the save; anything else asks nothing.
#[test]
fn a_late_answer_resends_changed_settings_and_confirms_unchanged_ones() {
    for (landed_ok, changed_since, busy, want) in [
        (true, true, false, LateAnswer::Resend),
        (true, true, true, LateAnswer::Resend),
        (false, true, false, LateAnswer::Resend),
        (false, true, true, LateAnswer::Resend),
        (true, false, false, LateAnswer::Saved),
        (true, false, true, LateAnswer::Nothing),
        (false, false, false, LateAnswer::Nothing),
        (false, false, true, LateAnswer::Nothing),
    ] {
        assert_eq!(
            late_answer(landed_ok, changed_since, busy),
            want,
            "landed_ok {landed_ok}, changed_since {changed_since}, busy {busy}"
        );
    }
}
