//! Outcome of processing one song in the lyrics worker (#144).
//!
//! Split out of `worker.rs` to keep that file under the 1000-line airuleset
//! cap. `SongOutcome` distinguishes a row that reached a terminal state this
//! pass (`Done`) from one that could not be processed now and must be retried
//! later (`Deferred`). Before this, `run_asr_path_branch` returned a bare
//! `Ok(())` on "leaving row unprocessed" exits, `process_next` treated `Ok`
//! as done, and `get_next_video_for_lyrics` re-selected the same row every
//! 5 s tick (37-min hot-loop on 3_ccqgwVZYM). `defer_song` stamps a `Deferred`
//! row with an exponential backoff (mirroring the downloader, #140) so the
//! selector skips it until due.

use tracing::warn;

use super::worker::LyricsWorker;

/// The result of `LyricsWorker::process_song` for a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SongOutcome {
    /// Terminal this pass — persisted, quarantined, or marked no_source.
    /// Nothing more to do for this row now.
    Done,
    /// Could not be processed now; retry after a durable backoff. The `&str`
    /// is a short machine reason surfaced in the WARN log.
    Deferred(&'static str),
}

impl LyricsWorker {
    /// Durably defer a lyrics row that could not be processed this pass (#144),
    /// mirroring the downloader's per-row backoff (#140). Reads the current
    /// `lyrics_attempts` so the backoff grows `5 min · 2^(n-1)` (cap 24 h, via
    /// `downloader::retry_backoff` — the math is not duplicated), records the
    /// deferral, WARNs with the reason + backoff, and clears the in-flight
    /// processing marker — so the selector skips this row until due instead of
    /// re-picking this unprocessable row every 5 s tick.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn defer_song(&self, video_id: i64, youtube_id: &str, reason: &'static str) {
        let prior: i64 = sqlx::query_scalar("SELECT lyrics_attempts FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        let backoff = crate::downloader::retry_backoff(prior as u32 + 1);
        let attempts = crate::db::models::record_lyrics_deferral(&self.pool, video_id, backoff)
            .await
            .unwrap_or(0);
        warn!(
            youtube_id = %youtube_id,
            reason,
            attempts,
            backoff_secs = backoff.as_secs(),
            "worker: deferring row — retry after backoff"
        );
        self.clear_processing().await;
    }
}
