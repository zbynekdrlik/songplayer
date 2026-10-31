//! #283: the lyrics lookups ask for the artist in full, from the video's
//! own title; the display artist stays.
//! Wired via `#[cfg(test)] #[path = "full_artist_tests.rs"] mod tests;` in `full_artist.rs`.

use std::borrow::Cow;

use super::{expand_lookup_artist, full_artist};
use crate::db::models::VideoLyricsRow;

#[test]
fn an_initial_and_surname_become_the_name_the_title_gives() {
    for (artist, title, want) in [
        (
            "P. Wickham",
            "Homesick For Heaven - Phil Wickham | Live From Nashville",
            "Phil Wickham",
        ),
        (
            "B. W. Hastings",
            "Still You (from the studio) // Benjamin William Hastings",
            "Benjamin William Hastings",
        ),
        (
            "Church of the City & D. Leonard",
            "Lamb of God // Church of the City feat. David Leonard // Worship Together",
            "Church of the City & David Leonard",
        ),
        (
            "Bethel Music & C. Asbury",
            "Reason To Praise - Cory Asbury feat. Naomi Raine",
            "Bethel Music & Cory Asbury",
        ),
        (
            "C. Winans",
            "CeCe Winans - Holy Forever (Official Music Video)",
            "CeCe Winans",
        ),
        (
            "D. Zschech, M. Wong",
            "Darlene Zschech - Yes Again (Mitch Wong) (Music Video)",
            "Darlene Zschech, Mitch Wong",
        ),
        (
            "C. Tomlin",
            "Holy Forever // CHRIS TOMLIN // Worship Together Session",
            "CHRIS TOMLIN",
        ),
    ] {
        assert_eq!(full_artist(artist, title), want, "{artist:?} in {title:?}");
    }
}

#[test]
fn a_name_the_title_does_not_spell_out_stays_as_it_is() {
    for (artist, title) in [
        // No space between the names in the title.
        (
            "J. Wilds",
            "He Has Done Great Things (Live) - @JohnWilds , Bethel Music",
        ),
        // The title names nobody of that surname.
        ("T. Galberth", "Proclaim (feat. D'nar)"),
        // The title spells the initial itself.
        ("P. Wickham", "Homesick For Heaven - P. Wickham"),
        // No initials in the artist.
        ("Phil Wickham", "Homesick For Heaven - Phil Wickham"),
        ("Elevation Worship", "Praise - Elevation Worship"),
        // Another first letter.
        ("K. Wickham", "Homesick For Heaven - Phil Wickham"),
        ("", "Homesick For Heaven - Phil Wickham"),
    ] {
        assert!(
            matches!(full_artist(artist, title), Cow::Borrowed(a) if a == artist),
            "{artist:?} in {title:?}"
        );
    }
}

/// Only the names the title spells out change; the rest of the artist
/// keeps its own text.
#[test]
fn only_the_names_the_title_spells_change() {
    assert_eq!(
        full_artist(
            "T. Galberth & D. Leonard",
            "Just Jesus | David Leonard | Worship Together Session"
        ),
        "T. Galberth & David Leonard"
    );
}

fn row(id: i64, artist: &str) -> VideoLyricsRow {
    VideoLyricsRow {
        id,
        youtube_id: format!("yt{id}"),
        song: "Homesick For Heaven".into(),
        artist: artist.into(),
        duration_ms: Some(255_000),
        audio_file_path: None,
        youtube_url: String::new(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
        spotify_resolved_at: None,
    }
}

/// The worker's row takes the full name from the video's title in the
/// database; the stored display artist is untouched, and a video with no
/// row keeps its artist.
#[tokio::test]
async fn the_worker_s_row_asks_for_the_artist_the_title_names() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'SP-test', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist) \
         VALUES (242, 1, 'yt242', 'Homesick For Heaven - Phil Wickham | Live', \
         'Homesick For Heaven', 'P. Wickham')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut r = row(242, "P. Wickham");
    expand_lookup_artist(&pool, &mut r).await;
    assert_eq!(r.artist, "Phil Wickham");
    let stored: String = sqlx::query_scalar("SELECT artist FROM videos WHERE id = 242")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "P. Wickham", "the display artist stays");
    let mut none = row(999, "P. Wickham");
    expand_lookup_artist(&pool, &mut none).await;
    assert_eq!(none.artist, "P. Wickham", "no row, no title");
}
