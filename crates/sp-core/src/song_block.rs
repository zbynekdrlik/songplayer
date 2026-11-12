//! #265: a per-node song block list — the pure rules of the `/blok` page and
//! of the wall title it searches by.
//!
//! A song blocked on a node (by its YouTube id, every playlist's row of it)
//! is never picked automatically there again; the page's texts, its search
//! over the node's songs and its "song on program" live here (WASM-safe,
//! workspace-tested: sp-ui has no unit tests).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// One blocked song as `GET /api/v1/blocked` lists it and `POST` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedSong {
    pub youtube_id: String,
    /// The song as the wall showed it when it was blocked ([`wall_title`]).
    pub title: String,
    /// When it was first blocked (UTC ms); a second block keeps it.
    pub blocked_at_ms: i64,
}

/// `POST /api/v1/blocked`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRequest {
    pub youtube_id: String,
}

/// The wall's title text: "Song - Artist", the song or the artist alone when
/// the other is empty, empty when both are (the one formatter of the
/// Resolume title, the OBS text and the `/blok` page).
pub fn wall_title(song: &str, artist: &str) -> String {
    match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    }
}

/// A song's shown title: [`wall_title`] of its metadata, else its YouTube
/// title (a song whose metadata is not known yet still has a name).
pub fn shown_title(song: Option<&str>, artist: Option<&str>, youtube_title: &str) -> String {
    let title = wall_title(song.unwrap_or("").trim(), artist.unwrap_or("").trim());
    if title.is_empty() {
        youtube_title.trim().to_string()
    } else {
        title
    }
}

/// The block button: "Zablokovať v <node>", "Zablokovať v tomto uzle" when
/// the node has no name.
pub fn block_label(node_name: Option<&str>) -> String {
    match node_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(node) => format!("Zablokovať v {node}"),
        None => "Zablokovať v tomto uzle".to_string(),
    }
}

/// One song of the node's catalog the page searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSong {
    pub youtube_id: String,
    pub title: String,
}

/// One row of a playlist's song list, as the page reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub row_id: i64,
    pub youtube_id: String,
    /// [`shown_title`] of the row.
    pub title: String,
    /// The row names its song (the server's pick counts only that).
    pub has_metadata: bool,
}

/// The node's songs once each, by title. A song's title is the server's
/// pick (`models_blocked::song_title`): a row with a song name first, then
/// the oldest; a row with no title at all (no song, artist or YouTube
/// title: nothing to search by) is left out.
pub fn catalog(rows: impl IntoIterator<Item = CatalogRow>) -> Vec<CatalogSong> {
    let mut rows: Vec<CatalogRow> = rows.into_iter().filter(|r| !r.title.is_empty()).collect();
    rows.sort_by_key(|r| (!r.has_metadata, r.row_id));
    let mut seen = HashSet::new();
    let mut out: Vec<CatalogSong> = rows
        .into_iter()
        .filter(|r| seen.insert(r.youtube_id.clone()))
        .map(|r| CatalogSong {
            youtube_id: r.youtube_id,
            title: r.title,
        })
        .collect();
    out.sort_by_cached_key(|s| fold(&s.title));
    out
}

/// At most this many search results are shown.
pub const SEARCH_LIMIT: usize = 20;

/// The songs whose title holds every word of `query` (case and Slovak /
/// Spanish diacritics ignored), at most [`SEARCH_LIMIT`]; nothing for an
/// empty query.
pub fn search<'a>(songs: &'a [CatalogSong], query: &str) -> Vec<&'a CatalogSong> {
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    if words.is_empty() {
        return Vec::new();
    }
    songs
        .iter()
        .filter(|s| {
            let title = fold(&s.title);
            words.iter().all(|w| title.contains(w.as_str()))
        })
        .take(SEARCH_LIMIT)
        .collect()
}

/// Lowercase with the diacritics a title carries taken off.
pub fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'ä' | 'à' | 'â' | 'ã' => 'a',
            'č' | 'ç' => 'c',
            'ď' => 'd',
            'é' | 'ě' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ĺ' | 'ľ' => 'l',
            'ň' | 'ñ' => 'n',
            'ó' | 'ô' | 'ö' | 'ò' | 'õ' => 'o',
            'ŕ' | 'ř' => 'r',
            'š' => 's',
            'ť' => 't',
            'ú' | 'ů' | 'ü' | 'ù' | 'û' => 'u',
            'ý' => 'y',
            'ž' => 'z',
            other => other,
        })
        .collect()
}

/// One playlist's now-playing as the page reads it from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramCandidate {
    pub playlist_id: i64,
    /// The playlist's told WS state is `Playing` (= on program, #225).
    pub on_program: bool,
    /// The song it plays (`None` until the song arrived).
    pub video_id: Option<i64>,
}

/// The page's "Teraz na programe" area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramView {
    /// Not every playlist's state was told yet (#225: claim nothing).
    Loading,
    /// Every state is known and no playlist on program plays a song.
    Nothing,
    /// The playlist on program and its song (the lowest playlist id when,
    /// in a cut, two are).
    Song { playlist_id: i64, video_id: i64 },
}

/// What the area shows: a song told to be on program shows at once; with
/// none, "nothing" only once every state is known (`states_known`).
pub fn program_view(states_known: bool, candidates: &[ProgramCandidate]) -> ProgramView {
    let on_program = candidates
        .iter()
        .filter(|c| c.on_program)
        .filter_map(|c| c.video_id.map(|v| (c.playlist_id, v)))
        .min_by_key(|(playlist_id, _)| *playlist_id);
    match on_program {
        Some((playlist_id, video_id)) => ProgramView::Song {
            playlist_id,
            video_id,
        },
        None if states_known => ProgramView::Nothing,
        None => ProgramView::Loading,
    }
}

/// Whether `youtube_id` is in the blocked list.
pub fn is_blocked(blocked: &[BlockedSong], youtube_id: &str) -> bool {
    blocked.iter().any(|b| b.youtube_id == youtube_id)
}

#[cfg(test)]
#[path = "song_block_tests.rs"]
mod tests;
