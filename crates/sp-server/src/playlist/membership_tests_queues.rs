//! #260: no worker queue starts a job for a row whose song left its
//! YouTube playlist, and no dashboard count waits for it (`db::queue_row!`;
//! the translation and register passes by their own `v.removed_at IS
//! NULL`). Each test puts the MARKED row first (the lower id, the newer
//! request, the manual priority) next to an identical row of a song still
//! in the playlist: the queue takes that one, and with it marked too,
//! nothing. Modelled on `test_item_tests_queues.rs` (#228).

use sqlx::SqlitePool;

use crate::lyrics::LYRICS_PIPELINE_VERSION;

/// The rows of one queue test: the marked one, then one still listed.
struct Rows {
    pool: SqlitePool,
    marked: i64,
    listed: i64,
    playlist: i64,
}

/// Two downloaded videos (an audio sidecar, no lyrics, no stems) of one
/// active playlist, the first marked as gone from it; `set` is applied to
/// both.
async fn rows(set: &str) -> Rows {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let playlist: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) \
         VALUES ('fast', 'https://youtube.com/playlist?list=fast', 'SP-fast') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for youtube_id in ["gone-v00001", "kept-v00001"] {
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
    let rows = Rows {
        pool,
        marked: ids[0],
        listed: ids[1],
        playlist,
    };
    rows.mark(rows.marked).await;
    rows
}

impl Rows {
    /// `id`'s song left the YouTube playlist (the sync's mark).
    async fn mark(&self, id: i64) {
        sqlx::query("UPDATE videos SET removed_at = 1760000000000 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// The song still listed leaves the playlist too.
    async fn mark_listed(&self) {
        self.mark(self.listed).await;
    }
}

#[tokio::test]
async fn the_lyrics_queue_never_takes_a_song_that_left() {
    let rows = rows("lyrics_manual_priority = 1").await;
    assert!(rows.marked < rows.listed);
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(next.map(|r| r.id), Some(rows.listed));
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

    rows.mark_listed().await;
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert!(next.is_none());
}

/// The stale bucket (served lyrics of an older pipeline) and its count.
#[tokio::test]
async fn the_stale_lyrics_bucket_never_takes_a_song_that_left() {
    let stale = LYRICS_PIPELINE_VERSION - 1;
    let rows = rows(&format!(
        "has_lyrics = 1, lyrics_source = 'gemini-3-5-transcribe', \
         lyrics_pipeline_version = {stale}"
    ))
    .await;
    let next =
        crate::lyrics::reprocess::get_next_video_for_lyrics(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(next.map(|r| r.id), Some(rows.listed));
    let (_, _, stale_count) =
        crate::api::lyrics::fetch_queue_counts(&rows.pool, LYRICS_PIPELINE_VERSION)
            .await
            .unwrap();
    assert_eq!(stale_count, 1, "the dashboard's stale count");
}

#[tokio::test]
async fn the_stem_queue_never_takes_a_song_that_left() {
    let rows = rows("").await;
    sqlx::query("UPDATE videos SET stem_manual_priority = 1 WHERE id = ?")
        .bind(rows.marked)
        .execute(&rows.pool)
        .await
        .unwrap();
    let on_program = [rows.playlist];
    let next = crate::db::models_stems_priority::get_next_stem_job(&rows.pool, &on_program, &[])
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.listed));
    let next = crate::db::models_stems::get_next_video_for_stems(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.listed));
    let counts = crate::db::models_stems::count_stems_progress(&rows.pool)
        .await
        .unwrap();
    assert_eq!(
        counts,
        (1, 0),
        "the dashboard's stems counts: pending, done"
    );

    rows.mark_listed().await;
    let next = crate::db::models_stems_priority::get_next_stem_job(&rows.pool, &on_program, &[])
        .await
        .unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn the_dub_queue_never_takes_a_song_that_left() {
    let rows = rows("dub_requested = 1, dub_status = 'queued'").await;
    sqlx::query("UPDATE videos SET dub_requested_at = '2026-10-10T00:00:00.000Z' WHERE id = ?")
        .bind(rows.marked)
        .execute(&rows.pool)
        .await
        .unwrap();
    let next = crate::db::models_dabing::get_next_dub_job(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|j| j.video_id), Some(rows.listed));
    rows.mark_listed().await;
    let next = crate::db::models_dabing::get_next_dub_job(&rows.pool)
        .await
        .unwrap();
    assert!(next.is_none());
}

/// The repair asks the paid providers: never for a song that left.
#[tokio::test]
async fn the_metadata_repair_never_takes_a_song_that_left() {
    let rows = rows("gemini_failed = 1, metadata_source = NULL").await;
    let failed = crate::metadata::health::failed_videos(&rows.pool)
        .await
        .unwrap();
    assert_eq!(failed, 1);
    rows.mark_listed().await;
    let failed = crate::metadata::health::failed_videos(&rows.pool)
        .await
        .unwrap();
    assert_eq!(failed, 0);
}

/// A song that left before it was downloaded is never downloaded.
#[tokio::test]
async fn the_download_queue_never_takes_a_song_that_left() {
    let rows = rows("normalized = 0").await;
    let next = crate::downloader::fetch_next_unprocessed(&rows.pool)
        .await
        .unwrap();
    assert_eq!(next.map(|v| v.id), Some(rows.listed));
    rows.mark_listed().await;
    let next = crate::downloader::fetch_next_unprocessed(&rows.pool)
        .await
        .unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn the_video_upgrade_never_takes_a_song_that_left() {
    let rows = rows("").await;
    let next = crate::video_upgrade::worker::next_song(&rows.pool, 2160, 1_760_000_000_000)
        .await
        .unwrap();
    assert_eq!(next.as_deref(), Some("kept-v00001"));
    let counts = crate::video_upgrade::worker::counts(&rows.pool, 2160)
        .await
        .unwrap();
    assert_eq!(counts.pending, 1, "the status route's count");
    rows.mark_listed().await;
    let next = crate::video_upgrade::worker::next_song(&rows.pool, 2160, 1_760_000_000_000)
        .await
        .unwrap();
    assert!(next.is_none());
}

/// The translation and the register check of a served track (paid AI).
#[tokio::test]
async fn the_translation_and_register_passes_never_take_a_song_that_left() {
    let rows = rows("has_lyrics = 1, lyrics_source = 'gemini-3-5-transcribe'").await;
    let next = crate::db::models::fetch_next_stale_translation(&rows.pool, 1)
        .await
        .unwrap();
    assert_eq!(next.map(|r| r.id), Some(rows.listed));
    let next = crate::db::models_register::fetch_next_stale_register(&rows.pool, 1)
        .await
        .unwrap();
    assert_eq!(next.map(|(id, _)| id), Some(rows.listed));
    rows.mark_listed().await;
    let next = crate::db::models::fetch_next_stale_translation(&rows.pool, 1)
        .await
        .unwrap();
    assert!(next.is_none());
    let next = crate::db::models_register::fetch_next_stale_register(&rows.pool, 1)
        .await
        .unwrap();
    assert!(next.is_none());
}

/// The missing-translation pass reads each candidate's track: a track
/// with no Slovak line is due, unless its song left the playlist.
#[tokio::test]
async fn the_missing_translation_pass_never_takes_a_song_that_left() {
    let rows = rows("has_lyrics = 1").await;
    let cache = tempfile::tempdir().unwrap();
    for youtube_id in ["gone-v00001", "kept-v00001"] {
        std::fs::write(
            cache.path().join(format!("{youtube_id}_lyrics.json")),
            r#"{"version":1,"source":"x","lines":[{"start_ms":0,"end_ms":1000,"en":"Hi"}]}"#,
        )
        .unwrap();
    }
    let skip = std::collections::HashSet::new();
    let next =
        crate::db::models::get_next_video_missing_translation(&rows.pool, cache.path(), &skip)
            .await
            .unwrap();
    assert_eq!(next.map(|(id, _)| id), Some(rows.listed));
    rows.mark_listed().await;
    let next =
        crate::db::models::get_next_video_missing_translation(&rows.pool, cache.path(), &skip)
            .await
            .unwrap();
    assert!(next.is_none());
}
