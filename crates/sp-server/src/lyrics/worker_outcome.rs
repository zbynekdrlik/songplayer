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

/// #144: how far ahead a stems-blocked lyrics row is re-checked. The stems
/// worker separates at ~2.2× realtime, so a 10-minute recheck is one or two
/// separations — long enough that the selector spends its ticks on songs whose
/// stems ARE ready, short enough that a freshly-separated song's lyrics follow
/// promptly. No `lyrics_attempts` penalty (see [`LyricsWorker::defer_for_stems`]).
pub(crate) const STEMS_WAIT_RECHECK: std::time::Duration = std::time::Duration::from_secs(600);

/// The result of `LyricsWorker::process_song` for a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SongOutcome {
    /// Terminal this pass — persisted or quarantined (or, for a song the
    /// wall serves, its failed attempt recorded, #144). Nothing more to do
    /// for this row now.
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
    /// re-picking this unprocessable row every 5 s tick. #263: also the error
    /// exit of a row the wall does not serve ([`Self::fail_song`]), its reason
    /// naming the error.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn defer_song(&self, video_id: i64, youtube_id: &str, reason: &str) {
        let backoff = self.next_backoff(video_id).await;
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

    /// #144: no-penalty recheck deferral for a song whose stems vocals sidecar
    /// is not ready yet. Schedules `lyrics_next_attempt_at` [`STEMS_WAIT_RECHECK`]
    /// ahead WITHOUT touching `lyrics_attempts`, so the selector moves to the
    /// next song instead of re-picking this stems-blocked row every tick;
    /// `defer_heavy` already logged the INFO + cleared the in-flight marker.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn defer_for_stems(&self, video_id: i64) {
        if let Err(e) =
            crate::db::models::record_lyrics_wait(&self.pool, video_id, STEMS_WAIT_RECHECK).await
        {
            warn!("worker: record_lyrics_wait failed for {video_id}: {e}");
        }
    }

    /// The backoff of the row's next attempt: `downloader::retry_backoff` of
    /// its current `lyrics_attempts` + 1 (`5 min · 2^(n-1)`, cap 24 h).
    async fn next_backoff(&self, video_id: i64) -> std::time::Duration {
        let prior: i64 = sqlx::query_scalar("SELECT lyrics_attempts FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        crate::downloader::retry_backoff(prior as u32 + 1)
    }

    /// #144: whether the row serves lyrics on the wall now: `has_lyrics = 1`
    /// AND its `<yt>_lyrics.json` exists (what `playback/lyrics_loader.rs`
    /// loads). F1: the file is the VIDEO's, so any row of the video with
    /// `has_lyrics = 1` serves it. A row that cannot be read counts as not
    /// served.
    async fn serves_lyrics(&self, video_id: i64, youtube_id: &str) -> bool {
        let has_lyrics: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(MAX(has_lyrics), 0) FROM videos \
             WHERE youtube_id = (SELECT youtube_id FROM videos WHERE id = ?)",
        )
        .bind(video_id)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        let file = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        has_lyrics == Some(1) && tokio::fs::try_exists(&file).await.unwrap_or(false)
    }

    /// #144 (ROZHODNUTÉ 5905945274, refined by 5908227646): a failed or empty
    /// re-run never darkens a song the wall already serves. When the row
    /// serves lyrics ([`Self::serves_lyrics`]), record ONLY the attempt:
    /// `lyrics_attempts` and the `lyrics_next_attempt_at` backoff (the manual
    /// bucket waits it out, so a queued song does not loop). It stays in the
    /// manual queue until its `SERVED_RERUN_MAX_ATTEMPTS`th failed attempt
    /// clears `lyrics_manual_priority`, WARNed with the song and the reason.
    /// `has_lyrics`, `lyrics_source` and the file stay until a successful run
    /// replaces them. Returns whether it did; `false` means the caller takes
    /// the row's terminal state.
    async fn keep_served_lyrics(&self, video_id: i64, youtube_id: &str, reason: &str) -> bool {
        if !self.serves_lyrics(video_id, youtube_id).await {
            return false;
        }
        let backoff = self.next_backoff(video_id).await;
        match crate::db::models::record_served_lyrics_failure(&self.pool, video_id, backoff).await {
            Ok(failure) => log_served_failure(video_id, youtube_id, reason, failure, backoff),
            Err(e) => warn!(
                youtube_id = %youtube_id,
                reason,
                %e,
                "worker: re-run failed — the served lyrics are kept, recording the attempt failed"
            ),
        }
        true
    }

    /// The `Err` exit of `process_next`: `process_song` failed for this row
    /// (e.g. a failed Claude cleanup in `gather.rs`, a CLIProxy or Gemini
    /// outage). A row the wall serves keeps its lyrics and records the
    /// attempt ([`Self::keep_served_lyrics`]). Any other row is DEFERRED
    /// (#263, [`Self::defer_song`]): the attempt with its backoff on every
    /// row of the video, one WARN naming the error, and `lyrics_source`,
    /// `has_lyrics`, the version and the file left as they are, so its queue
    /// picks it again once due. An error is never a terminal state: it used
    /// to park the song `no_source` at the current pipeline version, which no
    /// queue takes until a pipeline bump (SNV's Joy, 15.9.–13.10.2026).
    /// Clears the in-flight marker either way.
    pub(crate) async fn fail_song(&self, video_id: i64, youtube_id: &str, error: &anyhow::Error) {
        let reason = format!("process_error: {error}");
        if self.keep_served_lyrics(video_id, youtube_id, &reason).await {
            self.clear_processing().await;
        } else {
            self.defer_song(video_id, youtube_id, &reason).await;
        }
    }

    /// The g35t base tier's no-lyrics exit: an empty transcript, or one too
    /// sparse to be lyrics (#283, `g35t_transcript::is_sparse`); `reason`
    /// names which. A row the wall serves keeps its lyrics and records the
    /// attempt ([`Self::keep_served_lyrics`]), unless what it serves is
    /// itself too sparse to be lyrics ([`Self::serves_only_shouts`]); that
    /// row, like any other, is quarantined as `asr_gap`
    /// (`quarantine_video_lyrics`, which also removes its `<yt>_lyrics.json`).
    pub(crate) async fn quarantine_no_lyrics(
        &self,
        video_id: i64,
        youtube_id: &str,
        reason: &str,
        song_ms: Option<i64>,
    ) {
        if !self.serves_only_shouts(youtube_id, song_ms).await
            && self.keep_served_lyrics(video_id, youtube_id, reason).await
        {
            return;
        }
        warn!(
            youtube_id = %youtube_id,
            reason,
            "g35t base tier: no lyrics in the transcript — quarantining as asr_gap"
        );
        if let Err(e) = crate::db::models::quarantine_video_lyrics(
            &self.pool,
            video_id,
            &self.cache_dir,
            reason,
            crate::lyrics::LYRICS_PIPELINE_VERSION,
        )
        .await
        {
            warn!(youtube_id = %youtube_id, %e, "g35t base tier: quarantine failed");
        }
    }

    /// #283: whether the `<yt>_lyrics.json` the video has is a transcript's
    /// own track (the g35t base tier's sources) that is itself too sparse to
    /// be lyrics (242 served five shouts, `Yeah. / Come on.`, for a sung
    /// song). Such a track is never kept over a failed transcript. A track of
    /// a text (★, the operator's override) is lyrics however short; a missing
    /// or unreadable file, or a song of unknown length, is not judged.
    async fn serves_only_shouts(&self, youtube_id: &str, song_ms: Option<i64>) -> bool {
        use crate::lyrics::g35t_transcript::{SOURCE_G35T, is_sparse, track_words};
        let file = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let Ok(text) = tokio::fs::read_to_string(&file).await else {
            return false;
        };
        serde_json::from_str::<sp_core::lyrics::LyricsTrack>(&text).is_ok_and(|track| {
            track.source.starts_with(SOURCE_G35T) && is_sparse(track_words(&track), song_ms)
        })
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

/// The WARN of a served song's failed re-run (`keep_served_lyrics`), by
/// what it recorded: the song left the manual queue (its last allowed
/// attempt, named with its reason), or it stays queued for its next attempt,
/// or — a row that was not in the manual queue (the stale / full-mix
/// buckets) — only the attempt and its backoff. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_served_failure(
    video_id: i64,
    youtube_id: &str,
    reason: &str,
    failure: crate::db::models::ServedFailure,
    backoff: std::time::Duration,
) {
    let attempts = failure.attempts;
    let backoff_secs = backoff.as_secs();
    if failure.left_manual_queue {
        warn!(
            video_id,
            youtube_id = %youtube_id,
            reason,
            attempts,
            "worker: re-run failed on its last allowed attempt — the served lyrics are kept, and the song leaves the manual queue"
        );
    } else if failure.was_manual {
        warn!(
            youtube_id = %youtube_id,
            reason,
            attempts,
            backoff_secs,
            "worker: re-run failed — the served lyrics are kept, and the song stays queued for its next attempt after the backoff"
        );
    } else {
        warn!(
            youtube_id = %youtube_id,
            reason,
            attempts,
            backoff_secs,
            "worker: re-run failed — the served lyrics are kept, only the attempt is recorded"
        );
    }
}

#[cfg(test)]
#[path = "worker_outcome_tests.rs"]
mod tests;
