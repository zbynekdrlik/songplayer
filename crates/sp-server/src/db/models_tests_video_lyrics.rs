//! #144 F1: the lyrics state describes `{yt}_lyrics.json`, which is per
//! YouTube video, so every lyrics writer reaches EVERY row of the video (a
//! video in two playlists has two rows). Before, each writer was keyed by the
//! row: row 288 kept ★ for the base-tier lines sibling row 306 wrote.
//! Included from `models_tests_lyrics.rs` (`#[path]`), so `models.rs`'s
//! module block stays untouched.

use crate::db;
use crate::db::models::*;
use sqlx::SqlitePool;

/// Rows 1 and 2 are the same video (`same`) in two playlists; row 3 is
/// another video, which no writer of `same` may touch.
async fn siblings() -> SqlitePool {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES \
         (1, 'a', 'u1', 1), (2, 'b', 'u2', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_manual_priority) \
         VALUES (1, 1, 'same', 1, 1), (2, 2, 'same', 1, 1), (3, 1, 'other', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// `(has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_reference,
/// lyrics_manual_priority)` of one row.
async fn state(pool: &SqlitePool, id: i64) -> (i64, Option<String>, i64, i64, i64) {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_reference, \
         lyrics_manual_priority FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn untouched(pool: &SqlitePool) {
    assert_eq!(state(pool, 3).await, (0, None, 0, 0, 1), "another video");
}

#[tokio::test]
async fn a_completed_pass_writes_every_row_of_the_video() {
    let pool = siblings().await;
    mark_video_lyrics_complete(&pool, 1, "gemini-3-5-transcribe", 22, None, None)
        .await
        .unwrap();
    let want = (1, Some("gemini-3-5-transcribe".to_string()), 22, 0, 0);
    assert_eq!(state(&pool, 1).await, want);
    assert_eq!(state(&pool, 2).await, want, "the sibling row");
    untouched(&pool).await;
}

/// ★ is the track's: a gate PASS's source (`…/g35t-ok`) sets it on every
/// row, the next track that is not one clears it on every row.
#[tokio::test]
async fn the_star_follows_the_completed_track_on_every_row() {
    let pool = siblings().await;
    mark_video_lyrics_complete(&pool, 2, "lrclib+mtl@rev1/g35t-ok", 22, None, None)
        .await
        .unwrap();
    assert_eq!(state(&pool, 1).await.3, 1);
    assert_eq!(state(&pool, 2).await.3, 1);
    mark_video_lyrics_complete(&pool, 1, "gemini-3-5-transcribe", 22, None, None)
        .await
        .unwrap();
    assert_eq!(state(&pool, 1).await.3, 0);
    assert_eq!(state(&pool, 2).await.3, 0, "no ★ over base-tier lines");
    untouched(&pool).await;
}

#[tokio::test]
async fn a_terminal_pass_parks_every_row_of_the_video() {
    let pool = siblings().await;
    mark_video_lyrics(&pool, 1, false, Some("no_source"), 22)
        .await
        .unwrap();
    assert_eq!(
        state(&pool, 2).await,
        (0, Some("no_source".to_string()), 22, 0, 0)
    );
    let pool = siblings().await;
    mark_unsupported_source(&pool, 2, 22).await.unwrap();
    assert_eq!(
        state(&pool, 1).await,
        (0, Some("unsupported_source".to_string()), 22, 0, 0)
    );
    untouched(&pool).await;
}

#[tokio::test]
async fn a_quarantine_parks_every_row_of_the_video() {
    let pool = siblings().await;
    let dir = tempfile::tempdir().unwrap();
    quarantine_video_lyrics(&pool, 1, dir.path(), "empty transcript", 22)
        .await
        .unwrap();
    assert_eq!(
        state(&pool, 2).await,
        (0, Some("asr_gap".to_string()), 22, 0, 0)
    );
    untouched(&pool).await;
}

/// `(lyrics_attempts, lyrics_next_attempt_at is set)` of one row.
async fn backoff(pool: &SqlitePool, id: i64) -> (i64, bool) {
    sqlx::query_as(
        "SELECT lyrics_attempts, lyrics_next_attempt_at IS NOT NULL FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_backoff_reaches_every_row_of_the_video() {
    let pool = siblings().await;
    record_lyrics_deferral(&pool, 1, std::time::Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(backoff(&pool, 2).await, (1, true), "a deferral");
    let pool = siblings().await;
    record_lyrics_wait(&pool, 2, std::time::Duration::from_secs(600))
        .await
        .unwrap();
    assert_eq!(backoff(&pool, 1).await, (0, true), "a wait");
    let pool = siblings().await;
    record_served_lyrics_failure(&pool, 1, std::time::Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(backoff(&pool, 2).await, (1, true), "a served failure");
    assert_eq!(backoff(&pool, 3).await, (0, false), "another video");
}

#[tokio::test]
async fn the_star_toggle_and_the_owners_mark_reach_every_row_of_the_video() {
    let pool = siblings().await;
    assert_eq!(set_video_lyrics_reference(&pool, 1, true).await.unwrap(), 2);
    assert_eq!(state(&pool, 2).await.3, 1, "the admin ★");
    record_reference_feedback(&pool, 2, "refrén nesedí")
        .await
        .unwrap();
    let (reference, note): (i64, Option<String>) =
        sqlx::query_as("SELECT lyrics_reference, lyrics_reference_note FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((reference, note.as_deref()), (0, Some("refrén nesedí")));
    untouched(&pool).await;
}

/// `(lyrics_translation_gender, lyrics_translation_version)` of one row.
async fn translation(pool: &SqlitePool, id: i64) -> (Option<String>, i64) {
    sqlx::query_as(
        "SELECT lyrics_translation_gender, lyrics_translation_version FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The SK lines are in the one file: the gender and the version stamp are
/// the video's.
#[tokio::test]
async fn the_translation_gender_and_version_reach_every_row_of_the_video() {
    let pool = siblings().await;
    stamp_translation_version(&pool, 1, 4).await.unwrap();
    assert_eq!(translation(&pool, 2).await, (None, 4));
    assert!(set_translation_gender(&pool, 2, Some("f")).await.unwrap());
    assert_eq!(translation(&pool, 1).await, (Some("f".to_string()), 0));
    assert_eq!(translation(&pool, 3).await, (None, 0), "another video");
}

/// A terminal pass leaves no ★: a parked video serves no ★ text.
#[tokio::test]
async fn a_terminal_pass_clears_the_star_on_every_row_of_the_video() {
    let pool = siblings().await;
    mark_video_lyrics_complete(&pool, 1, "lrclib+mtl@rev1/g35t-ok", 22, None, None)
        .await
        .unwrap();
    mark_video_lyrics(&pool, 2, false, Some("no_source"), 22)
        .await
        .unwrap();
    assert_eq!((state(&pool, 1).await.3, state(&pool, 2).await.3), (0, 0));
    mark_video_lyrics_complete(&pool, 1, "lrclib+mtl@rev1/g35t-ok", 22, None, None)
        .await
        .unwrap();
    mark_unsupported_source(&pool, 1, 22).await.unwrap();
    assert_eq!((state(&pool, 1).await.3, state(&pool, 2).await.3), (0, 0));
}

/// The operator's override text is the video's lyrics input: it reaches
/// every row (a pass of any row serves every row).
#[tokio::test]
async fn the_override_text_spreads_to_every_row_of_the_video() {
    let pool = siblings().await;
    sqlx::query("UPDATE videos SET lyrics_override_text = 'Moj text' WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    spread_lyrics_override(&pool, 2).await.unwrap();
    let texts: Vec<Option<String>> =
        sqlx::query_scalar("SELECT lyrics_override_text FROM videos ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        texts,
        vec![
            Some("Moj text".to_string()),
            Some("Moj text".to_string()),
            None
        ]
    );
}

/// A video with a dub-requested row in any playlist is never a lyrics job:
/// its one `{yt}_lyrics.json` is the dub's subtitles.
#[tokio::test]
async fn a_video_dubbed_in_another_playlist_is_no_lyrics_job() {
    let pool = siblings().await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    let next = crate::lyrics::reprocess::get_next_video_for_lyrics(
        &pool,
        crate::lyrics::LYRICS_PIPELINE_VERSION,
    )
    .await
    .unwrap()
    .expect("another video is due");
    assert_eq!(next.id, 3);
}
