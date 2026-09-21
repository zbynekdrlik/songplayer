//! Dubbing D1 (#180) queries — split out of `models.rs` to keep it under the
//! 1000-line airuleset cap (declared `pub mod models_dabing;` in `db/mod.rs`
//! exactly like `models_stems`). Call sites use `crate::db::models_dabing::…`.
//!
//! The dub state lives as columns on `videos` (V26), mirroring the V24 stems
//! pattern — one row per video, no joins (main design decision, Approach 1;
//! the spec-v2 `dub_tracks` table was rejected). This module owns the D1
//! surface: the request toggle + its priority side effects, the newest-first
//! list for the Dabing section, the mixer-ratio write, and the pure
//! [`dub_chain_state`] display derivation. The dub CHAIN itself (stems /
//! transcript / translation / synth) is D3/D4 — the write-side selectors
//! (`get_next_dub_job`, `mark_dub_*`) belong to those lanes and are added there.

use serde::Serialize;
use sqlx::{Row, SqlitePool};

/// The stage a dub-requested video is at, derived from its `dub_status` plus the
/// stems/lyrics it already has. Displayed on the Dabing page as the glyph row
/// `stiahnuté → stemy → prepis → preklad → dabing → pripravené`, or `chyba:
/// <krok>` for [`DubChainState::Failed`] (the step text comes from `dub_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DubChainState {
    /// Requested, waiting for the chain to start.
    Queued,
    /// Voice/ambient stem separation.
    Stems,
    /// EN transcript.
    Transcript,
    /// SK translation.
    Translation,
    /// SK dub synthesis in progress.
    Synth,
    /// `dub.flac` written — playable through the mixer.
    Ready,
    /// A step errored (retryable). The reason is `DubRow::dub_error`.
    Failed,
}

impl DubChainState {
    /// Stable wire string for the JSON payload + the E2E mock fixtures + the
    /// UI's match arms.
    pub fn as_str(self) -> &'static str {
        match self {
            DubChainState::Queued => "queued",
            DubChainState::Stems => "stems",
            DubChainState::Transcript => "transcript",
            DubChainState::Translation => "translation",
            DubChainState::Synth => "synth",
            DubChainState::Ready => "ready",
            DubChainState::Failed => "failed",
        }
    }
}

/// Pure `dub_status` + `stem_status` + `lyrics_present` → [`DubChainState`]
/// mapping, unit-tested for every combination. `dub_status` is authoritative for
/// the named stages (`stems`/`transcript`/`translation`/`synth`/`ready`/
/// `failed`); in the ambiguous pre-transcript region (`queued`/`none`/unknown)
/// the already-available artefacts refine the displayed step so the operator
/// sees real progress: existing lyrics ⇒ `Transcript`, else finished stems ⇒
/// `Stems`, else `Queued`. It never contradicts an explicit later `dub_status`.
pub fn dub_chain_state(
    dub_status: &str,
    stem_status: Option<&str>,
    lyrics_present: bool,
) -> DubChainState {
    match dub_status {
        "failed" => DubChainState::Failed,
        "ready" => DubChainState::Ready,
        "synth" => DubChainState::Synth,
        "translation" => DubChainState::Translation,
        "transcript" => DubChainState::Transcript,
        "stems" => DubChainState::Stems,
        // queued / none / anything else: refine by what already exists.
        _ => {
            if lyrics_present {
                DubChainState::Transcript
            } else if stem_status == Some("done") {
                DubChainState::Stems
            } else {
                DubChainState::Queued
            }
        }
    }
}

/// One dub-requested video for the Dabing section list. All fields live on the
/// `videos` row (no joins). `title` is the operator-facing label (song → title
/// fallback); `chain_state` is the resolved [`DubChainState`] wire string.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DubRow {
    pub video_id: i64,
    pub playlist_id: i64,
    pub title: String,
    pub dub_status: String,
    pub dub_error: Option<String>,
    pub dub_mix_ratio: f64,
    pub dub_file_path: Option<String>,
    pub stem_status: Option<String>,
    pub lyrics_present: bool,
    /// Resolved [`DubChainState::as_str`] so the UI does not duplicate the
    /// derivation logic — computed server-side from the three inputs above.
    pub chain_state: String,
    /// The pinned dub voice (#184 round C), read from the repurposed
    /// `dub_voice_ref_path` column. `None` until the worker resolves one.
    pub dub_voice: Option<String>,
}

/// Build a [`DubRow`] from a selected `videos` row (shared by every query here).
fn row_to_dub_row(r: &sqlx::sqlite::SqliteRow) -> DubRow {
    let title: Option<String> = r.get("title");
    let song: Option<String> = r.get("song");
    let dub_status: String = r.get("dub_status");
    let stem_status: Option<String> = r.get("stem_status");
    let lyrics_present = r.get::<i64, _>("has_lyrics") != 0;
    let chain_state = dub_chain_state(&dub_status, stem_status.as_deref(), lyrics_present).as_str();
    DubRow {
        video_id: r.get("id"),
        playlist_id: r.get("playlist_id"),
        title: song.filter(|s| !s.is_empty()).or(title).unwrap_or_default(),
        dub_status,
        dub_error: r.get("dub_error"),
        dub_mix_ratio: r.get("dub_mix_ratio"),
        dub_file_path: r.get("dub_file_path"),
        stem_status,
        lyrics_present,
        chain_state: chain_state.to_string(),
        dub_voice: r.get("dub_voice_ref_path"),
    }
}

const DUB_ROW_SELECT: &str = "SELECT id, playlist_id, title, song, dub_status, \
     dub_error, dub_mix_ratio, dub_file_path, stem_status, has_lyrics, \
     dub_voice_ref_path FROM videos";

/// Set (or clear) the dub request on a video. Requesting flips `dub_requested`
/// on, moves `dub_status` to `'queued'`, stamps `dub_requested_at`, and raises
/// the stems manual-priority bucket (`stem_manual_priority = 1`) so the ambient
/// stem is separated first. It does NOT raise `lyrics_manual_priority` (#182): a
/// dubbed talk's EN/SK subtitles come from the Live-session transcript, not the
/// song-lyrics pipeline, which now skips every dub-requested video. Un-requesting
/// flips `dub_requested` off and resets `dub_status` to `'none'`; it leaves the
/// stems flag alone. Returns the number of rows affected (0 = no such id).
pub async fn set_dub_requested(
    pool: &SqlitePool,
    video_id: i64,
    requested: bool,
) -> Result<u64, sqlx::Error> {
    let res = if requested {
        sqlx::query(
            "UPDATE videos \
             SET dub_requested = 1, dub_status = 'queued', \
                 dub_requested_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
                 stem_manual_priority = 1 \
             WHERE id = ?",
        )
        .bind(video_id)
        .execute(pool)
        .await?
    } else {
        sqlx::query("UPDATE videos SET dub_requested = 0, dub_status = 'none' WHERE id = ?")
            .bind(video_id)
            .execute(pool)
            .await?
    };
    Ok(res.rows_affected())
}

/// Every dub-requested video, newest request first (`dub_requested_at DESC`,
/// tie-broken by `id DESC` so a batch added in the same millisecond stays
/// deterministic), for the Dabing section list.
pub async fn list_dub_videos(pool: &SqlitePool) -> Result<Vec<DubRow>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "{DUB_ROW_SELECT} WHERE dub_requested = 1 \
         ORDER BY dub_requested_at DESC, id DESC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_dub_row).collect())
}

/// Persist the mixer blend ratio for a video, clamped to `0.0..=1.0`. Returns
/// `(clamped_value, rows_affected)` so the handler can 404 when no such id
/// (consistent with `set_dub_requested`/`patch_dub`). A `NaN` clamps to `1.0`
/// (dub-only, the safe default).
/// Clamp a dub-mix ratio to the mixer's `0.0..=1.0` range, mapping NaN to the
/// dub-only default `1.0`. Pure so the live push (`api/dabing.rs::patch_dub_mix`)
/// and the DB persist ([`set_dub_mix_ratio`]) apply the SAME clamped value
/// (#184 round A — the push must carry exactly what gets stored).
pub fn clamp_dub_ratio(ratio: f64) -> f64 {
    if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(0.0, 1.0)
    }
}

pub async fn set_dub_mix_ratio(
    pool: &SqlitePool,
    video_id: i64,
    ratio: f64,
) -> Result<(f64, u64), sqlx::Error> {
    let clamped = clamp_dub_ratio(ratio);
    let res = sqlx::query("UPDATE videos SET dub_mix_ratio = ? WHERE id = ?")
        .bind(clamped)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok((clamped, res.rows_affected()))
}

/// Persist the resolved dub voice for a video (#184 round C). The voice name is
/// stored in the EXISTING nullable `dub_voice_ref_path` TEXT column — REPURPOSED
/// as the voice name (it was dead plumbing from the abandoned clone lane, so no
/// schema change is needed). Returns the rows affected so a caller can tell a
/// missing id (0). Idempotent.
pub async fn set_dub_voice(
    pool: &SqlitePool,
    video_id: i64,
    voice: &str,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("UPDATE videos SET dub_voice_ref_path = ? WHERE id = ?")
        .bind(voice)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ── D4 write-side: the dub synthesis chain (#183) ───────────────────────────────

/// One unit of dub work: a dub-requested, downloaded video and the artefacts the
/// worker needs to decide the next step. `vocals_file_path` / `instrumental_file_path`
/// gate the stems precondition; `audio_file_path` is the Live-API input.
#[derive(Debug, Clone, PartialEq)]
pub struct DubJob {
    pub video_id: i64,
    pub youtube_id: String,
    pub audio_file_path: String,
    pub duration_ms: Option<i64>,
    pub dub_status: String,
    pub dub_mix_ratio: f64,
    pub vocals_file_path: Option<String>,
    pub instrumental_file_path: Option<String>,
    /// The stems worker's terminal marker: `'unsupported'` means the video is over
    /// the 120-min separation cap and stems will NEVER arrive — the dub proceeds
    /// without them (2-stream mix) and does NOT raise the stems priority.
    pub stem_status: Option<String>,
    pub dub_attempts: i64,
}

/// Whether both stems are present for a dub job (the ambient bed + original-voice
/// channels the 4-stream mix needs). Pure so the worker's precondition branch is
/// unit-tested without the filesystem.
pub fn dub_stems_ready(vocals: Option<&str>, instrumental: Option<&str>) -> bool {
    vocals.is_some_and(|v| !v.is_empty()) && instrumental.is_some_and(|i| !i.is_empty())
}

/// The stems availability for the dub proceed decision (#183 round 2), collapsed
/// to the three cases [`synth_ready`] branches on. Pure + observable so the
/// decision is unit-tested exhaustively without the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DubStemsState {
    /// Both stem files present → the 4-stream mix will be used.
    Ready,
    /// Stems not present yet, still separable (status pending / failed / none) —
    /// a background separation may still produce them.
    Pending,
    /// `stem_status = 'unsupported'` — the video is over the 120-min cap, so stems
    /// will NEVER arrive; the dub uses the 2-stream mix.
    Unsupported,
}

/// Collapse the raw stem artefacts + status into a [`DubStemsState`]. Present
/// files win over any status (a `Ready` mix is ready regardless of the recorded
/// status); else `'unsupported'` is terminal; else `Pending`.
pub fn dub_stems_state(
    vocals: Option<&str>,
    instrumental: Option<&str>,
    stem_status: Option<&str>,
) -> DubStemsState {
    if dub_stems_ready(vocals, instrumental) {
        DubStemsState::Ready
    } else if stem_status == Some("unsupported") {
        DubStemsState::Unsupported
    } else {
        DubStemsState::Pending
    }
}

/// What the dub worker should do with a job, given whether the video is
/// downloaded, its stems availability, and its duration (#183 round 2). The dub
/// chain NEVER waits for stems (there is no `WaitForStems` — long videos the stem
/// worker cannot separate must still be dubbed):
///
/// - not downloaded → [`SynthDecision::WaitForDownload`];
/// - stems `Ready` or `Unsupported` → [`SynthDecision::Proceed`] (synthesize now);
/// - stems `Pending` AND the duration is within the 120-min stem cap →
///   [`SynthDecision::ProceedRaisePriority`] (synthesize now AND raise
///   `stem_manual_priority` so a later separation enriches the mix);
/// - stems `Pending` but the duration is BEYOND the cap → [`SynthDecision::Proceed`]
///   (separation would only be marked unsupported, so raising its priority is
///   pointless).
///
/// Pure — unit-tested for every combination.
pub fn synth_ready(
    downloaded: bool,
    stems: DubStemsState,
    duration_ms: Option<i64>,
) -> SynthDecision {
    if !downloaded {
        return SynthDecision::WaitForDownload;
    }
    match stems {
        DubStemsState::Ready | DubStemsState::Unsupported => SynthDecision::Proceed,
        DubStemsState::Pending => {
            if crate::stems::worker::stem_duration_supported(duration_ms) {
                SynthDecision::ProceedRaisePriority
            } else {
                SynthDecision::Proceed
            }
        }
    }
}

/// The dub worker's per-tick decision (#183 round 2). See [`synth_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthDecision {
    /// The video is not downloaded yet — nothing to synthesize.
    WaitForDownload,
    /// Synthesize now (stems ready, or will never arrive).
    Proceed,
    /// Synthesize now AND raise `stem_manual_priority` so a later separation
    /// enriches the mix (stems absent but still within the separation cap).
    ProceedRaisePriority,
}

/// Select the next dub-requested video needing work, NEWEST request first (the
/// Dabing section is the owner's priority queue — a fresh add jumps ahead).
/// Eligible: `dub_requested=1`, downloaded (`normalized=1` + `audio_file_path`),
/// not terminal (`dub_status` not `none`/`ready`), and past any failure backoff.
/// Returns `None` when nothing is due.
pub async fn get_next_dub_job(pool: &SqlitePool) -> Result<Option<DubJob>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, youtube_id, audio_file_path, duration_ms, dub_status, \
                dub_mix_ratio, vocals_file_path, instrumental_file_path, \
                stem_status, dub_attempts \
         FROM videos \
         WHERE dub_requested = 1 \
           AND normalized = 1 \
           AND audio_file_path IS NOT NULL \
           AND dub_status NOT IN ('none', 'ready') \
           AND (dub_next_attempt_at IS NULL \
                OR dub_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
         ORDER BY dub_requested_at DESC, id DESC \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| DubJob {
        video_id: r.get("id"),
        youtube_id: r.get("youtube_id"),
        audio_file_path: r.get("audio_file_path"),
        duration_ms: r.get("duration_ms"),
        dub_status: r.get("dub_status"),
        dub_mix_ratio: r.get("dub_mix_ratio"),
        vocals_file_path: r.get("vocals_file_path"),
        instrumental_file_path: r.get("instrumental_file_path"),
        stem_status: r.get("stem_status"),
        dub_attempts: r.get("dub_attempts"),
    }))
}

/// Raise `stem_manual_priority` for a dub job whose stems are not yet present but
/// are still separable (#183 round 2). Unlike the retired `mark_dub_waiting_stems`
/// this does NOT park `dub_status` at `stems` — the dub proceeds to `synth`
/// immediately; a later separation only ENRICHES the mix (2-stream → 4-stream on
/// the next open). Idempotent; clears any error. No backoff.
pub async fn raise_dub_stem_priority(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET stem_manual_priority = 1, dub_error = NULL WHERE id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Advance a dub job to `synth` (stems ready, Live synthesis about to run).
/// Clears any prior error. Idempotent.
pub async fn mark_dub_synth(pool: &SqlitePool, video_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE videos SET dub_status = 'synth', dub_error = NULL WHERE id = ?")
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a finished dub: store the track path + engine tag, mark `ready`, and
/// reset the retry backoff. Playback picks the dub up on the next play.
pub async fn mark_dub_ready(
    pool: &SqlitePool,
    video_id: i64,
    dub_file_path: &str,
    dub_engine: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos \
         SET dub_file_path = ?, dub_engine = ?, dub_status = 'ready', \
             dub_attempts = 0, dub_next_attempt_at = NULL, dub_error = NULL \
         WHERE id = ?",
    )
    .bind(dub_file_path)
    .bind(dub_engine)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored `dub_mix_ratio` for a video ONLY when it has a finished dub track
/// (`dub_file_path` set) — i.e. a video that will actually play the 4-stream dub
/// mix. `None` for a non-dub video (so play-start seeding is a no-op for it) or a
/// missing id. Lets the engine restore the per-video blend to the process-global
/// dub control at play-start (the design's "global, set per play").
pub async fn dub_ratio_if_ready(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT dub_mix_ratio FROM videos \
         WHERE id = ? AND dub_file_path IS NOT NULL",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await
}

/// Record a transient dub failure: mark `failed`, store the error tail, increment
/// `dub_attempts`, and schedule `dub_next_attempt_at = now + backoff` (same
/// `strftime` format `get_next_dub_job` compares against). Returns the new attempt
/// count. Mirrors `models_stems::record_stem_deferral`.
pub async fn record_dub_deferral(
    pool: &SqlitePool,
    video_id: i64,
    error: &str,
    backoff: std::time::Duration,
) -> Result<u32, sqlx::Error> {
    let secs = backoff.as_secs() as i64;
    let current: i64 = sqlx::query_scalar("SELECT dub_attempts FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_one(pool)
        .await?;
    let new_attempts = current + 1;
    // Keep the stored error short — it is display text, not a log.
    let truncated: String = error.chars().take(500).collect();
    sqlx::query(
        "UPDATE videos \
         SET dub_status = 'failed', dub_error = ?, dub_attempts = ?, \
             dub_next_attempt_at = \
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) \
         WHERE id = ?",
    )
    .bind(truncated)
    .bind(new_attempts)
    .bind(secs)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(new_attempts as u32)
}

/// A finished dub that still lacks its subtitle track (#182 backfill).
#[derive(Debug, Clone, PartialEq)]
pub struct DubSubtitleBackfill {
    pub video_id: i64,
    pub youtube_id: String,
    pub audio_file_path: String,
}

/// Dub-ready videos whose lyrics track is NOT the Live-Translate subtitle track:
/// dubs that finished before #182 shipped, or whose subtitle build failed.
/// Oldest first.
pub async fn list_ready_dubs_without_subtitles(
    pool: &SqlitePool,
) -> Result<Vec<DubSubtitleBackfill>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, youtube_id, audio_file_path FROM videos \
         WHERE dub_requested = 1 AND dub_status = 'ready' \
           AND audio_file_path IS NOT NULL \
           AND (lyrics_source IS NULL OR lyrics_source != ?) \
         ORDER BY id ASC",
    )
    .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DubSubtitleBackfill {
            video_id: r.get("id"),
            youtube_id: r.get("youtube_id"),
            audio_file_path: r.get("audio_file_path"),
        })
        .collect())
}

#[cfg(test)]
#[path = "models_tests_dabing.rs"]
mod tests;
