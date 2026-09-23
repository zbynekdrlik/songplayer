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

/// Whether a row's duration exceeds the lyrics processing cap
/// ([`crate::lyrics::MAX_LYRICS_DURATION_MS`], #144). `None` (unknown
/// duration) is treated as under the cap — an unknown-length row is still
/// worth attempting; only a row we KNOW is over 30 min is rejected outright.
pub(crate) fn exceeds_duration_cap(duration_ms: Option<i64>) -> bool {
    matches!(duration_ms, Some(d) if d > crate::lyrics::MAX_LYRICS_DURATION_MS)
}

/// The result of `LyricsWorker::process_song` for a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SongOutcome {
    /// Terminal this pass — persisted, quarantined, or marked no_source.
    /// Nothing more to do for this row now.
    Done,
    /// Could not be processed now; retry after a durable backoff. The `&str`
    /// is a short machine reason surfaced in the WARN log.
    Deferred(&'static str),
    /// #154: the wall went busy after isolation but before the mtl spawn.
    /// Deferred with NO backoff penalty — the row stays at the head of the
    /// queue and is re-picked the instant the wall goes idle (the isolated
    /// vocal WAV is preserved on disk for a cache-hit re-run). Distinct from
    /// `Deferred` precisely so `process_next` skips the exponential backoff.
    WaitingForWall,
    /// #162: free RAM / commit fell below `HEAVY_STEP_MIN_FREE_BYTES` before a
    /// heavy step, so it was NOT spawned (owner ruling: never overload the PC).
    /// Deferred with NO backoff penalty — like `WaitingForWall`, the row stays
    /// at the head and re-runs the instant memory frees; distinct from
    /// `Deferred` so `process_next` skips the exponential backoff.
    WaitingForMemory,
    /// #144: the ★ isolation step's vocals come from the stems worker's vocals
    /// sidecar, which is not ready for this song yet. Deferred with NO backoff
    /// penalty — like `WaitingForWall`/`WaitingForMemory`, the row re-runs once
    /// the stems worker produces the sidecar; distinct from `Deferred` so
    /// `process_next` skips the exponential backoff.
    WaitingForStems,
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

    /// Stamp an over-cap row (#144) `unsupported_source`, log it, clear the
    /// in-flight processing marker, and report `Done`. Called from
    /// `process_song` when [`exceeds_duration_cap`] is true — a > 30-min video
    /// is a live set / mix, not a song, and each retry would burn ~1 h of GPU
    /// on the shared live PC, so it terminates without gather/network/GPU work.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn mark_over_cap(
        &self,
        row: &crate::db::models::VideoLyricsRow,
    ) -> anyhow::Result<SongOutcome> {
        let duration_s = row.duration_ms.unwrap_or(0) / 1000;
        crate::db::models::mark_unsupported_source(
            &self.pool,
            row.id,
            crate::lyrics::LYRICS_PIPELINE_VERSION,
        )
        .await?;
        tracing::info!(
            youtube_id = %row.youtube_id,
            duration_s,
            "lyrics: longer than 30 min — not a song, marking unsupported_source (#144)"
        );
        self.clear_processing().await;
        Ok(SongOutcome::Done)
    }
}
