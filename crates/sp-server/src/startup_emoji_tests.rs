//! #135 tests for [`self_heal_emoji_metadata`]. Sibling of `startup.rs`,
//! wired via `#[path = "startup_emoji_tests.rs"]` so `startup.rs` stays under
//! the 1000-line airuleset cap (moved out verbatim for #136).

use super::*;
use crate::db;

async fn seed_pool() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name)
         VALUES (1, 'p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// RED: mirrors the live #135 E2E failure — a stored row whose
/// `gemini_failed = 1` (every provider keeps failing on this video, so
/// it can never be healed by a provider retry) carries an emoji in
/// `artist` written before the sanitizer choke point existed. The
/// startup self-heal pass must clean it in place.
#[tokio::test]
async fn heals_dirty_stored_row_with_emoji() {
    let pool = seed_pool().await;
    sqlx::query(
        "INSERT INTO videos
            (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
         VALUES (1, '0HQOYVf6-Yg', 't', 'Our God + The Blessing',
                 'Christian Afro House 2025 \u{1F525}', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let healed = self_heal_emoji_metadata(&pool).await.unwrap();
    assert_eq!(healed, 1, "exactly the dirty row should be healed");

    let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = '0HQOYVf6-Yg'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let song: String = row.get("song");
    let artist: String = row.get("artist");
    assert_eq!(song, "Our God + The Blessing");
    assert_eq!(
        artist, "Christian Afro House 2025",
        "self-heal must strip emoji from the stored artist"
    );
}

/// A row already clean (no emoji, `gemini_failed = 0`) must be left
/// untouched — proves the pass doesn't rewrite every row on every boot.
#[tokio::test]
async fn skips_already_clean_rows() {
    let pool = seed_pool().await;
    sqlx::query(
        "INSERT INTO videos
            (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
         VALUES (1, 'clean1', 't', 'The Blessing', 'Elevation Worship', 0, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let healed = self_heal_emoji_metadata(&pool).await.unwrap();
    assert_eq!(healed, 0, "no row should be touched when already clean");

    let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = 'clean1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("song"), "The Blessing");
    assert_eq!(row.get::<String, _>("artist"), "Elevation Worship");
}

/// A row with NULL song/artist (never processed yet) must stay NULL —
/// the sanitizer must never turn an absent value into `""`.
#[tokio::test]
async fn preserves_null_song_and_artist() {
    let pool = seed_pool().await;
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, gemini_failed, normalized)
         VALUES (1, 'unprocessed', 't', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let healed = self_heal_emoji_metadata(&pool).await.unwrap();
    assert_eq!(healed, 0, "a NULL song/artist row is not dirty");

    let row = sqlx::query("SELECT song, artist FROM videos WHERE youtube_id = 'unprocessed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let song: Option<String> = row.get("song");
    let artist: Option<String> = row.get("artist");
    assert_eq!(song, None, "song must stay NULL, never coerced to \"\"");
    assert_eq!(artist, None, "artist must stay NULL, never coerced to \"\"");
}
