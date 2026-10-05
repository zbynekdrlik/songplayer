//! Where an NDI receiver is expected (#221, design record 5873773896 §1f;
//! B4 step 6). Pure; a sibling of `ndi_health.rs` (its 1000-line cap).
//!
//! Every consumer takes SongPlayer's PROGRAM now: the LED wall `SP-program-MAX`
//! (Spout), the Presenter, strih and the stream `SP-program` (NDI), FOH its
//! VBAN. cg OBS is only the NDI input "OBS manuál", and SongPlayer never
//! switches it to a playlist scene any more (the legacy mirror is deleted),
//! so DistroAV disconnects cg OBS's sp-* inputs.
//!
//! - **A playlist's own NDI output expects NO receiver**
//!   ([`PLAYLIST_RECEIVER_EXPECTED`]): `SP-program` takes the playlist off
//!   the program bus, never over NDI. So the per-playlist dark-wall reason is
//!   dropped ([`expected_reason`]), and the #173 ladder — whose targets are
//!   cg OBS's sp-* inputs — never runs on its own (it fires only on that
//!   reason). Every other degraded reason (an underrun, no frames) stays.
//!   The per-playlist senders, with their ladder and their #196 self-check,
//!   go in the lane that retires them (main session comment 5999882988,
//!   lane 3).
//! - **`SP-program` expects a receiver while a source is on program**
//!   ([`program_degraded_reason`]): served as `degraded_reason` on
//!   `GET /api/v1/program`, logged at the first poll, when its last receiver
//!   goes and when the first comes back ([`log_program_receivers`]), and
//!   gated by the post-deploy E2E.

use tracing::{info, warn};

use crate::playback::ndi_health::DARK_WALL_REASON;

/// No receiver is expected on a playlist's own NDI output (the module doc).
pub(crate) const PLAYLIST_RECEIVER_EXPECTED: bool = false;

/// `GET /api/v1/program` → `degraded_reason` while a source is on program
/// and `SP-program` has no NDI receiver.
pub(crate) const PROGRAM_NO_RECEIVER_REASON: &str = "no NDI receiver on SP-program";

/// The degraded reason with the dark-wall one dropped where no receiver is
/// expected (0 receivers is normal there); any other reason passes through.
pub(crate) fn expected_reason(base: Option<String>, receiver_expected: bool) -> Option<String> {
    if !receiver_expected && base.as_deref() == Some(DARK_WALL_REASON) {
        None
    } else {
        base
    }
}

/// `SP-program`'s degraded reason: [`PROGRAM_NO_RECEIVER_REASON`] while a
/// `source` is on program (a playlist, or -1 "OBS manuál": the consumers take
/// the program whatever it carries) and it has no receiver; none otherwise.
pub(crate) fn program_degraded_reason(
    source: Option<i64>,
    connections: i32,
) -> Option<&'static str> {
    (source.is_some() && connections < 1).then_some(PROGRAM_NO_RECEIVER_REASON)
}

/// The log line of a new `SP-program` receiver count (`before` = `None` on
/// the first poll): a WARN when a source is on program and the first poll
/// finds no receiver or the last one went, an INFO when the first poll finds
/// one or the first one came back. Logging only
/// (`ProgramCore::set_connections`).
#[cfg_attr(test, mutants::skip)]
pub(crate) fn log_program_receivers(source: Option<i64>, before: Option<i32>, after: i32) {
    let had = before.map(|n| n >= 1);
    let has = after >= 1;
    if !has && source.is_some() && had != Some(false) {
        warn!(
            ?source,
            "SP-program: {PROGRAM_NO_RECEIVER_REASON} — the Presenter, strih and the stream get nothing over NDI"
        );
    } else if has && had != Some(true) {
        info!(
            receivers = after,
            ?source,
            "SP-program: a receiver is connected"
        );
    }
}

#[cfg(test)]
#[path = "ndi_health_expect_tests.rs"]
mod tests;
