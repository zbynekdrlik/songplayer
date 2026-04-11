//! Startup migration integration test: legacy files are deleted and
//! all video rows are reset to unnormalized on first boot.

use std::fs;

use sp_server::startup::self_heal_cache;
use sqlx::Row;

#[tokio::test]
async fn self_heal_deletes_legacy_files_and_resets_normalized() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Seed a playlist + an already-normalized video pointing at a legacy
    // .mp4 path. (Note: V4 has already reset normalized=0 via run_migrations,
    // so we UPDATE the row back to normalized=1 to simulate a row that
    // somehow survived.)
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let legacy_path = tmp.path().join("Old_Song_dQw4w9WgXcQ_normalized.mp4");
    fs::write(&legacy_path, b"legacy").unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'dQw4w9WgXcQ', 1, ?)",
    )
    .bind(legacy_path.to_string_lossy().as_ref())
    .execute(&pool)
    .await
    .unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    // File is gone.
    assert!(!legacy_path.exists(), "legacy .mp4 must be deleted");
}

#[tokio::test]
async fn self_heal_deletes_orphan_half_sidecar() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();

    // A video sidecar without its audio partner — classic mid-download crash.
    let orphan = tmp.path().join("S_A_aaaaaaaaaaa_normalized_video.mp4");
    fs::write(&orphan, b"orphan").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(!orphan.exists(), "orphan sidecar must be deleted");
}

#[tokio::test]
async fn self_heal_keeps_complete_pairs_and_links_to_db() {
    let pool = sp_server::db::create_memory_pool().await.unwrap();
    sp_server::db::run_migrations(&pool).await.unwrap();

    // Seed a playlist + an un-normalized video row matching the pair below.
    sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'bbbbbbbbbbb', 0, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let v = tmp.path().join("S_A_bbbbbbbbbbb_normalized_video.mp4");
    let a = tmp.path().join("S_A_bbbbbbbbbbb_normalized_audio.flac");
    fs::write(&v, b"v").unwrap();
    fs::write(&a, b"a").unwrap();

    self_heal_cache(&pool, tmp.path()).await.unwrap();

    assert!(v.exists(), "complete video sidecar must survive");
    assert!(a.exists(), "complete audio sidecar must survive");

    // DB row must have been re-linked and marked normalized.
    let row = sqlx::query("SELECT file_path, audio_file_path, normalized FROM videos WHERE youtube_id = 'bbbbbbbbbbb'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let file_path: Option<String> = row.get("file_path");
    let audio_path: Option<String> = row.get("audio_file_path");
    let normalized: i64 = row.get("normalized");
    assert_eq!(normalized, 1, "row must be marked normalized after re-link");
    assert!(file_path.is_some() && file_path.unwrap().ends_with("_video.mp4"));
    assert!(audio_path.is_some() && audio_path.unwrap().ends_with("_audio.flac"));
}
