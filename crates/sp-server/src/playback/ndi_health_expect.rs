//! Where an NDI receiver is expected (#221, design record 5873773896 §1f;
//! B4 step 6). Pure; a sibling of `ndi_health.rs` (its 1000-line cap).
//!
//! Every consumer takes SongPlayer's PROGRAM: the LED wall `SP-program-MAX`
//! (Spout), the Presenter, strih and the stream `SP-program` (NDI), FOH its
//! VBAN. cg OBS is only the NDI input "OBS manuál". #221 lane 3 retired the
//! per-playlist NDI senders (a playlist feeds the program bus only), so
//! `SP-program` is the one NDI sender, and it expects a receiver while a
//! source is on program ([`program_degraded_reason`]): served as
//! `degraded_reason` on `GET /api/v1/program` once the sender has polled its
//! receivers, logged when it turns dark and when a receiver is found or back
//! ([`log_program_receivers`]), and gated by the post-deploy E2E.

use tracing::{info, warn};

/// `GET /api/v1/program` → `degraded_reason` while a source is on program
/// and `SP-program` has no NDI receiver.
pub(crate) const PROGRAM_NO_RECEIVER_REASON: &str = "no NDI receiver on SP-program";

/// `SP-program`'s degraded reason: [`PROGRAM_NO_RECEIVER_REASON`] while a
/// `source` is on program (a playlist, or -1 "OBS manuál": the consumers take
/// the program whatever it carries) and it has no receiver; none otherwise,
/// and none before the sender polled its receivers (`connections` `None`:
/// the count's initial 0 is no reading).
pub(crate) fn program_degraded_reason(
    source: Option<i64>,
    connections: Option<i32>,
) -> Option<&'static str> {
    let unreceived = connections.is_some_and(|n| n < 1);
    (source.is_some() && unreceived).then_some(PROGRAM_NO_RECEIVER_REASON)
}

/// What a new `SP-program` receiver count is logged as ([`receiver_log`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiverLog {
    /// It turned dark: a source on program, no receiver (a WARN).
    TurnedDark,
    /// The first poll found a receiver, or the first one came back (an INFO).
    ReceiverFound,
    /// Nothing changed worth a line.
    Nothing,
}

/// The decision of a new `SP-program` receiver count (pure): `before` is the
/// last polled count (`None` on the first poll), `was_dark` whether the last
/// poll found `SP-program` dark (a source on program, no receiver). Returns
/// what to log and whether it is dark now. It turns dark (a WARN, not
/// repeated while it stays dark) when the first poll finds no receiver, the
/// last one went, or a source came on program while none was connected
/// (review round 4); a receiver found on the first poll or back is an INFO.
pub(crate) fn receiver_log(
    source: Option<i64>,
    before: Option<i32>,
    after: i32,
    was_dark: bool,
) -> (ReceiverLog, bool) {
    let has = after >= 1;
    let dark = source.is_some() && !has;
    let log = if dark && !was_dark {
        ReceiverLog::TurnedDark
    } else if has && before.is_none_or(|n| n < 1) {
        ReceiverLog::ReceiverFound
    } else {
        ReceiverLog::Nothing
    };
    (log, dark)
}

/// The log line of a new `SP-program` receiver count ([`receiver_log`]
/// decides); returns whether `SP-program` is dark now, which
/// `ProgramCore::set_connections` keeps for the next poll. Logging only.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn log_program_receivers(
    source: Option<i64>,
    before: Option<i32>,
    after: i32,
    was_dark: bool,
) -> bool {
    let (log, dark) = receiver_log(source, before, after, was_dark);
    match log {
        ReceiverLog::TurnedDark => warn!(
            ?source,
            "SP-program: {PROGRAM_NO_RECEIVER_REASON} — the Presenter, strih and the stream get nothing over NDI"
        ),
        ReceiverLog::ReceiverFound => info!(
            receivers = after,
            ?source,
            "SP-program: a receiver is connected"
        ),
        ReceiverLog::Nothing => {}
    }
    dark
}

#[cfg(test)]
#[path = "ndi_health_expect_tests.rs"]
mod tests;
