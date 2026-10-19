//! HTTP push to the Presenter stage-display API. Used by the playback
//! engine's line-change hook (T2.4) to inform band singers what line is
//! sung and what comes next on their stage displays, independently of
//! whatever the audience wall shows.
//!
//! Prod host: http://10.77.9.205/api/stage
//! Dev host:  http://10.77.8.134:8080/api/stage

pub mod client;
pub mod payload;

pub use client::{PresenterClient, PresenterError};
pub use payload::PresenterPayload;

use std::sync::Arc;

use crate::lyrics::renderer::PresenterLines;

/// #222 + #281: what was pushed last — the lines (current + next, EN + SK)
/// AND the song's title (`currentSong`): the push dedup key. A new song is
/// pushed even where its lines are the same as the last song's (both
/// blank).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushedLine {
    pub lines: PresenterLines,
    pub song: String,
}

/// Default Presenter API endpoint when `presenter_url` setting is empty.
pub const DEFAULT_URL: &str = "http://10.77.9.205/api/stage";

/// Build a `PresenterClient` from the two `presenter_*` DB settings, or
/// return None when disabled. Called from lib.rs startup once per process.
#[cfg_attr(test, mutants::skip)]
pub async fn build_from_settings(
    pool: &sqlx::SqlitePool,
) -> Result<Option<Arc<PresenterClient>>, sqlx::Error> {
    let url = crate::db::models::get_setting(pool, "presenter_url")
        .await?
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let enabled = crate::db::models::get_setting(pool, "presenter_enabled")
        .await?
        .map(|s| {
            !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "off" | "no"
            )
        })
        .unwrap_or(true);
    if enabled {
        tracing::info!(%url, "presenter: push enabled");
        Ok(Some(Arc::new(PresenterClient::new(url))))
    } else {
        tracing::info!("presenter: push DISABLED via settings");
        Ok(None)
    }
}

/// The song's title as Presenter shows it (`currentSong`, its snapshot's
/// `songName`): `"<song> - <artist>"`, the song alone with no artist.
pub fn song_title(song: &str, artist: &str) -> String {
    if artist.is_empty() {
        song.to_string()
    } else {
        format!("{song} - {artist}")
    }
}

/// #222: the payload to push for `lines`, or `None` when exactly these lines
/// were pushed last (`last_seen`): the dedup key is the WHOLE payload,
/// current + next, EN + SK, and (#281) the song's title. A Slovak line that
/// arrives later under the same English one is pushed, and so is a repeated
/// sentence whose next line changed ("What a God, what a God." ×4: the 4th
/// repeat carries the line after the repeats). Both languages always go out
/// (Presenter's stage layout picks what to show); a line with no translation
/// sends "".
pub fn payload_for(
    last_seen: Option<&PushedLine>,
    lines: &PresenterLines,
    song: &str,
    artist: &str,
) -> Option<PresenterPayload> {
    let current_song = song_title(song, artist);
    if last_seen.is_some_and(|seen| seen.lines == *lines && seen.song == current_song) {
        return None;
    }
    Some(PresenterPayload {
        // Wrap long lyric lines so they don't overflow the stage display;
        // many source lines are 40-60 chars and become unreadable on a
        // phone/tablet without breaks. Only the live-lyric fields are
        // wrapped — `currentSong` stays one line on purpose.
        current_text: payload::wrap_for_presenter(&lines.current_en),
        next_text: payload::wrap_for_presenter(&lines.next_en),
        current_song,
        next_song: String::new(),
        current_translation: payload::wrap_for_presenter(&lines.current_sk),
        next_translation: payload::wrap_for_presenter(&lines.next_sk),
    })
}

/// Line-change push helper used by the playback engine hot path. Hands a
/// fire-and-forget push to `PresenterClient::send` (the newest wins) when `payload_for` has a
/// payload (a line or the song's title changed), and returns the new `last_seen`
/// for the caller to persist. No-op when `client` is None (push disabled).
#[cfg_attr(test, mutants::skip)] // send glue; `payload_for` is the tested decision
pub fn maybe_push_line(
    client: Option<&Arc<PresenterClient>>,
    last_seen: Option<PushedLine>,
    lines: PresenterLines,
    song: &str,
    artist: &str,
) -> Option<PushedLine> {
    let Some(client) = client else {
        return last_seen;
    };
    let Some(payload) = payload_for(last_seen.as_ref(), &lines, song, artist) else {
        return last_seen;
    };
    let pushed = PushedLine {
        lines,
        song: payload.current_song.clone(),
    };
    client.send(payload, "a line or the title");
    Some(pushed)
}

/// #283 + #281: the stage display at a position of a playing song: the
/// plan's `lines` through [`maybe_push_line`], or, where the wall is blank
/// (`None`: no lyrics, before the first line, a break's blank stretch, after
/// the last line's hide), empty lines with the song's title KEPT, pushed
/// once like any payload (the same dedup key), so the stage display shows
/// no stale line and the title stays for the whole song; the next line is
/// pushed again. The title goes only with the song ([`push_empty`] at its
/// end, at a scene off with no owner). Without a client nothing is pushed
/// and the key is kept.
pub fn sync_line(
    client: Option<&Arc<PresenterClient>>,
    last_seen: Option<PushedLine>,
    lines: Option<PresenterLines>,
    song: &str,
    artist: &str,
) -> Option<PushedLine> {
    maybe_push_line(client, last_seen, lines.unwrap_or_default(), song, artist)
}

/// Clear the stage display (`PresenterPayload::empty`), sent like
/// [`maybe_push_line`]; `why` names the caller in the failure's WARN. No-op
/// when `client` is None (push disabled).
#[cfg_attr(test, mutants::skip)] // queue glue; the engine tests see the push
pub fn push_empty(client: Option<&Arc<PresenterClient>>, why: &'static str) {
    if let Some(client) = client {
        client.send(PresenterPayload::empty(), why);
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
