//! Reprocess worker — retries metadata extraction for videos where Gemini failed.
//!
//! Runs periodically in the background, querying for videos with `gemini_failed = 1`
//! and re-attempting metadata extraction via the configured providers.

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::{Row, SqlitePool};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::metadata::MetadataProvider;

/// Background worker that periodically retries metadata extraction for
/// videos where the initial Gemini extraction failed.
pub struct ReprocessWorker {
    pool: SqlitePool,
    providers: Arc<Vec<Box<dyn MetadataProvider>>>,
    cache_dir: PathBuf,
}

/// Row data for a video that needs reprocessing.
struct ReprocessRow {
    id: i64,
    youtube_id: String,
    title: String,
    file_path: String,
}

impl ReprocessWorker {
    pub fn new(
        pool: SqlitePool,
        providers: Arc<Vec<Box<dyn MetadataProvider>>>,
        cache_dir: PathBuf,
    ) -> Self {
        Self {
            pool,
            providers,
            cache_dir,
        }
    }

    /// Run the reprocess loop until shutdown is signalled.
    ///
    /// Waits 5 seconds on startup, then loops every 30 minutes:
    /// query videos with `gemini_failed = 1 AND normalized = 1`,
    /// retry metadata extraction, rename files and update the DB on success.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        info!("reprocess worker started");

        // Initial delay — let other workers settle.
        tokio::select! {
            _ = shutdown.recv() => {
                info!("reprocess worker shutting down before first run");
                return;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }

        loop {
            match self.process_all().await {
                Ok(count) => {
                    if count > 0 {
                        info!(count, "reprocessed videos");
                    } else {
                        debug!("no videos to reprocess");
                    }
                }
                Err(e) => {
                    warn!("reprocess run failed: {e}");
                }
            }

            // Sleep 30 minutes between runs, listening for shutdown.
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("reprocess worker shutting down");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(30 * 60)) => {}
            }
        }

        info!("reprocess worker stopped");
    }

    /// Process all videos with `gemini_failed = 1`. Returns count of successfully
    /// reprocessed videos.
    async fn process_all(&self) -> Result<usize, anyhow::Error> {
        let rows = self.fetch_gemini_failed().await?;
        if rows.is_empty() {
            return Ok(0);
        }

        info!(count = rows.len(), "found videos to reprocess");
        let mut success_count = 0;

        for row in rows {
            match self.reprocess_one(&row).await {
                Ok(true) => {
                    info!(video_id = %row.youtube_id, "reprocessed successfully");
                    success_count += 1;
                }
                Ok(false) => {
                    debug!(video_id = %row.youtube_id, "metadata still failed, will retry later");
                }
                Err(e) => {
                    warn!(video_id = %row.youtube_id, "reprocess error: {e}");
                }
            }
        }

        Ok(success_count)
    }

    /// Query videos with gemini_failed = 1 AND normalized = 1.
    async fn fetch_gemini_failed(&self) -> Result<Vec<ReprocessRow>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, youtube_id, COALESCE(title, '') AS title, COALESCE(file_path, '') AS file_path
             FROM videos
             WHERE gemini_failed = 1 AND normalized = 1
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| ReprocessRow {
                id: r.get("id"),
                youtube_id: r.get("youtube_id"),
                title: r.get("title"),
                file_path: r.get("file_path"),
            })
            .collect())
    }

    /// Retry metadata extraction for a single video.
    /// Returns `true` if the metadata was successfully updated (gemini_failed cleared).
    async fn reprocess_one(&self, row: &ReprocessRow) -> Result<bool, anyhow::Error> {
        let meta =
            crate::metadata::get_metadata(&self.providers, &row.youtube_id, &row.title).await;

        if meta.gemini_failed {
            return Ok(false);
        }

        // Rename file: replace _gf suffix if present.
        let new_file_path = if row.file_path.contains("_gf.mp4") {
            let new_name = crate::downloader::cache::normalized_filename(
                &meta.song,
                &meta.artist,
                &row.youtube_id,
                false,
            );
            let new_path = self.cache_dir.join(&new_name);

            if !row.file_path.is_empty() {
                let old_path = std::path::Path::new(&row.file_path);
                if old_path.exists() {
                    if let Err(e) = tokio::fs::rename(old_path, &new_path).await {
                        warn!(
                            video_id = %row.youtube_id,
                            old = %row.file_path,
                            new = %new_path.display(),
                            "rename failed: {e}"
                        );
                        // Continue with DB update even if rename fails —
                        // the old path still works.
                        row.file_path.clone()
                    } else {
                        new_path.to_string_lossy().into_owned()
                    }
                } else {
                    new_path.to_string_lossy().into_owned()
                }
            } else {
                row.file_path.clone()
            }
        } else {
            row.file_path.clone()
        };

        // Update DB.
        sqlx::query(
            "UPDATE videos
             SET song = ?, artist = ?, metadata_source = ?,
                 gemini_failed = 0, file_path = ?
             WHERE id = ?",
        )
        .bind(&meta.song)
        .bind(&meta.artist)
        .bind(meta.source.as_str())
        .bind(&new_file_path)
        .bind(row.id)
        .execute(&self.pool)
        .await?;

        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::metadata::{MetadataError, MetadataProvider};
    use async_trait::async_trait;
    use sp_core::metadata::{MetadataSource, VideoMetadata};

    /// Provider that always succeeds with fixed metadata.
    struct SuccessProvider;

    #[async_trait]
    impl MetadataProvider for SuccessProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Ok(VideoMetadata {
                song: "Corrected Song".into(),
                artist: "Corrected Artist".into(),
                source: MetadataSource::Gemini,
                gemini_failed: false,
            })
        }
        fn name(&self) -> &str {
            "success-mock"
        }
    }

    /// Provider that always fails.
    struct FailProvider;

    #[async_trait]
    impl MetadataProvider for FailProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Err(MetadataError::ApiError("still failing".into()))
        }
        fn name(&self) -> &str {
            "fail-mock"
        }
    }

    async fn setup() -> SqlitePool {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        pool
    }

    /// Insert a test video with gemini_failed = 1 and normalized = 1.
    async fn insert_gf_video(pool: &SqlitePool, youtube_id: &str, file_path: &str) -> i64 {
        // Create playlist first.
        sqlx::query(
            "INSERT OR IGNORE INTO playlists (id, name, youtube_url) VALUES (1, 'Test', 'url')",
        )
        .execute(pool)
        .await
        .unwrap();

        let row = sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized, file_path, metadata_source)
             VALUES (1, ?, 'Original Title', 'Old Song', 'Old Artist', 1, 1, ?, 'regex')
             RETURNING id",
        )
        .bind(youtube_id)
        .bind(file_path)
        .fetch_one(pool)
        .await
        .unwrap();

        row.get("id")
    }

    #[tokio::test]
    async fn reprocess_updates_db_on_success() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        // Create a _gf file on disk.
        let gf_name = "Old Song_Old Artist_dQw4w9WgXcQ_normalized_gf.mp4";
        let gf_path = tmp.path().join(gf_name);
        tokio::fs::write(&gf_path, "fake video data").await.unwrap();

        let video_id = insert_gf_video(&pool, "dQw4w9WgXcQ", gf_path.to_str().unwrap()).await;

        let providers: Arc<Vec<Box<dyn MetadataProvider>>> =
            Arc::new(vec![Box::new(SuccessProvider)]);
        let worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 1);

        // Verify DB was updated.
        let row =
            sqlx::query("SELECT song, artist, gemini_failed, file_path FROM videos WHERE id = ?")
                .bind(video_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        let song: String = row.get("song");
        let artist: String = row.get("artist");
        let gf: i32 = row.get("gemini_failed");
        let new_path: String = row.get("file_path");

        assert_eq!(song, "Corrected Song");
        assert_eq!(artist, "Corrected Artist");
        assert_eq!(gf, 0);
        assert!(
            !new_path.contains("_gf"),
            "file path should not contain _gf suffix: {new_path}"
        );

        // Verify file was renamed on disk.
        assert!(!gf_path.exists(), "old _gf file should be gone");
    }

    #[tokio::test]
    async fn reprocess_leaves_db_unchanged_on_failure() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        let gf_name = "Old Song_Old Artist_xxxxxxxxxxx_normalized_gf.mp4";
        let gf_path = tmp.path().join(gf_name);
        tokio::fs::write(&gf_path, "fake").await.unwrap();

        insert_gf_video(&pool, "xxxxxxxxxxx", gf_path.to_str().unwrap()).await;

        let providers: Arc<Vec<Box<dyn MetadataProvider>>> = Arc::new(vec![Box::new(FailProvider)]);
        let worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);

        // DB should be unchanged.
        let row = sqlx::query("SELECT gemini_failed FROM videos WHERE youtube_id = 'xxxxxxxxxxx'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let gf: i32 = row.get("gemini_failed");
        assert_eq!(gf, 1);

        // File should still exist.
        assert!(gf_path.exists());
    }

    #[tokio::test]
    async fn reprocess_no_gf_videos_returns_zero() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        let providers: Arc<Vec<Box<dyn MetadataProvider>>> =
            Arc::new(vec![Box::new(SuccessProvider)]);
        let worker = ReprocessWorker::new(pool, providers, tmp.path().to_path_buf());

        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn run_shuts_down_cleanly() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        let providers: Arc<Vec<Box<dyn MetadataProvider>>> = Arc::new(vec![]);
        let worker = ReprocessWorker::new(pool, providers, tmp.path().to_path_buf());

        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        let handle = tokio::spawn(async move {
            worker.run(shutdown_rx).await;
        });

        // Send shutdown before the 5-second initial delay expires.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());

        // Worker should exit.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("worker should shut down within 2s")
            .expect("worker task should not panic");
    }
}
