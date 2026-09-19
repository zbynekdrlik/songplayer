//! Karaoke stem-separation queries (#14) — split out of `models.rs` to keep
//! it under the 1000-line airuleset cap. Call sites use the module path
//! directly (`crate::db::models_stems::…`).
//!
//! Mirrors the V22 lyrics retry-backoff bookkeeping: the stem worker selects the
//! next normalized song that has no stems yet (or whose last attempt failed and
//! whose backoff has elapsed), then records the two sidecar paths + a terminal
//! status. `stem_status` values: NULL = pending, 'done', 'failed' (retryable,
//! backoff-gated), 'unsupported' (terminal — e.g. audio too long / no vocals).

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

/// Per-song karaoke-stems state for the dashboard (#177). Derived from the DB
/// row via [`stems_state_of`]; exposed on the karaoke now-playing payload and
/// as the additive `stems_state` field on the videos list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemsState {
    /// Both stems on disk → karaoke presets work for this song.
    Ready,
    /// Pending / waiting in the worker queue (no stems yet).
    Queued,
    /// The stem worker is separating THIS song right now (live in-flight signal).
    Processing,
    /// Terminal: not separable (too long / no vocals — `stem_status='unsupported'`).
    Unavailable,
    /// The last separation attempt failed (retryable) — ✖ chyba.
    Failed,
}

impl StemsState {
    /// Stable wire string for the JSON payload (matches the UI's `stems_state`
    /// match arms and the E2E mock fixtures).
    pub fn as_str(self) -> &'static str {
        match self {
            StemsState::Ready => "ready",
            StemsState::Queued => "queued",
            StemsState::Processing => "processing",
            StemsState::Unavailable => "unavailable",
            StemsState::Failed => "failed",
        }
    }
}

/// Pure DB-row → [`StemsState`] mapping (#177), unit-tested for every branch.
///
/// `is_processing` is the LIVE stem-worker in-flight signal
/// ([`crate::stems::progress::in_flight`]) — the only honest source of the
/// "⚙ spracúvam" state, because there is deliberately NO DB `'processing'`
/// status (a crash mid-separation would strand a persisted one). It is passed
/// in place of the design's `stem_next_attempt_at`, which does not change the
/// displayed glyph: a `'failed'` row is ✖ chyba whether or not its backoff has
/// elapsed, and queue eligibility (which the backoff gates) is answered
/// separately by [`queue_position`].
///
/// Precedence: Processing (live) → Ready (both stems on disk, presets work
/// regardless of the recorded status) → Unavailable (`'unsupported'`, terminal)
/// → Failed (`'failed'`, retryable) → Queued (pending / NULL / anything else).
pub fn stems_state_of(
    stem_status: Option<&str>,
    vocals_exists: bool,
    instrumental_exists: bool,
    is_processing: bool,
) -> StemsState {
    if is_processing {
        return StemsState::Processing;
    }
    if vocals_exists && instrumental_exists {
        return StemsState::Ready;
    }
    match stem_status {
        Some("unsupported") => StemsState::Unavailable,
        Some("failed") => StemsState::Failed,
        _ => StemsState::Queued,
    }
}

/// 1-based position of `video_id` in the stem worker's queue, or `None` when the
/// row is not queue-eligible (done / unsupported / not normalized / no audio /
/// within its failure backoff). Mirrors [`get_next_video_for_stems`]'s
/// eligibility predicate and its `ORDER BY stem_manual_priority DESC, id ASC`, so
/// the number the panel shows matches the order the worker will pick: a
/// higher-priority row, or a same-priority lower-id row, comes before this one.
pub async fn queue_position(pool: &SqlitePool, video_id: i64) -> Result<Option<i64>, sqlx::Error> {
    const ELIGIBLE_PRED: &str = "normalized = 1 AND audio_file_path IS NOT NULL \
         AND (stem_status IS NULL OR stem_status = 'failed') \
         AND (stem_next_attempt_at IS NULL \
              OR stem_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";

    let this: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT stem_manual_priority FROM videos WHERE id = ? AND {ELIGIBLE_PRED}"
    ))
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some(prio) = this else {
        return Ok(None);
    };
    let before: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM videos \
         WHERE {ELIGIBLE_PRED} \
           AND (stem_manual_priority > ? \
                OR (stem_manual_priority = ? AND id < ?))"
    ))
    .bind(prio)
    .bind(prio)
    .bind(video_id)
    .fetch_one(pool)
    .await?;
    Ok(Some(before + 1))
}

/// One video's stems-related fields, plus its resolved [`StemsState`]. Feeds the
/// karaoke now-playing payload (`api/karaoke.rs`) and is joined with the videos
/// list. `title` is the operator-facing song label (song → title fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoStemsInfo {
    pub title: String,
    pub state: StemsState,
    pub attempts: i64,
}

/// Fetch one video's title + stems fields and resolve its [`StemsState`].
/// Returns `None` when no such video row exists. `is_processing` is the live
/// in-flight signal for THIS video (the caller passes
/// `crate::stems::progress::in_flight() == Some(video_id)`).
pub async fn video_stems_info(
    pool: &SqlitePool,
    video_id: i64,
    is_processing: bool,
) -> Result<Option<VideoStemsInfo>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT title, song, stem_status, stem_attempts, \
                vocals_file_path, instrumental_file_path \
         FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        let title: Option<String> = r.get("title");
        let song: Option<String> = r.get("song");
        let stem_status: Option<String> = r.get("stem_status");
        let vocals: Option<String> = r.get("vocals_file_path");
        let instrumental: Option<String> = r.get("instrumental_file_path");
        VideoStemsInfo {
            title: song.filter(|s| !s.is_empty()).or(title).unwrap_or_default(),
            state: stems_state_of(
                stem_status.as_deref(),
                vocals.is_some(),
                instrumental.is_some(),
                is_processing,
            ),
            attempts: r.get("stem_attempts"),
        }
    }))
}

/// Map each STEM-RELEVANT video in a playlist to its [`StemsState`] wire string,
/// for the video-list marker. A row is stem-relevant when it could carry stems —
/// normalized with an audio sidecar, OR already having a stem file — so a
/// not-yet-processed download shows no misleading "queued" marker. Rows that are
/// not relevant are omitted (the videos payload leaves their `stems_state`
/// `None`). `processing_video_id` is the worker's live in-flight id (so a
/// currently-separating row shows ⚙ in the list too).
pub async fn stems_state_map(
    pool: &SqlitePool,
    playlist_id: i64,
    processing_video_id: Option<i64>,
) -> Result<HashMap<i64, String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, normalized, audio_file_path, stem_status, \
                vocals_file_path, instrumental_file_path \
         FROM videos WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id: i64 = r.get("id");
            let normalized: bool = r.get::<i64, _>("normalized") != 0;
            let has_audio: bool = r.get::<Option<String>, _>("audio_file_path").is_some();
            let vocals: Option<String> = r.get("vocals_file_path");
            let instrumental: Option<String> = r.get("instrumental_file_path");
            let has_stem = vocals.is_some() || instrumental.is_some();
            // Only stem-relevant rows get a marker.
            if !(normalized && has_audio) && !has_stem {
                return None;
            }
            let stem_status: Option<String> = r.get("stem_status");
            let state = stems_state_of(
                stem_status.as_deref(),
                vocals.is_some(),
                instrumental.is_some(),
                processing_video_id == Some(id),
            );
            Some((id, state.as_str().to_string()))
        })
        .collect())
}

/// Operator "Zaradiť do fronty" (#177): reset a video's stem bookkeeping so the
/// oldest-first worker selector picks it on its next tick — clears any failure
/// backoff and re-opens a terminal `'unsupported'` row. It does NOT jump the
/// queue: the selector is oldest-first by id and `stem_manual_priority` is #182
/// (out of scope here). Affects one row; harmless on a row without audio (the
/// selector still won't pick it).
pub async fn enqueue_stems(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET stem_status = NULL, stem_attempts = 0, stem_next_attempt_at = NULL \
         WHERE id = ?",
    )
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// One unit of stem-separation work: a normalized song whose `{id}_audio.flac`
/// exists but whose stems are not yet generated.
#[derive(Debug, Clone, PartialEq)]
pub struct StemJob {
    pub video_id: i64,
    pub youtube_id: String,
    pub audio_file_path: String,
    pub duration_ms: Option<i64>,
    pub song: Option<String>,
    pub artist: Option<String>,
}

/// Select the next song needing stem separation. Manual-priority rows first
/// (`stem_manual_priority DESC` — a dub-requested video's stems jump the queue,
/// #183 D4 / #182), then oldest-first (`id ASC`) within the same priority.
/// Eligible rows are normalized, have an `audio_file_path`, are not already
/// `done`/`unsupported`, and (if previously `failed`) have passed their backoff
/// window. Returns `None` when nothing is due.
pub async fn get_next_video_for_stems(pool: &SqlitePool) -> Result<Option<StemJob>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, youtube_id, audio_file_path, duration_ms, song, artist \
         FROM videos \
         WHERE normalized = 1 \
           AND audio_file_path IS NOT NULL \
           AND (stem_status IS NULL OR stem_status = 'failed') \
           AND (stem_next_attempt_at IS NULL \
                OR stem_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
         ORDER BY stem_manual_priority DESC, id ASC \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| StemJob {
        video_id: r.get("id"),
        youtube_id: r.get("youtube_id"),
        audio_file_path: r.get("audio_file_path"),
        duration_ms: r.get("duration_ms"),
        song: r.get("song"),
        artist: r.get("artist"),
    }))
}

/// Record a successful separation: store both sidecar paths, mark `done`, and
/// reset the retry backoff.
pub async fn mark_stems_done(
    pool: &SqlitePool,
    video_id: i64,
    vocals_path: &str,
    instrumental_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET vocals_file_path = ?, instrumental_file_path = ?, \
             stem_status = 'done', stem_attempts = 0, stem_next_attempt_at = NULL \
         WHERE id = ?",
    )
    .bind(vocals_path)
    .bind(instrumental_path)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a transient separation failure: mark `failed`, increment
/// `stem_attempts`, and schedule `stem_next_attempt_at = now + backoff` (same
/// `strftime` format the selector compares against). Returns the new attempt
/// count. The song is retried once the backoff elapses.
pub async fn record_stem_deferral(
    pool: &SqlitePool,
    video_id: i64,
    backoff: std::time::Duration,
) -> Result<u32, sqlx::Error> {
    let secs = backoff.as_secs() as i64;
    let current: i64 = sqlx::query_scalar("SELECT stem_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(pool)
        .await?;
    let new_attempts = current + 1;
    sqlx::query(
        "UPDATE videos \
         SET stem_status = 'failed', stem_attempts = ?, \
             stem_next_attempt_at = \
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) \
         WHERE id = ?",
    )
    .bind(new_attempts)
    .bind(secs)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(new_attempts as u32)
}

/// Mark a song as terminally unsupported for stem separation (no retry).
pub async fn mark_stems_unsupported(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET stem_status = 'unsupported', stem_next_attempt_at = NULL \
         WHERE id = ?",
    )
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `(pending, done)` stem counts for the dashboard badge. `pending` counts every
/// normalized song with an audio sidecar that is not yet `done`/`unsupported`
/// (NULL or `failed`); `done` counts songs with both stems written.
pub async fn count_stems_progress(pool: &SqlitePool) -> Result<(i64, i64), sqlx::Error> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos \
         WHERE normalized = 1 AND audio_file_path IS NOT NULL \
           AND (stem_status IS NULL OR stem_status = 'failed')",
    )
    .fetch_one(pool)
    .await?;
    let done: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM videos WHERE stem_status = 'done'")
        .fetch_one(pool)
        .await?;
    Ok((pending, done))
}

#[cfg(test)]
#[path = "models_tests_stems.rs"]
mod tests;
