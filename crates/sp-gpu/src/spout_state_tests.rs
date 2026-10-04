//! Tests for the Spout sender's registration state machine (#223 S1b).

use super::{
    AfterSend, BeforeSend, FirstSendFacts, Listed, NOT_LISTED, REGISTRATION_FAILED, Registration,
    TAKEN, after_send, before_send, claim,
};
use crate::error::GpuError;

fn facts(initialized: bool, name_matches: bool, listed: Listed) -> FirstSendFacts {
    FirstSendFacts {
        initialized,
        name_matches,
        listed,
    }
}

/// A `listed` question that must not be asked.
fn never_listed() -> Listed {
    panic!("the names list must not be read here")
}

/// The first-send facts that must not be gathered.
fn never_facts() -> FirstSendFacts {
    panic!("the first-send facts must not be gathered here")
}

#[test]
fn the_shim_s_list_answer_is_yes_no_or_unknown() {
    assert_eq!(Listed::from_code(1), Listed::Yes);
    assert_eq!(Listed::from_code(0), Listed::No);
    for code in [-1, 2, 4] {
        assert_eq!(Listed::from_code(code), Listed::Unknown, "{code}");
    }
}

#[test]
fn the_reasons_say_what_happened() {
    assert_eq!(TAKEN, "another sender took the name before its first send");
    assert_eq!(
        NOT_LISTED,
        "Spout did not list it (its list is full, or another program's clean-up dropped it)"
    );
    assert_eq!(REGISTRATION_FAILED, "Spout could not register it");
}

#[test]
fn a_listed_name_is_taken_at_create_and_never_claimed() {
    let taken = Err(GpuError::SpoutNameTaken {
        name: "SP-program-MAX".to_owned(),
    });
    assert_eq!(
        claim(Listed::Yes, || panic!("never claimed"), "SP-program-MAX"),
        taken
    );
    // Spout renamed it while claiming: another sender listed it meanwhile.
    assert_eq!(claim(Listed::No, || 1, "SP-program-MAX"), taken);
}

#[test]
fn a_free_or_unreadable_list_lets_the_name_be_claimed() {
    // Unknown: Spout's own FindSenderName reads an unreadable list as free.
    for listed in [Listed::No, Listed::Unknown] {
        let mut claimed = false;
        let verdict = claim(
            listed,
            || {
                claimed = true;
                0
            },
            "SP-program-MAX",
        );
        assert_eq!(verdict, Ok(()), "{listed:?}");
        assert!(claimed, "{listed:?}: the name is given to spoutDX");
    }
    assert_eq!(
        claim(Listed::No, || 4, "n"),
        Err(GpuError::Spout {
            call: "spout_sender_claim_name",
            code: 4
        })
    );
}

#[test]
fn a_fresh_sender_whose_name_was_listed_since_is_refused_before_it_sends() {
    assert_eq!(
        before_send(Registration::Fresh, || Listed::Yes),
        BeforeSend::Refuse(TAKEN)
    );
    for listed in [Listed::No, Listed::Unknown] {
        assert_eq!(
            before_send(Registration::Fresh, || listed),
            BeforeSend::Send,
            "{listed:?}"
        );
    }
}

#[test]
fn a_registered_sender_sends_without_reading_the_list() {
    for state in [Registration::Unconfirmed, Registration::Confirmed] {
        assert_eq!(
            before_send(state, never_listed),
            BeforeSend::Send,
            "{state:?}"
        );
    }
}

#[test]
fn a_refused_sender_stays_refused_and_sends_nothing() {
    for why in [TAKEN, NOT_LISTED, REGISTRATION_FAILED] {
        assert_eq!(
            before_send(Registration::Refused(why), never_listed),
            BeforeSend::Refused(why)
        );
        assert_eq!(
            after_send(Registration::Refused(why), 0, never_facts),
            AfterSend::Refuse(why)
        );
    }
}

#[test]
fn a_confirmed_sender_shares_or_loses_a_frame_without_any_check() {
    assert_eq!(
        after_send(Registration::Confirmed, 0, never_facts),
        AfterSend::Shared(Registration::Confirmed)
    );
    for code in [3, 4, 5] {
        assert_eq!(
            after_send(Registration::Confirmed, code, never_facts),
            AfterSend::Lost(code)
        );
    }
}

#[test]
fn a_first_send_under_its_name_is_confirmed_by_the_list() {
    for state in [Registration::Fresh, Registration::Unconfirmed] {
        let after = |listed| after_send(state, 0, || facts(true, true, listed));
        assert_eq!(
            after(Listed::Yes),
            AfterSend::Shared(Registration::Confirmed),
            "{state:?}"
        );
        // The list could not be read: checked again at the next send.
        assert_eq!(
            after(Listed::Unknown),
            AfterSend::Shared(Registration::Unconfirmed),
            "{state:?}"
        );
        assert_eq!(
            after(Listed::No),
            AfterSend::Refuse(NOT_LISTED),
            "{state:?}"
        );
    }
}

#[test]
fn a_first_send_spout_registered_under_another_name_is_refused() {
    // Spout renamed it `<name>_1`; the list holds `<name>` (the other sender).
    for listed in [Listed::Yes, Listed::No, Listed::Unknown] {
        assert_eq!(
            after_send(Registration::Fresh, 0, || facts(true, false, listed)),
            AfterSend::Refuse(TAKEN),
            "{listed:?}"
        );
    }
}

#[test]
fn a_send_that_fails_before_any_registration_refuses_the_sender() {
    for code in [3, 4] {
        assert_eq!(
            after_send(Registration::Fresh, code, || facts(false, true, Listed::No)),
            AfterSend::Refuse(REGISTRATION_FAILED),
            "{code}"
        );
    }
}

#[test]
fn a_send_that_fails_after_the_registration_loses_only_the_frame() {
    for state in [Registration::Fresh, Registration::Unconfirmed] {
        for code in [3, 4] {
            assert_eq!(
                after_send(state, code, || facts(true, true, Listed::Yes)),
                AfterSend::Lost(code),
                "{state:?} {code}"
            );
        }
    }
}
