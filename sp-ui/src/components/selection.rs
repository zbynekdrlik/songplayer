//! #165: selection-state helpers for the redesigned dashboard — which playlist
//! the single work area shows, ordering + glyphs for the selector, and the
//! URL-query + localStorage persistence.

use std::collections::HashMap;

use leptos::prelude::Set;
use sp_core::models::Playlist;
use sp_core::playback::PlaybackState;
use wasm_bindgen::JsValue;

use crate::store::{DashboardStore, NowPlayingInfo};

const STORAGE_KEY: &str = "sp_selected_playlist";

/// Playback glyph for a selector row: ▶ Playing, ⏸ WaitingForScene
/// ("Paused — waiting for scene"), – idle / unknown.
pub fn playback_glyph(np: &HashMap<i64, NowPlayingInfo>, id: i64) -> &'static str {
    match np.get(&id).map(|i| i.state) {
        Some(PlaybackState::Playing) => "▶",
        Some(PlaybackState::WaitingForScene) => "⏸",
        _ => "–",
    }
}

/// Is this playlist currently Playing?
pub fn is_playing(np: &HashMap<i64, NowPlayingInfo>, id: i64) -> bool {
    matches!(np.get(&id).map(|i| i.state), Some(PlaybackState::Playing))
}

/// Selector display order: STABLE, alphabetical (case-insensitive) by name.
///
/// #170: NOT playing-first. A row must never jump under the operator's cursor
/// when a playlist's playback state changes — the ▶ glyph and the "Práve hrá"
/// strip already mark the playing one, and the selector re-ordering on every
/// `now_playing` tick was the click-race that failed post-deploy test 16.
/// The order therefore depends only on the playlist set, never on `now_playing`.
pub fn ordered(playlists: &[Playlist]) -> Vec<Playlist> {
    let mut v = playlists.to_vec();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

/// The INITIAL default selection: the currently-playing playlist (first by
/// name if several), else the first playlist alphabetically. #170: the
/// selector order is no longer playing-first, so this preserves "preselect
/// what's playing on a fresh load" explicitly via `first_playing`. `None`
/// only for an empty list.
pub fn choose_default(playlists: &[Playlist], np: &HashMap<i64, NowPlayingInfo>) -> Option<i64> {
    first_playing(playlists, np).or_else(|| ordered(playlists).first().map(|p| p.id))
}

/// The first currently-playing playlist id (first by name if several).
pub fn first_playing(playlists: &[Playlist], np: &HashMap<i64, NowPlayingInfo>) -> Option<i64> {
    let mut playing: Vec<&Playlist> = playlists.iter().filter(|p| is_playing(np, p.id)).collect();
    playing.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    playing.first().map(|p| p.id)
}

// --------------------------- persistence (best-effort) ----------------------

/// Read the persisted selection: URL `?playlist=<id>` first, else localStorage.
pub fn persisted_selection() -> Option<i64> {
    if let Some(id) = read_url_playlist() {
        return Some(id);
    }
    read_local_storage()
}

fn read_url_playlist() -> Option<i64> {
    let search = web_sys::window()?.location().search().ok()?;
    parse_playlist_query(&search)
}

/// Parse `?playlist=<id>` out of a raw `location.search` string. Pure so it is
/// unit-testable without a browser.
pub fn parse_playlist_query(search: &str) -> Option<i64> {
    let s = search.strip_prefix('?').unwrap_or(search);
    s.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "playlist")
        .and_then(|(_, v)| v.parse::<i64>().ok())
}

fn read_local_storage() -> Option<i64> {
    let storage = web_sys::window()?.local_storage().ok()??;
    storage.get_item(STORAGE_KEY).ok()??.parse::<i64>().ok()
}

/// Persist the selection to the URL query + localStorage (best-effort — a
/// private window can throw on `local_storage`, a missing `history` API no-ops).
pub fn persist(id: i64) {
    if let Some(win) = web_sys::window() {
        if let Ok(Some(storage)) = win.local_storage() {
            let _ = storage.set_item(STORAGE_KEY, &id.to_string());
        }
        if let Ok(history) = win.history() {
            let path = win
                .location()
                .pathname()
                .unwrap_or_else(|_| "/".to_string());
            let url = format!("{path}?playlist={id}");
            let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&url));
        }
    }
}

/// Set + pin the selection and persist it. The single entry point for every
/// user-driven selection (selector row click, `<select>` change, "Prejsť").
pub fn select(store: DashboardStore, id: i64) {
    store.selected_playlist.set(Some(id));
    store.selection_pinned.set(true);
    persist(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_playlist_query_reads_the_id() {
        assert_eq!(parse_playlist_query("?playlist=7"), Some(7));
        assert_eq!(parse_playlist_query("?foo=1&playlist=42&bar=2"), Some(42));
        assert_eq!(parse_playlist_query("playlist=3"), Some(3));
        assert_eq!(parse_playlist_query("?foo=1"), None);
        assert_eq!(parse_playlist_query("?playlist=abc"), None);
        assert_eq!(parse_playlist_query(""), None);
    }
}
