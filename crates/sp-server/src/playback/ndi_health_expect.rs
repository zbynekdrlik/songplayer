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
//!   the program bus, never over NDI. So its receiver count plays no part in
//!   its health (review round 1): the pipeline counts no bad poll for it
//!   (`pipeline::classify_bad_poll`), and the health handler judges it on a
//!   satisfied count ([`judged_connections`]) — no dark-wall reason, so the
//!   #173 ladder (whose targets are cg OBS's sp-* inputs) never runs on its
//!   own; no #196 post-restart flag; no "no receiver" lock. An underrun or a
//!   stalled submit is still reported. The per-playlist senders, with their
//!   ladder and their #196 self-check, go in the lane that retires them
//!   (main session comment 5999882988, lane 3).
//! - **`SP-program` expects a receiver while a source is on program**
//!   ([`program_degraded_reason`]): served as `degraded_reason` on
//!   `GET /api/v1/program` once the sender has polled its receivers, logged
//!   when it turns dark and when a receiver is found or back
//!   ([`log_program_receivers`]), and gated by the post-deploy E2E.

use tracing::{info, warn};

/// No receiver is expected on a playlist's own NDI output (the module doc).
pub(crate) const PLAYLIST_RECEIVER_EXPECTED: bool = false;

/// `GET /api/v1/program` → `degraded_reason` while a source is on program
/// and `SP-program` has no NDI receiver.
pub(crate) const PROGRAM_NO_RECEIVER_REASON: &str = "no NDI receiver on SP-program";

/// The receiver count a playlist output's HEALTH is judged on: it expects
/// no receiver ([`PLAYLIST_RECEIVER_EXPECTED`]), so 0 is normal and the count
/// is taken as satisfied (at least 1). It never makes the output dark,
/// flagged after a restart or DEGRADED in its lock — an underrun or a stalled
/// submit still does. The snapshot, the logs and the persisted count keep the
/// real one.
pub(crate) fn judged_connections(connections: i32) -> i32 {
    connections.max(1)
}

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

/// The log line of a new `SP-program` receiver count (`before` = `None` on
/// the first poll), and whether `SP-program` is now dark (a source on
/// program, no receiver): a WARN when it turns dark (`was_dark` false: the
/// first poll finds no receiver, the last one went, or a source came on
/// program while none was connected), an INFO when the first poll finds a
/// receiver or the first one came back. Logging only
/// (`ProgramCore::set_connections` keeps the dark state for the next poll).
#[cfg_attr(test, mutants::skip)]
pub(crate) fn log_program_receivers(
    source: Option<i64>,
    before: Option<i32>,
    after: i32,
    was_dark: bool,
) -> bool {
    let has = after >= 1;
    let dark = source.is_some() && !has;
    if dark && !was_dark {
        warn!(
            ?source,
            "SP-program: {PROGRAM_NO_RECEIVER_REASON} — the Presenter, strih and the stream get nothing over NDI"
        );
    } else if has && before.is_none_or(|n| n < 1) {
        info!(
            receivers = after,
            ?source,
            "SP-program: a receiver is connected"
        );
    }
    dark
}

#[cfg(test)]
#[path = "ndi_health_expect_tests.rs"]
mod tests;
