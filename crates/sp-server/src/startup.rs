//! First-boot self-healing routines: cache reconciliation + legacy
//! playlist sync parity with the original Python implementation.
//!
//! On first boot of a new version, [`self_heal_cache`] walks the cache
//! directory, deletes any legacy single-file `.mp4` left over from the
//! pre-FLAC pipeline, deletes any orphan half-sidecars from a crashed
//! mid-download, and re-links any complete video+audio pairs back to the
//! DB row that owns them.
//!
//! [`startup_sync_active_playlists`] replicates the behavior of the
//! legacy Python `tools.py::trigger_startup_sync` that was missed in the
//! initial Rust rewrite: after tools setup completes, fire a one-shot
//! [`SyncRequest`] for every `is_active = 1` playlist so the download
//! worker has fresh video IDs to process.

use std::path::Path;

use sqlx::{Row, SqlitePool};

use crate::SyncRequest;
use crate::downloader::cache;

/// Walk the cache directory, categorise every file, and:
///
/// * delete legacy single-file `.mp4`s (from before the FLAC migration),
/// * delete orphan half-sidecars (debris from a crashed download),
/// * re-link complete video+audio pairs to their DB row.
#[cfg_attr(test, mutants::skip)]
pub async fn self_heal_cache(pool: &SqlitePool, cache_dir: &Path) -> Result<(), sqlx::Error> {
    let scan = cache::scan_cache(cache_dir);
    tracing::info!(
        songs = scan.songs.len(),
        legacy = scan.legacy.len(),
        orphans = scan.orphans.len(),
        lyrics = scan.lyrics_files.len(),
        "self-heal cache scan"
    );

    // Delete legacy AAC single-file .mp4s — unusable under the new pipeline.
    cache::cleanup_legacy(&scan.legacy);

    // Delete orphan half-sidecars (mid-download crash debris).
    for orphan in &scan.orphans {
        tracing::info!(
            "removing orphan sidecar for {}: {}",
            orphan.video_id,
            orphan.path.display()
        );
        if let Err(e) = std::fs::remove_file(&orphan.path) {
            tracing::warn!("failed to remove orphan {}: {e}", orphan.path.display());
        }
    }

    // Re-link complete pairs back to their DB row.
    for song in &scan.songs {
        let v = song.video_path.to_string_lossy().to_string();
        let a = song.audio_path.to_string_lossy().to_string();
        sqlx::query(
            "UPDATE videos SET file_path = ?, audio_file_path = ?, normalized = 1
             WHERE youtube_id = ?",
        )
        .bind(&v)
        .bind(&a)
        .bind(&song.video_id)
        .execute(pool)
        .await?;
    }

    // Detect DB/disk mismatch: rows marked has_lyrics=1 but JSON file is gone.
    // This was originally a wholesale delete-all-lyrics-and-reset loop from
    // PR #24's migration, but that caused Gemini quota burn on every restart
    // (N songs × 1 Gemini translation call per restart). Now we only reset
    // rows where the file is genuinely missing — a truly idempotent self-heal.
    let claimed_rows = sqlx::query("SELECT youtube_id FROM videos WHERE has_lyrics = 1")
        .fetch_all(pool)
        .await?;
    let mut orphan_resets = 0usize;
    for row in claimed_rows {
        let youtube_id: String = row.get("youtube_id");
        let json_path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
        if !json_path.exists() {
            sqlx::query(
                "UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE youtube_id = ?",
            )
            .bind(&youtube_id)
            .execute(pool)
            .await?;
            orphan_resets += 1;
        }
    }
    if orphan_resets > 0 {
        tracing::info!("reset {orphan_resets} DB rows claiming has_lyrics=1 but missing JSON file");
    }

    Ok(())
}

/// Trigger a one-time playlist sync for every active playlist at startup.
/// Legacy Python parity with `tools.py::trigger_startup_sync`.
pub async fn startup_sync_active_playlists(
    pool: &SqlitePool,
    sync_tx: &tokio::sync::mpsc::Sender<SyncRequest>,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, youtube_url FROM playlists WHERE is_active = 1 AND kind = 'youtube'",
    )
    .fetch_all(pool)
    .await?;
    tracing::info!(
        count = rows.len(),
        "startup sync: enqueueing one SyncRequest per active playlist"
    );
    for row in rows {
        let playlist_id: i64 = row.get("id");
        let youtube_url: String = row.get("youtube_url");
        if let Err(e) = sync_tx
            .send(SyncRequest {
                playlist_id,
                youtube_url,
            })
            .await
        {
            tracing::warn!(playlist_id, "startup sync enqueue failed: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod sync_filter_tests {
    use super::*;
    use crate::db;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn startup_sync_skips_custom_playlists() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        // Insert one youtube playlist alongside the pre-seeded ytlive custom one.
        db::models::insert_playlist(&pool, "ytfast", "https://yt.com/fast")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel::<SyncRequest>(8);
        startup_sync_active_playlists(&pool, &tx).await.unwrap();
        drop(tx);

        let mut received_urls = Vec::new();
        while let Some(req) = rx.recv().await {
            received_urls.push(req.youtube_url);
        }

        assert_eq!(received_urls.len(), 1, "only youtube playlists should be synced");
        assert_eq!(received_urls[0], "https://yt.com/fast");
    }
}
