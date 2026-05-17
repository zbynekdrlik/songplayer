//! Integration test for the post-download rename + DB-update + legacy-cleanup
//! orchestration that lives at the tail of `DownloadWorker::process_next`.
//!
//! Real subprocess invocations (yt-dlp, ffmpeg loudnorm) require network +
//! binaries that aren't available on `ubuntu-latest` CI without extra
//! installation, so this test fakes the pre-rename inputs by writing
//! placeholder bytes and exercises the parts that matter:
//!
//! - `mark_video_processed_pair` updates the DB row with both sidecar
//!   paths, song/artist, source, gemini_failed, and `normalized = 1`
//! - `cleanup_legacy` deletes pre-migration `_normalized.mp4` files
//! - `scan_cache` reports the post-migration pair as a `CachedSong`
//!   (no orphans, no legacy)
//!
//! Closes #19.

use std::collections::HashSet;
use std::path::PathBuf;

use sp_server::db::{create_memory_pool, models, run_migrations};
use sp_server::downloader::cache::{
    LegacyFile, audio_filename, cleanup_legacy, scan_cache, video_filename,
};

/// Create a tempdir for the cache, return its path and a guard that keeps
/// the dir alive for the duration of the test.
fn cache_tempdir() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    (path, dir)
}

/// Seed an in-memory DB with a single playlist and a single un-normalized
/// video row, returning the pool plus the row id.
async fn seed_db_with_video(youtube_id: &str, title: &str) -> (sqlx::SqlitePool, i64) {
    let pool = create_memory_pool().await.expect("pool");
    run_migrations(&pool).await.expect("migrations");

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'test', 'https://youtube.com/playlist?list=PLtest', 'SP-test', 1)",
    )
    .execute(&pool)
    .await
    .expect("insert playlist");

    let row = sqlx::query_as::<_, (i64,)>(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
         VALUES (1, ?, ?, 0) RETURNING id",
    )
    .bind(youtube_id)
    .bind(title)
    .fetch_one(&pool)
    .await
    .expect("insert video");

    (pool, row.0)
}

#[tokio::test]
async fn mark_processed_pair_updates_db_row_with_both_sidecar_paths() {
    let (cache_dir, _guard) = cache_tempdir();
    let yt_id = "abcdEF12345";
    let (pool, video_id) = seed_db_with_video(yt_id, "Song Title - Artist Name").await;

    // Place pair files on disk (placeholder bytes — the function we're
    // testing does NOT read or validate the contents, only records the
    // paths in the DB).
    let video_path = cache_dir.join(video_filename("Song", "Artist", yt_id, false));
    let audio_path = cache_dir.join(audio_filename("Song", "Artist", yt_id, false));
    tokio::fs::write(&video_path, b"fake video")
        .await
        .expect("write video");
    tokio::fs::write(&audio_path, b"fake audio")
        .await
        .expect("write audio");

    // Execute the DB write that the real worker performs as the final step.
    models::mark_video_processed_pair(
        &pool,
        video_id,
        "Song",
        "Artist",
        "gemini",
        false,
        video_path.to_str().unwrap(),
        audio_path.to_str().unwrap(),
    )
    .await
    .expect("mark processed");

    // Verify all 6 fields landed where they belong.
    let row = sqlx::query_as::<_, (String, String, String, i64, String, String, i64)>(
        "SELECT song, artist, metadata_source, gemini_failed, file_path, audio_file_path, normalized \
         FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_one(&pool)
    .await
    .expect("select row");

    assert_eq!(row.0, "Song");
    assert_eq!(row.1, "Artist");
    assert_eq!(row.2, "gemini");
    assert_eq!(row.3, 0, "gemini_failed=false → 0");
    assert_eq!(row.4, video_path.to_string_lossy());
    assert_eq!(row.5, audio_path.to_string_lossy());
    assert_eq!(row.6, 1, "normalized flag must flip to 1");
}

#[tokio::test]
async fn cleanup_legacy_deletes_pre_migration_normalized_mp4_files() {
    let (cache_dir, _guard) = cache_tempdir();
    // Pre-migration filename (single-file _normalized.mp4 without _video).
    let legacy_path = cache_dir.join("Song_Artist_abcdEF12345_normalized.mp4");
    tokio::fs::write(&legacy_path, b"legacy single-file mp4")
        .await
        .expect("write legacy");
    assert!(legacy_path.exists());

    cleanup_legacy(&[LegacyFile {
        video_id: "abcdEF12345".into(),
        gemini_failed: false,
        path: legacy_path.clone(),
    }]);

    assert!(
        !legacy_path.exists(),
        "legacy file must be deleted, still present at {legacy_path:?}"
    );
}

#[tokio::test]
async fn scan_cache_recognizes_post_migration_pair_as_cached_song() {
    let (cache_dir, _guard) = cache_tempdir();
    let yt_id = "xyZ0123_-DE";

    let video_path = cache_dir.join(video_filename("Song", "Artist", yt_id, false));
    let audio_path = cache_dir.join(audio_filename("Song", "Artist", yt_id, false));
    tokio::fs::write(&video_path, b"v").await.expect("write v");
    tokio::fs::write(&audio_path, b"a").await.expect("write a");

    let scan = scan_cache(&cache_dir);

    assert_eq!(scan.songs.len(), 1, "exactly one paired song expected");
    assert_eq!(scan.songs[0].video_id, yt_id);
    assert_eq!(scan.songs[0].video_path, video_path);
    assert_eq!(scan.songs[0].audio_path, audio_path);
    assert!(
        scan.legacy.is_empty(),
        "no legacy files expected, got: {:?}",
        scan.legacy
    );
    assert!(
        scan.orphans.is_empty(),
        "no orphans expected, got: {:?}",
        scan.orphans
    );
}

#[tokio::test]
async fn scan_cache_reports_orphan_when_pair_is_incomplete() {
    let (cache_dir, _guard) = cache_tempdir();
    let yt_id = "ORPHAN12345";

    // Audio sidecar without its video pair — the integrity check the
    // production self-heal relies on.
    let audio_path = cache_dir.join(audio_filename("Song", "Artist", yt_id, false));
    tokio::fs::write(&audio_path, b"a").await.expect("write a");

    let scan = scan_cache(&cache_dir);
    assert!(scan.songs.is_empty(), "no complete pairs expected");
    assert_eq!(scan.orphans.len(), 1, "exactly one orphan expected");
    assert_eq!(scan.orphans[0].video_id, yt_id);
    assert_eq!(scan.orphans[0].path, audio_path);
}

#[tokio::test]
async fn cleanup_legacy_is_safe_on_already_deleted_files() {
    // Idempotency: if a previous cleanup removed the file, calling again
    // must not panic or error. The production self-heal path can re-run
    // after a partial failure and must tolerate this.
    let (cache_dir, _guard) = cache_tempdir();
    let path = cache_dir.join("Song_Artist_abcdEF12345_normalized.mp4");
    // Note: file is NOT created.

    cleanup_legacy(&[LegacyFile {
        video_id: "abcdEF12345".into(),
        gemini_failed: false,
        path,
    }]);
    // No assertion — completing without panicking is the test.
    // Suppress unused warning on the import.
    let _: HashSet<String> = HashSet::new();
}
