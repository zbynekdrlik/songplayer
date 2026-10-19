//! #283: the lyrics lookups ask for the artist IN FULL.
//!
//! The metadata prompt shortens a person's first and middle names to
//! initials for the wall's title (`metadata/gemini.rs`: "P. Wickham",
//! "B. W. Hastings"; the parser does the same), and LRCLIB, lyrics.ovh and
//! Genius, asked for "P. Wickham", miss the song (SNV, 19.10.2026: 112
//! rows). [`full_artist`] takes each `X. Y. Surname` of the display artist
//! back to the name the video's own title gives ("Homesick For Heaven -
//! Phil Wickham | …" → "Phil Wickham"); a name the title does not spell out
//! stays as it is. [`expand_lookup_artist`] puts it on the row the lyrics
//! worker processes, right before its lookups; the display artist in the
//! database and on the wall is untouched.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::db::models::VideoLyricsRow;

/// One or more capital initials, each with its period, then a surname.
static INITIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b((?:\p{Lu}\.\.\s*)+)(\p{Lu}[\p{L}\p{M}'’-]*)").expect("a valid pattern")
});

/// The title's spelling of the person `initials` + `surname` names: each
/// initial starts a word (a full name, or the initial again), then the
/// surname, any case.
fn in_title<'t>(initials: &str, surname: &str, title: &'t str) -> Option<&'t str> {
    let mut pattern = String::from(r"(?i)\b");
    for letter in initials.chars().filter(|c| c.is_alphabetic()) {
        pattern.push_str(&regex::escape(&letter.to_string()));
        pattern.push_str(r"[\p{L}\p{M}'’-]*\.?\s+");
    }
    pattern.push_str(&regex::escape(surname));
    pattern.push_str(r"\b");
    let found = Regex::new(&pattern).ok()?.find(title)?;
    Some(found.as_str())
}

/// `artist` with every `X. Surname` / `X. Y. Surname` the video's `title`
/// spells out in full replaced by the title's spelling; borrowed when
/// nothing changes (no initials, or none the title names).
pub fn full_artist<'a>(artist: &'a str, title: &str) -> Cow<'a, str> {
    let mut out = String::new();
    let mut last = 0;
    for name in INITIALS.captures_iter(artist) {
        let whole = name.get(0).expect("group 0 is the match");
        let Some(full) = in_title(&name[1], &name[2], title) else {
            continue;
        };
        out.push_str(&artist[last..whole.start()]);
        out.push_str(full);
        last = whole.end();
    }
    if out.is_empty() {
        return Cow::Borrowed(artist);
    }
    out.push_str(&artist[last..]);
    if out == artist {
        Cow::Borrowed(artist)
    } else {
        Cow::Owned(out)
    }
}

/// #283: the artist the lyrics worker's lookups use for `row`: its display
/// artist made full from the video's title ([`full_artist`]), one INFO when
/// it changes. A title that cannot be read leaves the artist as it is.
pub(crate) async fn expand_lookup_artist(pool: &SqlitePool, row: &mut VideoLyricsRow) {
    let title = sqlx::query_scalar::<_, Option<String>>("SELECT title FROM videos WHERE id = ?")
        .bind(row.id)
        .fetch_optional(pool)
        .await;
    let title = match title {
        Ok(title) => title.flatten().unwrap_or_default(),
        Err(e) => {
            warn!(video_id = row.id, %e, "lyrics: reading the title for the lookups failed");
            return;
        }
    };
    let full = match full_artist(&row.artist, &title) {
        Cow::Owned(full) => full,
        Cow::Borrowed(_) => return,
    };
    info!(
        video_id = row.id,
        display = %row.artist,
        lookup = %full,
        "lyrics: the lookups ask for the artist in full (#283)"
    );
    row.artist = full;
}

#[cfg(test)]
#[path = "full_artist_tests.rs"]
mod tests;
