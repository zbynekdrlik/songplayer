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

    // Build a set of video IDs that have a complete video+audio pair.
    let paired_ids: std::collections::HashSet<&str> =
        scan.songs.iter().map(|s| s.video_id.as_str()).collect();

    // Handle lyrics sidecars: delete orphaned ones, re-link paired ones.
    for (video_id, path) in &scan.lyrics_files {
        if !paired_ids.contains(video_id.as_str()) {
            tracing::info!(
                "removing orphaned lyrics sidecar for {}: {}",
                video_id,
                path.display()
            );
            if let Err(e) = std::fs::remove_file(path) {
                tracing::warn!(
                    "failed to remove orphaned lyrics file {}: {e}",
                    path.display()
                );
            }
        } else {
            sqlx::query("UPDATE videos SET has_lyrics = 1 WHERE youtube_id = ? AND has_lyrics = 0")
                .bind(video_id)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

/// Trigger a one-time playlist sync for every active playlist at startup.
/// Legacy Python parity with `tools.py::trigger_startup_sync`.
pub async fn startup_sync_active_playlists(
    pool: &SqlitePool,
    sync_tx: &tokio::sync::mpsc::Sender<SyncRequest>,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id, youtube_url FROM playlists WHERE is_active = 1")
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
