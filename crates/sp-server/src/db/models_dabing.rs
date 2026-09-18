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
    }
}

const DUB_ROW_SELECT: &str = "SELECT id, playlist_id, title, song, dub_status, \
     dub_error, dub_mix_ratio, dub_file_path, stem_status, has_lyrics FROM videos";

/// Set (or clear) the dub request on a video. Requesting flips `dub_requested`
/// on, moves `dub_status` to `'queued'`, stamps `dub_requested_at`, and raises
/// BOTH the stems and lyrics manual-priority buckets (`stem_manual_priority` /
/// `lyrics_manual_priority` = 1) so the dub chain's inputs jump their queues
/// (spec §4). Un-requesting flips `dub_requested` off and resets `dub_status`
/// to `'none'`; it leaves the manual-priority flags alone (the video may still
/// want stems/lyrics). Returns the number of rows affected (0 = no such id).
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
                 stem_manual_priority = 1, lyrics_manual_priority = 1 \
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
/// the clamped value actually stored. A `NaN` clamps to `1.0` (dub-only, the
/// safe default). 0 rows affected (no such id) still returns the clamped value.
pub async fn set_dub_mix_ratio(
    pool: &SqlitePool,
    video_id: i64,
    ratio: f64,
) -> Result<f64, sqlx::Error> {
    let clamped = if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(0.0, 1.0)
    };
    sqlx::query("UPDATE videos SET dub_mix_ratio = ? WHERE id = ?")
        .bind(clamped)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(clamped)
}

#[cfg(test)]
#[path = "models_tests_dabing.rs"]
mod tests;
