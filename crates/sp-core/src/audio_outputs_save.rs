//! #233 release review: the dashboard's "Zvukové výstupy" decisions, pure
//! (the sp-ui section only acts on them; sp-ui has no unit tests).
//!
//! - Right before its PATCH the section re-reads `GET /api/v1/settings` and
//!   [`save_refusal`] decides on what the server holds NOW against what the
//!   page loaded: a list changed elsewhere (the main session's API, another
//!   dashboard) is never overwritten, nor a rate changed elsewhere when this
//!   save would send one, and nothing is saved while the old `vban_*` keys
//!   still wait for their migration (a stored list would stop it for good,
//!   FOH with it).
//! - The rate is sent only when it changed from the one loaded
//!   ([`rate_to_send`]).
//! - A number field that holds no whole number is refused at save
//!   ([`parse_whole`], [`not_a_number_sk`]), never stored as 0 or ignored.
//! - "Uložené" is shown only while the rows and the rate are what was saved
//!   ([`shown_message`]).
//! - "Odobrať" removes ONE row ([`remove_one`]), never every row of a
//!   repeated id.
//! - A waiting VBAN output's reason in Slovak ([`vban_reason_sk`],
//!   [`vban_waiting_text`]), from the server's `reason_code`.
//! - Every running row's latency reads the same ([`latency_sk`], review
//!   round 2: a VBAN row read "67 ms", an ASIO row "oneskorenie 71 ms").

use std::collections::HashMap;

use crate::audio_outputs::{OutputEntry, field_sk, shown_id};
use crate::config::{
    SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, SETTING_VBAN_ENABLED,
    SETTING_VBAN_STREAM_NAME, SETTING_VBAN_TARGETS, audio_network_rate,
};

/// The old keys still wait for their migration into the list.
pub const MIGRATION_PENDING: &str =
    "Výstupy sa ešte prenášajú zo starých nastavení VBAN — skúste uložiť o pár sekúnd";

/// The list (or the rate this save would send) changed on the server since
/// the page loaded it.
pub const CHANGED_ON_SERVER: &str =
    "Výstupy sa medzičasom zmenili na serveri — obnovte stránku (F5) a upravte ich znova";

/// The re-read before the save failed.
pub const NOT_CHECKED: &str = "Uložené výstupy sa nedali overiť — skúste uložiť znova";

/// What a save that went through shows.
pub const SAVED: &str = "Uložené";

/// A stored value as the section compares it: trimmed, blank as absent.
fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Two stored values of `audio_outputs` hold the same list: compared as
/// lists when both read (the server stores the list normalized, the page
/// keeps the text it sent), else as text.
pub fn same_list(a: Option<&str>, b: Option<&str>) -> bool {
    let (a, b) = (present(a), present(b));
    let read = |v: Option<&str>| v.map(serde_json::from_str::<Vec<OutputEntry>>);
    match (read(a), read(b)) {
        (Some(Ok(x)), Some(Ok(y))) => x == y,
        _ => a == b,
    }
}

/// The migration of the old keys has not run on the server: `audio_outputs`
/// is absent (the key, as the server's migration reads it) and a `vban_*`
/// key exists.
pub fn migration_pending(now: &HashMap<String, String>) -> bool {
    !now.contains_key(SETTING_AUDIO_OUTPUTS)
        && [
            SETTING_VBAN_ENABLED,
            SETTING_VBAN_STREAM_NAME,
            SETTING_VBAN_TARGETS,
        ]
        .iter()
        .any(|k| now.contains_key(*k))
}

/// Why the save must not send, from `now` (the settings the server holds,
/// re-read right before the PATCH) against what the page loaded
/// (`loaded_list`, `loaded_rate`); `send_rate`: the save would send the
/// rate. `None`: send.
pub fn save_refusal(
    loaded_list: Option<&str>,
    loaded_rate: Option<&str>,
    now: &HashMap<String, String>,
    send_rate: bool,
) -> Option<&'static str> {
    if migration_pending(now) {
        return Some(MIGRATION_PENDING);
    }
    let now_list = now.get(SETTING_AUDIO_OUTPUTS).map(String::as_str);
    if !same_list(loaded_list, now_list) {
        return Some(CHANGED_ON_SERVER);
    }
    let now_rate = now.get(SETTING_AUDIO_NETWORK_RATE).map(String::as_str);
    if send_rate && audio_network_rate(loaded_rate) != audio_network_rate(now_rate) {
        return Some(CHANGED_ON_SERVER);
    }
    None
}

/// The rate the save sends: the chosen one, only when it is not the rate
/// the page loaded (an untouched select never overwrites a rate set
/// elsewhere).
pub fn rate_to_send(loaded_rate: Option<&str>, chosen: &str) -> Option<String> {
    let chosen = chosen.trim();
    (chosen != audio_network_rate(loaded_rate).to_string()).then(|| chosen.to_string())
}

/// A typed number field of an output row: a whole number, else `None`
/// (refused at save, never stored as 0).
pub fn parse_whole(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

/// The Slovak refusal of entry `index`'s `field` that holds no whole
/// number, in `ListError::sk`'s words.
pub fn not_a_number_sk(index: usize, id: &str, field: &'static str) -> String {
    format!(
        "Výstup {} ({}): pole „{}“ nie je celé číslo",
        index + 1,
        shown_id(id),
        field_sk(field)
    )
}

/// The first field of `entries` that holds no whole number, as its Slovak
/// refusal: `bad` lists `(id, field)` pairs typed unreadable since their
/// last good value; a pair whose row is gone does not count.
pub fn first_unreadable(entries: &[OutputEntry], bad: &[(String, &'static str)]) -> Option<String> {
    entries.iter().enumerate().find_map(|(index, e)| {
        bad.iter()
            .find(|(id, _)| *id == e.id)
            .map(|(id, field)| not_a_number_sk(index, id, field))
    })
}

/// The message the section shows: "Uložené" only while the rows and the rate
/// are what was saved (`unchanged`), any other message as it is.
pub fn shown_message(message: &str, unchanged: bool) -> &str {
    if message == SAVED && !unchanged {
        ""
    } else {
        message
    }
}

/// Remove ONE entry with `id`, the first, never every entry of a repeated
/// id (a stored list may hold one twice: the save then names the duplicate
/// until one row is removed).
pub fn remove_one(entries: &mut Vec<OutputEntry>, id: &str) {
    if let Some(i) = entries.iter().position(|e| e.id == id) {
        entries.remove(i);
    }
}

/// A waiting VBAN output's reason codes (`outputs[i].reason_code`): ONE
/// vocabulary, the server's (`sp-server` `audio_out::vban_reason_code`) and
/// the dashboard's Slovak below (review round 1: two crates' bare literals
/// could drift into "neznámy dôvod").
pub const VBAN_NOT_BUILT: &str = "not_built";
pub const VBAN_NOT_STARTED: &str = "not_started";
pub const VBAN_CONVERTER: &str = "converter";
pub const VBAN_UNRESOLVED: &str = "unresolved";
pub const VBAN_RESOLVING: &str = "resolving";

/// A waiting VBAN output's reason in Slovak, by the server's `reason_code`.
pub fn vban_reason_sk(code: &str) -> &'static str {
    match code {
        VBAN_NOT_BUILT => "výstup sa nedá zostaviť",
        VBAN_NOT_STARTED => "vlákno výstupu nebeží — spustí sa znova",
        VBAN_CONVERTER => "prevod frekvencie zlyhal — posiela ticho",
        VBAN_UNRESOLVED => "cieľ sa nedá preložiť na adresu",
        VBAN_RESOLVING => "cieľ sa ešte prekladá na adresu",
        _ => "neznámy dôvod",
    }
}

/// A running output's latency as its row reads it, VBAN's and ASIO's
/// alike: whole ms, rounded half away from zero; "meria sa" while the
/// server reads it 0 (an ASIO output before its servo's first window).
pub fn latency_sk(latency_ms: f64) -> String {
    if latency_ms > 0.0 {
        format!("oneskorenie {} ms", latency_ms.round() as i64)
    } else {
        "oneskorenie: meria sa".to_string()
    }
}

/// A VBAN output's line when it does not run: the state, and why in Slovak
/// when the server names it.
pub fn vban_waiting_text(state: &str, reason_code: Option<&str>) -> String {
    match reason_code {
        Some(code) => format!("{state} · {}", vban_reason_sk(code)),
        None => state.to_string(),
    }
}

#[cfg(test)]
#[path = "audio_outputs_save_tests.rs"]
mod tests;
