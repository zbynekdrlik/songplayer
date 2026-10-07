//! Reprocess worker — retries metadata extraction for videos whose metadata
//! came from the title parser because every provider failed.
//!
//! Runs periodically in the background, querying the repair queue
//! (`metadata::health::REPAIR_QUEUE_WHERE`) and re-attempting metadata
//! extraction on the ONE production provider chain (#136: the same `Arc`
//! the download worker uses — before #136 this worker had its own list with
//! Gemini alone, so a broken Gemini left every row broken forever).
//!
//! A failed row is logged at WARN with EVERY provider's reason, in chain
//! order, once per attempt. The per-video backoff below allows one attempt
//! per stage; the stages shorter than the 30-min cycle (1, 5, 15 min) are
//! retried every cycle, the longer ones (1 h, 6 h, 24 h) wait for theirs.
//!
//! ## Rate-limit cooldown (issue #12)
//!
//! When a provider returns `MetadataError::RateLimited`, the worker enters
//! a global cooldown of [`RATE_LIMIT_COOLDOWN`] during which no provider is
//! called. Each rate-limited video also enters a per-video exponential
//! backoff — its entry is skipped until its `next_retry_at` instant passes.
//! The stages are 1 min, 5 min, 15 min, 1 h, 6 h, 24 h and stay at 24 h once
//! reached. Successful extraction clears the video's backoff entry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::metadata::VideoMetadata;
use sqlx::{Row, SqlitePool};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::metadata::health::REPAIR_QUEUE_WHERE;
use crate::metadata::{ChainFailure, ProviderChain};

/// Global cooldown after any provider's rate-limit response.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(300); // 5 min (a literal: no mutable op)

/// Per-video exponential backoff stages. The worker indexes into this slice
/// by the video's current stage; stages beyond the last pin to the final
/// entry so they never escalate further.
const BACKOFF_STAGES: [Duration; 6] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

/// Background worker that periodically retries metadata extraction for
/// videos where the initial Gemini extraction failed.
pub struct ReprocessWorker {
    pool: SqlitePool,
    /// The ONE production chain (#136), shared with the download worker.
    chain: Arc<ProviderChain>,
    cache_dir: PathBuf,
    /// Global rate-limit cooldown — while set, no provider is called.
    cooldown_until: Option<Instant>,
    /// Per-video backoff: `video_id → (next_retry_at, stage_index)`.
    per_video_backoff: HashMap<i64, (Instant, usize)>,
    /// #229: this node in the exchange, asked before the providers (`None`
    /// in tests that do not need it).
    peer: Option<Arc<crate::peer::Exchange>>,
}

/// Row data for a video that needs reprocessing.
struct ReprocessRow {
    id: i64,
    youtube_id: String,
    title: String,
}

/// Outcome of attempting to reprocess a single video.
enum ReprocessOutcome {
    /// Metadata was successfully updated (DB cleared `gemini_failed`).
    Success,
    /// A provider said "rate limited" — the worker should stop the current
    /// batch and honour [`RATE_LIMIT_COOLDOWN`]. Carries every provider's error.
    RateLimited(String),
    /// Non-rate-limit failure (transient, API error, or still
    /// `gemini_failed=true` from parser fallback). The batch may continue.
    /// Carries every provider's error, in chain order.
    Failed(String),
    /// The video was skipped because it's in per-video backoff or the
    /// global cooldown window is still active.
    Skipped,
    /// The row left the repair queue while the batch ran (#136: the operator
    /// corrected its title): nothing was renamed or written.
    LeftQueue,
}

impl ReprocessWorker {
    pub fn new(pool: SqlitePool, chain: Arc<ProviderChain>, cache_dir: PathBuf) -> Self {
        Self {
            pool,
            chain,
            cache_dir,
            cooldown_until: None,
            per_video_backoff: HashMap::new(),
            peer: None,
        }
    }

    /// #229: ask the exchange's peers for a title before the providers.
    pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self {
        self.peer = Some(peer);
        self
    }

    /// Returns `true` if the worker is currently inside the rate-limit cooldown
    /// window.
    fn in_global_cooldown(&self) -> bool {
        self.cooldown_until
            .map(|t| Instant::now() < t)
            .unwrap_or(false)
    }

    /// Returns `true` if the given video is still in its per-video backoff
    /// window.
    fn in_video_backoff(&self, video_id: i64) -> bool {
        self.per_video_backoff
            .get(&video_id)
            .map(|(retry_at, _)| Instant::now() < *retry_at)
            .unwrap_or(false)
    }

    /// Run the reprocess loop until shutdown is signalled.
    ///
    /// Waits 5 seconds on startup, then loops every 30 minutes:
    /// query the repair queue (`REPAIR_QUEUE_WHERE`),
    /// retry metadata extraction, rename files and update the DB on success.
    pub async fn run(mut self, mut shutdown: broadcast::Receiver<()>) {
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

    /// Process every row of the repair queue (`REPAIR_QUEUE_WHERE`). Returns
    /// the count of successfully reprocessed videos.
    ///
    /// Aborts the current batch on the first rate-limit response, setting
    /// the global cooldown so subsequent calls within the cooldown window
    /// are no-ops.
    pub async fn process_all(&mut self) -> Result<usize, anyhow::Error> {
        let rows = self.fetch_gemini_failed().await?;
        if rows.is_empty() {
            return Ok(0);
        }

        if self.in_global_cooldown() {
            debug!(
                count = rows.len(),
                "reprocess skipped: rate-limit cooldown active"
            );
            return Ok(0);
        }

        info!(count = rows.len(), "found videos to reprocess");
        let mut success_count = 0;

        for row in rows {
            match self.reprocess_one(&row).await {
                Ok(ReprocessOutcome::Success) => {
                    info!(video_id = %row.youtube_id, "reprocessed successfully");
                    success_count += 1;
                }
                Ok(ReprocessOutcome::RateLimited(reasons)) => {
                    warn!(
                        video_id = %row.youtube_id,
                        %reasons,
                        "metadata provider rate-limited; entering {}s cooldown, aborting batch",
                        RATE_LIMIT_COOLDOWN.as_secs()
                    );
                    break;
                }
                Ok(ReprocessOutcome::Failed(reasons)) => {
                    // One WARN per attempt; the backoff stage decides the next
                    // attempt (a stage shorter than the cycle = the next cycle).
                    let (backoff_s, stage) = self.backoff_of(row.id);
                    warn!(
                        video_id = %row.youtube_id,
                        %reasons,
                        backoff_stage = stage,
                        backoff_s,
                        "metadata still failed: no provider named the video"
                    );
                }
                Ok(ReprocessOutcome::Skipped) => {
                    debug!(video_id = %row.youtube_id, "in per-video backoff, skipped");
                }
                Ok(ReprocessOutcome::LeftQueue) => {
                    info!(
                        video_id = %row.youtube_id,
                        "left the repair queue during the batch (a title correction), not repaired"
                    );
                }
                Err(e) => {
                    warn!(video_id = %row.youtube_id, "reprocess error: {e}");
                }
            }
        }

        Ok(success_count)
    }

    /// Query the repair queue (`REPAIR_QUEUE_WHERE`, the predicate
    /// `status.metadata.failed_videos` counts too).
    async fn fetch_gemini_failed(&self) -> Result<Vec<ReprocessRow>, sqlx::Error> {
        let sql = format!(
            "SELECT id, youtube_id, COALESCE(title, '') AS title
             FROM videos
             WHERE {REPAIR_QUEUE_WHERE}
             ORDER BY id"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| ReprocessRow {
                id: r.get("id"),
                youtube_id: r.get("youtube_id"),
                title: r.get("title"),
            })
            .collect())
    }

    /// Retry metadata extraction for a single video.
    ///
    /// Honours per-video backoff and surfaces rate-limit errors so the
    /// caller can abort the batch and enter the global cooldown.
    async fn reprocess_one(
        &mut self,
        row: &ReprocessRow,
    ) -> Result<ReprocessOutcome, anyhow::Error> {
        // #229: a peer's provider or operator title repairs the row with no
        // provider call (`peer::repair`), so neither this video's backoff nor
        // the rate-limit cooldown (both about the providers) holds it back.
        let peer = self.peer.clone();
        if let Some(ex) = peer.as_deref()
            && let Some(title) = crate::peer::repair::peer_title(ex, &row.youtube_id).await
        {
            self.per_video_backoff.remove(&row.id);
            return self
                .apply_title(row, &title.song, &title.artist, title.source)
                .await;
        }
        if self.in_global_cooldown() {
            return Ok(ReprocessOutcome::Skipped);
        }
        if self.in_video_backoff(row.id) {
            return Ok(ReprocessOutcome::Skipped);
        }

        let meta = match self.try_providers(&row.youtube_id, &row.title).await {
            Ok(m) => m,
            Err(failure) if failure.rate_limited => {
                self.cooldown_until = Some(Instant::now() + RATE_LIMIT_COOLDOWN);
                self.bump_video_backoff(row.id);
                return Ok(ReprocessOutcome::RateLimited(failure.reasons));
            }
            Err(failure) => {
                self.bump_video_backoff(row.id);
                return Ok(ReprocessOutcome::Failed(failure.reasons));
            }
        };

        if meta.gemini_failed {
            self.bump_video_backoff(row.id);
            return Ok(ReprocessOutcome::Failed(
                "the provider's answer was flagged gemini_failed".into(),
            ));
        }
        // Success clears this video's per-video backoff so future failures
        // start from stage 0 again.
        self.per_video_backoff.remove(&row.id);

        self.apply_title(row, &meta.song, &meta.artist, meta.source.as_str())
            .await
    }

    /// The ONE repair write: the row re-checked against the queue under
    /// `cache::SONG_FILES`, its file set renamed after `song` / `artist` (no
    /// `_gf`), recorded on every row that recorded it, the title written with
    /// `source`. Reached from the providers' answer and from a peer's title
    /// (#229, `peer::repair`).
    async fn apply_title(
        &self,
        row: &ReprocessRow,
        song: &str,
        artist: &str,
        source: &str,
    ) -> Result<ReprocessOutcome, anyhow::Error> {
        // #136: move the song's COMPLETE file set (the video, the audio, and the
        // stems + dub named after the audio) to the upgraded name as ONE unit
        // (`_gf` stripped). A song is never split across two names: when a move
        // fails, the set stays under the old name and the rows record that. The
        // set is read NOW, not from the batch snapshot: an earlier row of this
        // batch may have moved the same files (the video in another playlist).
        // The song-files lock keeps a post-job re-link out until it is recorded,
        // and a title PATCH (`patch_video` takes it too) out until the metadata
        // is written. The row is re-checked against the queue under it: a
        // correction made while the providers answered took it out (#136).
        let _files = crate::downloader::cache::SONG_FILES.lock().await;
        let queued: Option<(String, Option<String>)> = sqlx::query_as(&format!(
            "SELECT COALESCE(file_path, ''), audio_file_path FROM videos \
             WHERE id = ? AND {REPAIR_QUEUE_WHERE}"
        ))
        .bind(row.id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((file_path, audio_file_path)) = queued else {
            return Ok(ReprocessOutcome::LeftQueue);
        };
        let old =
            crate::downloader::cache::SongFiles::recorded(&file_path, audio_file_path.as_deref());
        let new = old.named(&self.cache_dir, song, artist, &row.youtube_id, false);
        let files =
            crate::downloader::cache::rename_song_files(&row.youtube_id, &old, &new).columns();

        // Every row that recorded these files (this video in another playlist
        // too) records them where they now are.
        files
            .record(
                &self.pool,
                &row.youtube_id,
                &file_path,
                audio_file_path.as_deref(),
            )
            .await?;
        sqlx::query(
            "UPDATE videos
             SET song = ?, artist = ?, metadata_source = ?, gemini_failed = 0
             WHERE id = ?",
        )
        .bind(song)
        .bind(artist)
        .bind(source)
        .bind(row.id)
        .execute(&self.pool)
        .await?;

        Ok(ReprocessOutcome::Success)
    }

    /// The chain's one walk (`metadata::first_answer`, the same the download
    /// path uses): the first sanitized answer with a song, or EVERY
    /// provider's reason in chain order (#136: before, only the last one
    /// survived, and it was logged nowhere), flagged `rate_limited` when any
    /// provider was, so the batch-abort path always wins over generic
    /// failures.
    async fn try_providers(
        &self,
        video_id: &str,
        title: &str,
    ) -> Result<VideoMetadata, ChainFailure> {
        crate::metadata::first_answer(self.chain.providers(), video_id, title).await
    }

    /// `(the backoff stage's wait in seconds, the stage)` of a video — for the
    /// failed-row WARN. A wait shorter than the 30-min cycle means "the next
    /// cycle".
    fn backoff_of(&self, video_id: i64) -> (u64, usize) {
        let stage = self
            .per_video_backoff
            .get(&video_id)
            .map(|(_, s)| *s)
            .unwrap_or(0);
        (BACKOFF_STAGES[stage].as_secs(), stage)
    }

    /// Advance the per-video backoff stage and schedule the next retry.
    fn bump_video_backoff(&mut self, video_id: i64) {
        let current_stage = self
            .per_video_backoff
            .get(&video_id)
            .map(|(_, s)| *s + 1)
            .unwrap_or(0);
        let capped = current_stage.min(BACKOFF_STAGES.len() - 1);
        let wait = BACKOFF_STAGES[capped];
        self.per_video_backoff
            .insert(video_id, (Instant::now() + wait, capped));
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

        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(SuccessProvider)]));
        let mut worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

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

        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(FailProvider)]));
        let mut worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

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

        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(SuccessProvider)]));
        let mut worker = ReprocessWorker::new(pool, providers, tmp.path().to_path_buf());

        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn run_shuts_down_cleanly() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        let providers: Arc<ProviderChain> = Arc::new(ProviderChain::new(vec![]));
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

    /// Provider that always returns `Err(RateLimited)`. Used to exercise
    /// the cooldown path.
    struct RateLimitProvider;

    #[async_trait]
    impl MetadataProvider for RateLimitProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Err(MetadataError::RateLimited("mock quota exhausted".into()))
        }
        fn name(&self) -> &str {
            "rate-limit-mock"
        }
    }

    /// Issue #12: on rate-limit, the worker must abort the current batch
    /// and skip all subsequent calls until the cooldown window expires.
    ///
    /// Uses direct manipulation of `cooldown_until` instead of
    /// `tokio::time::advance` — the sqlite pool setup relies on real I/O
    /// and doesn't cope with a paused timer.
    #[tokio::test]
    async fn gemini_rate_limit_triggers_global_cooldown() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        // Two videos both needing reprocessing.
        let gf1 = "Song_Artist_aaa1234567_normalized_gf.mp4";
        tokio::fs::write(tmp.path().join(gf1), b"x").await.unwrap();
        insert_gf_video(&pool, "aaa1234567", tmp.path().join(gf1).to_str().unwrap()).await;
        let gf2 = "Song_Artist_bbb7654321_normalized_gf.mp4";
        tokio::fs::write(tmp.path().join(gf2), b"x").await.unwrap();
        insert_gf_video(&pool, "bbb7654321", tmp.path().join(gf2).to_str().unwrap()).await;

        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(RateLimitProvider)]));
        let mut worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

        // First run: hits rate limit on the first video, aborts batch.
        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);
        assert!(
            worker.in_global_cooldown(),
            "cooldown should be active after a rate-limit"
        );

        // Second run within cooldown: must be a no-op, still zero success.
        // And no provider should have been called (would be asserted by
        // the fact that cooldown_until is still in the future).
        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);
        assert!(worker.in_global_cooldown(), "still in cooldown");

        // Simulate cooldown expiry by setting the instant to the past.
        worker.cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(
            !worker.in_global_cooldown(),
            "cooldown should report expired once `until` is in the past"
        );

        // Third run: cooldown expired, but videos just got a fresh
        // per-video backoff (1 min stage 0) from the first attempt.
        // They are still skipped.
        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 0);
    }

    /// Per-video exponential backoff escalates through the stages and caps
    /// at the final entry.
    #[tokio::test]
    async fn bump_video_backoff_escalates_and_caps() {
        let pool = setup().await;
        let providers: Arc<ProviderChain> = Arc::new(ProviderChain::new(vec![]));
        let mut worker = ReprocessWorker::new(pool, providers, PathBuf::from("."));

        // Stage 0: 1 min
        worker.bump_video_backoff(7);
        let (_, stage) = worker.per_video_backoff.get(&7).copied().unwrap();
        assert_eq!(stage, 0);

        // Stage 1: 5 min
        worker.bump_video_backoff(7);
        let (_, stage) = worker.per_video_backoff.get(&7).copied().unwrap();
        assert_eq!(stage, 1);

        // Escalate through all remaining stages and past the cap.
        for _ in 0..10 {
            worker.bump_video_backoff(7);
        }
        let (_, stage) = worker.per_video_backoff.get(&7).copied().unwrap();
        assert_eq!(
            stage,
            BACKOFF_STAGES.len() - 1,
            "stage must cap at the last entry"
        );
    }

    /// Provider whose extracted metadata still carries a raw emoji — used to
    /// prove `try_providers` sanitizes its return value structurally,
    /// independent of whether the provider itself sanitizes.
    struct EmojiProvider;

    #[async_trait]
    impl MetadataProvider for EmojiProvider {
        async fn extract(
            &self,
            _video_id: &str,
            _title: &str,
        ) -> Result<VideoMetadata, MetadataError> {
            Ok(VideoMetadata {
                song: "Song".into(),
                artist: "Foo \u{1F525}".into(),
                source: MetadataSource::Gemini,
                gemini_failed: false,
            })
        }
        fn name(&self) -> &str {
            "emoji-mock"
        }
    }

    /// #135 / #136: `try_providers` walks the chain through
    /// `metadata::first_answer`, the shared emoji-sanitizer choke point — a
    /// provider whose output still carries a raw emoji must come back clean,
    /// whichever providers are wired.
    #[tokio::test]
    async fn try_providers_sanitizes_emoji_in_returned_metadata() {
        let pool = setup().await;
        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(EmojiProvider)]));
        let worker = ReprocessWorker::new(pool, providers, PathBuf::from("."));

        let meta = worker.try_providers("vid123", "title").await.unwrap();

        assert_eq!(
            meta.artist, "Foo",
            "emoji must be stripped from provider output"
        );
        assert_eq!(meta.song, "Song");
    }

    /// Successful extraction clears the video's per-video backoff so a
    /// later failure starts from stage 0 again.
    #[tokio::test]
    async fn success_clears_per_video_backoff() {
        let pool = setup().await;
        let tmp = tempfile::tempdir().unwrap();

        let gf = "Song_Artist_ccc9999999_normalized_gf.mp4";
        let gf_path = tmp.path().join(gf);
        tokio::fs::write(&gf_path, b"x").await.unwrap();
        let video_id = insert_gf_video(&pool, "ccc9999999", gf_path.to_str().unwrap()).await;

        let providers: Arc<ProviderChain> =
            Arc::new(ProviderChain::new(vec![Box::new(SuccessProvider)]));
        let mut worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

        // Pre-populate a backoff entry to prove it gets cleared.
        worker
            .per_video_backoff
            .insert(video_id, (Instant::now(), 3));

        let count = worker.process_all().await.unwrap();
        assert_eq!(count, 1);
        assert!(
            !worker.per_video_backoff.contains_key(&video_id),
            "successful reprocess must clear the backoff entry"
        );
    }
}

#[cfg(test)]
#[path = "tests_chain.rs"]
mod tests_chain;

#[cfg(test)]
#[path = "tests_files.rs"]
mod tests_files;

#[cfg(test)]
#[path = "tests_peer.rs"]
mod tests_peer;
