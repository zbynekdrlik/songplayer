//! #228: no worker queue takes the test item, and no dashboard count waits
//! for it. Each test puts a test-item row FIRST (the lower id, the newer
//! request, the manual priority) next to an identical row of a normal
//! playlist: the queue takes the normal one, and with the normal one gone,
//! nothing. Wired via `#[cfg(test)] #[path = "test_item_tests_queues.rs"]
//! mod tests_queues;` in `test_item.rs`.

use sqlx::SqlitePool;

use super::ensure_playlist;
use crate::lyrics::LYRICS_PIPELINE_VERSION;

/// The rows of one queue test: the test item's, then a normal one.
struct Rows {
    pool: SqlitePool,
    test: i64,
    normal: i64,
    test_playlist: i64,
}

/// A downloaded video (an audio sidecar, no lyrics, no stems) in the test
/// playlist and the same in a normal playlist; `set` is applied to both.
async fn rows(set: &str) -> Rows {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let test_playlist = ensure_playlist(&pool).await.unwrap();
    let normal_playlist: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) \
         VALUES ('fast', 'https://youtube.com/playlist?list=fast', 'SP-fast') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for (playlist, youtube_id) in [
        (test_playlist, "measure-v01"),
        (normal_playlist, "normal-v001"),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, song, artist, normalized, \
             file_path, audio_file_path) VALUES (?, ?, 'Song', 'Artist', 1, ?, ?) RETURNING id",
        )
        .bind(playlist)
        .bind(youtube_id)
        .bind(format!("/cache/{youtube_id}_video.mp4"))
        .bind(format!("/cache/{youtube_id}_audio.flac"))
        .fetch_one(&pool)
        .await
        .unwrap();
        ids.push(id);
    }
    if !set.is_empty() {
        sqlx::query(&format!("UPDATE videos SET {set}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    Rows {
        pool,
        test: ids[0],
        normal: ids[1],
        test_playlist,
    }
}

impl Rows {
    /// Only the test item's row is left.
    async fn drop_normal(&self) {
        sqlx::query("DELETE FROM videos WHERE id = ?")
            .bind(self.normal)
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_lyrics_queue_never_takes_the_test_item() {
    let rows = rows("lyrics_manual_priority = 1").await;
    assert!(rows.test < rows.normal);
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(next.map(|r| r.id), Some(rows.normal));
    let (manual, null, _) =
        crate::api::lyrics::fetch_queue_counts(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(manual, 1, "the dashboard's manual count");

    sqlx::query("UPDATE videos SET lyrics_manual_priority = 0")
        .execute(&rows.pool)
        .await
        .unwrap();
    let (_, null_after, _) =
        crate::api::lyrics::fetch_queue_counts(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!((null, null_after), (0, 1), "the no-lyrics count");

    rows.drop_normal().await;
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn the_stem_queue_never_takes_the_test_item() {
    let rows = rows("").await;
    sqlx::query("UPDATE videos SET stem_manual_priority = 1 WHERE id = ?")
        .bind(rows.test)
        .execute(&rows.pool)
        .await
        .unwrap();
    let on_program = [rows.test_playlist];
    let next = crate::db::models_stems_priority::get_next_stem_job(&rows.pool, &on_program, &[])
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.normal));
    let next = crate::db::models_stems::get_next_video_for_stems(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.normal));
    let (pending, _) = crate::db::models_stems::count_stems_progress(&rows.pool)
        .await
        .unwrap();
    assert_eq!(pending, 1, "the dashboard's stems count");

    rows.drop_normal().await;
    let next = crate::db::models_stems_priority::get_next_stem_job(&rows.pool, &on_program, &[])
        .await
        .unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn the_dub_queue_never_takes_the_test_item() {
    let rows = rows("dub_requested = 1, dub_status = 'queued'").await;
    sqlx::query("UPDATE videos SET dub_requested_at = '2026-10-10T00:00:00.000Z' WHERE id = ?")
        .bind(rows.test)
        .execute(&rows.pool)
        .await
        .unwrap();
    let next = crate::db::models_dabing::get_next_dub_job(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.normal));
    rows.drop_normal().await;
    let next = crate::db::models_dabing::get_next_dub_job(&rows.pool)
        .await
        .unwrap();
    assert!(next.is_none());
}

/// The test item's row is `manual` anyway; the fragment keeps it out even
/// as a parser-titled row.
#[tokio::test]
async fn the_metadata_repair_never_takes_the_test_item() {
    let rows = rows("gemini_failed = 1, metadata_source = NULL").await;
    let failed = crate::metadata::health::failed_videos(&rows.pool)
        .await
        .unwrap();
    assert_eq!(failed, 1);
    rows.drop_normal().await;
    let failed = crate::metadata::health::failed_videos(&rows.pool)
        .await
        .unwrap();
    assert_eq!(failed, 0);
}

#[tokio::test]
async fn the_download_queue_never_takes_the_test_item() {
    let rows = rows("normalized = 0").await;
    let next = crate::downloader::fetch_next_unprocessed(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|v| v.id), Some(rows.normal));
    rows.drop_normal().await;
    let next = crate::downloader::fetch_next_unprocessed(&rows.pool)
        .await
        .unwrap();
    assert!(next.is_none());
}
