//! The Spout sender's registration, as a pure state machine (#223 S1b).
//!
//! Spout registers a sender at its first `SendTexture`, and on a name another
//! sender listed it RENAMES it (`<name>_1`), which Resolume Arena's
//! `SPOUT_<name>` layer would never show. So the sender REFUSES instead:
//!
//! - at create, when its name is listed ([`claim`]);
//! - before its first send, when another sender listed the name since
//!   ([`before_send`]);
//! - after its first send, when Spout registered another name, did not list
//!   it, or could not register it ([`after_send`]).
//!
//! A refusal is for good: what the sender registered is released and it
//! never sends again. The shim (`win/spout_shim.cpp`) only does SpoutDX's
//! steps and reports what happened; every decision is here.

use crate::error::GpuError;
use crate::spout::status_result;

/// Why a sender is refused: another sender listed the name before this one's
/// first send went through (Spout would rename this one `<name>_1`).
pub const TAKEN: &str = "another sender took the name before its first send";
/// Why a sender is refused: its first send went through, but Spout's list
/// does not hold the name.
pub const NOT_LISTED: &str =
    "Spout did not list it (its list is full, or another program's clean-up dropped it)";
/// Why a sender is refused: Spout could not register it (its shared texture
/// or its maps could not be made, or the SDK threw).
pub const REGISTRATION_FAILED: &str = "Spout could not register it";

/// What Spout's names list says about a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    /// The list holds the name.
    Yes,
    /// The list does not hold the name.
    No,
    /// The list could not be read now (its 67 ms lock).
    Unknown,
}

impl Listed {
    /// The shim's `spout_sender_listed` answer: 1 yes, 0 no, else unknown.
    pub fn from_code(code: i32) -> Self {
        match code {
            1 => Listed::Yes,
            0 => Listed::No,
            _ => Listed::Unknown,
        }
    }
}

/// Where a sender is in its registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// Nothing registered yet: the next send registers it.
    Fresh,
    /// Spout registered it under its name, but its list could not be read
    /// to confirm the listing: the next send checks again.
    Unconfirmed,
    /// Spout lists it under its name.
    Confirmed,
    /// Refused for good, and why.
    Refused(&'static str),
}

/// What spoutDX holds after a send of a sender that is not yet confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstSendFacts {
    /// spoutDX holds a registration (`IsInitialized`).
    pub initialized: bool,
    /// Under the name asked for (not `<name>_<n>`).
    pub name_matches: bool,
    /// Whether Spout's list holds the name.
    pub listed: Listed,
}

/// What comes before a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeforeSend {
    /// Send.
    Send,
    /// Refuse the sender now (release what it holds), and why.
    Refuse(&'static str),
    /// The sender was refused before: send nothing, and why.
    Refused(&'static str),
}

/// What a send leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSend {
    /// The frame is shared; the sender is now in this state.
    Shared(Registration),
    /// This frame was lost (the shim's code: 3 = SendTexture failed, 4 = a
    /// C++ exception); the sender stays as it was.
    Lost(i32),
    /// Refuse the sender for good (release what it registered), and why.
    Refuse(&'static str),
}

/// The verdict on a new sender: [`GpuError::SpoutNameTaken`] when `listed`
/// says another sender holds the name (then `claim_name` is never called),
/// else the shim's `claim_name` code mapped by `status_result` (0 kept, 1
/// Spout renamed it: taken).
pub fn claim(listed: Listed, claim_name: impl FnOnce() -> i32, name: &str) -> Result<(), GpuError> {
    if listed == Listed::Yes {
        return Err(GpuError::SpoutNameTaken {
            name: name.to_owned(),
        });
    }
    status_result(claim_name(), "spout_sender_claim_name", name)
}

/// Before a send in `state`: a fresh sender whose name another sender has
/// listed since it was created is refused before Spout can rename it
/// (`listed` is asked only then).
pub fn before_send(state: Registration, listed: impl FnOnce() -> Listed) -> BeforeSend {
    match state {
        Registration::Refused(why) => BeforeSend::Refused(why),
        Registration::Fresh => {
            if listed() == Listed::Yes {
                BeforeSend::Refuse(TAKEN)
            } else {
                BeforeSend::Send
            }
        }
        Registration::Unconfirmed | Registration::Confirmed => BeforeSend::Send,
    }
}

/// After a send in `state` that returned `code` (0 sent). A confirmed
/// sender only shares or loses a frame, and `facts` is not asked (no lock
/// per frame). Otherwise:
///
/// - sent, under the name asked for: listed → confirmed, list unreadable →
///   unconfirmed (checked again next send), not listed → refused;
/// - sent under another name → refused;
/// - not sent (failed or threw): with a registration the frame is lost;
///   without one the registration failed half-made, and a retry would meet
///   its own stale name and be renamed → refused.
pub fn after_send(
    state: Registration,
    code: i32,
    facts: impl FnOnce() -> FirstSendFacts,
) -> AfterSend {
    match state {
        Registration::Refused(why) => AfterSend::Refuse(why),
        Registration::Confirmed if code == 0 => AfterSend::Shared(Registration::Confirmed),
        Registration::Confirmed => AfterSend::Lost(code),
        Registration::Fresh | Registration::Unconfirmed => {
            let facts = facts();
            if code != 0 {
                return if facts.initialized {
                    AfterSend::Lost(code)
                } else {
                    AfterSend::Refuse(REGISTRATION_FAILED)
                };
            }
            if !facts.name_matches {
                return AfterSend::Refuse(TAKEN);
            }
            match facts.listed {
                Listed::Yes => AfterSend::Shared(Registration::Confirmed),
                Listed::Unknown => AfterSend::Shared(Registration::Unconfirmed),
                Listed::No => AfterSend::Refuse(NOT_LISTED),
            }
        }
    }
}

#[cfg(test)]
#[path = "spout_state_tests.rs"]
mod tests;
