//! HTTP handlers for `/api/v1/lyrics/*`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::warn;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct QueueResponse {
    pub bucket0_count: i64,
    pub bucket1_count: i64,
    pub bucket2_count: i64,
    pub pipeline_version: u32,
}

pub async fn get_queue(State(state): State<AppState>) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    match fetch_queue_counts(&state.pool, LYRICS_PIPELINE_VERSION).await {
        Ok((b0, b1, b2)) => Json(QueueResponse {
            bucket0_count: b0,
            bucket1_count: b1,
            bucket2_count: b2,
            pipeline_version: LYRICS_PIPELINE_VERSION,
        })
        .into_response(),
        Err(e) => {
            warn!("get_queue error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// pub(crate) so the worker's queue_update_loop can reuse it (Task 10).
pub(crate) async fn fetch_queue_counts(
    pool: &sqlx::SqlitePool,
    current_version: u32,
) -> Result<(i64, i64, i64), sqlx::Error> {
    let b0: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.lyrics_manual_priority = 1 \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
                    OR v.lyrics_pipeline_version < ?) \
               AND p.is_active = 1 AND v.normalized = 1",
    )
    .bind(current_version as i64)
    .fetch_one(pool)
    .await?;
    let b1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0) \
               AND (v.lyrics_source IS NULL \
                    OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap') \
                    OR v.lyrics_pipeline_version < ?) \
               AND v.lyrics_manual_priority = 0 \
               AND p.is_active = 1 AND v.normalized = 1",
    )
    .bind(current_version as i64)
    .fetch_one(pool)
    .await?;
    let b2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.has_lyrics = 1 AND v.lyrics_pipeline_version < ? \
         AND v.lyrics_manual_priority = 0 AND p.is_active = 1 AND v.normalized = 1",
    )
    .bind(current_version as i64)
    .fetch_one(pool)
    .await?;
    Ok((b0, b1, b2))
}

#[derive(Debug, Deserialize)]
pub struct ListSongsQuery {
    pub playlist_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SongListItem {
    pub video_id: i64,
    pub youtube_id: String,
    pub title: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub source: Option<String>,
    pub pipeline_version: i64,
    pub quality_score: Option<f64>,
    pub has_lyrics: bool,
    pub is_stale: bool,
    pub manual_priority: bool,
    /// `videos.suppress_resolume_en` — when true, the playback engine skips
    /// pushing the English lyric line to Resolume's `#sp-subs` / `#sp-subs-next`
    /// clips. The /live setlist UI renders a checkbox bound to this field.
    pub suppress_resolume_en: bool,
}

// HTTP handler: behavior covered by integration tests in Task 14 Playwright + is_stale/manual_priority cast logic verified via API shape tests.
#[cfg_attr(test, mutants::skip)]
pub async fn list_songs(
    State(state): State<AppState>,
    Query(q): Query<ListSongsQuery>,
) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    let mut sql = String::from(
        "SELECT id, youtube_id, title, song, artist, lyrics_source, \
         lyrics_pipeline_version, lyrics_quality_score, has_lyrics, lyrics_manual_priority, \
         suppress_resolume_en \
         FROM videos WHERE normalized = 1",
    );
    if q.playlist_id.is_some() {
        sql.push_str(" AND playlist_id = ?");
    }
    sql.push_str(" ORDER BY song, artist, youtube_id");

    let mut query = sqlx::query(&sql);
    if let Some(pid) = q.playlist_id {
        query = query.bind(pid);
    }
    let rows = match query.fetch_all(&state.pool).await {
        Ok(r) => r,
        Err(e) => {
            warn!("list_songs error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let items: Vec<SongListItem> = rows
        .iter()
        .map(|r| {
            let pv: i64 = r.get("lyrics_pipeline_version");
            let hl: i64 = r.get("has_lyrics");
            let mp: i64 = r.get("lyrics_manual_priority");
            let sre: i64 = r.get("suppress_resolume_en");
            SongListItem {
                video_id: r.get("id"),
                youtube_id: r.get("youtube_id"),
                title: r.try_get("title").ok(),
                song: r.try_get("song").ok(),
                artist: r.try_get("artist").ok(),
                source: r.try_get("lyrics_source").ok(),
                pipeline_version: pv,
                quality_score: r.try_get("lyrics_quality_score").ok(),
                has_lyrics: hl == 1,
                is_stale: hl == 1 && pv < LYRICS_PIPELINE_VERSION as i64,
                manual_priority: mp == 1,
                suppress_resolume_en: sre != 0,
            }
        })
        .collect();
    Json(items).into_response()
}

#[derive(Debug, Serialize)]
pub struct SongDetail {
    pub list_item: SongListItem,
    pub lyrics_json: Option<serde_json::Value>,
    pub audit_json: Option<serde_json::Value>,
}

// HTTP handler: behavior covered by integration tests in Task 14 Playwright + is_stale/manual_priority cast logic verified via API shape tests.
#[cfg_attr(test, mutants::skip)]
pub async fn get_song_detail(
    State(state): State<AppState>,
    Path(video_id): Path<i64>,
) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    let row = match sqlx::query(
        "SELECT id, youtube_id, title, song, artist, lyrics_source, \
         lyrics_pipeline_version, lyrics_quality_score, has_lyrics, lyrics_manual_priority, \
         suppress_resolume_en \
         FROM videos WHERE id = ? AND normalized = 1",
    )
    .bind(video_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "video not found").into_response(),
        Err(e) => {
            warn!("get_song_detail db error for {video_id}: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let pv: i64 = row.get("lyrics_pipeline_version");
    let hl: i64 = row.get("has_lyrics");
    let mp: i64 = row.get("lyrics_manual_priority");
    let sre: i64 = row.get("suppress_resolume_en");
    let youtube_id: String = row.get("youtube_id");
    let list_item = SongListItem {
        video_id: row.get("id"),
        youtube_id: youtube_id.clone(),
        title: row.try_get("title").ok(),
        song: row.try_get("song").ok(),
        artist: row.try_get("artist").ok(),
        source: row.try_get("lyrics_source").ok(),
        pipeline_version: pv,
        quality_score: row.try_get("lyrics_quality_score").ok(),
        has_lyrics: hl == 1,
        is_stale: hl == 1 && pv < LYRICS_PIPELINE_VERSION as i64,
        manual_priority: mp == 1,
        suppress_resolume_en: sre != 0,
    };
    let lyrics_path = state.cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let audit_path = state
        .cache_dir
        .join(format!("{youtube_id}_alignment_audit.json"));
    let lyrics_json = tokio::fs::read_to_string(&lyrics_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let audit_json = tokio::fs::read_to_string(&audit_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    Json(SongDetail {
        list_item,
        lyrics_json,
        audit_json,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ReprocessRequest {
    #[serde(default)]
    pub video_ids: Option<Vec<i64>>,
    #[serde(default)]
    pub playlist_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ReprocessResponse {
    pub queued: i64,
    /// Count of rows in the requested set whose `lyrics_source = 'asr_gap'`.
    /// Those rows have their `manual_priority` flag set but the worker pop
    /// SQL excludes the asr_gap sentinel, so they will not be reprocessed
    /// without a pipeline-version bump (#91). Defaults to 0 for the
    /// reprocess-all-stale endpoint where the scope is "stale rows only"
    /// — those by definition cannot be asr_gap (asr_gap has has_lyrics=0).
    #[serde(default)]
    pub blocked_by_asr_gap: i64,
}

// HTTP handler: validates video_ids/playlist_id shape + dispatches to SQL UPDATE. Covered by reprocess_video_ids_sets_manual_priority + Playwright.
#[cfg_attr(test, mutants::skip)]
pub async fn post_reprocess(
    State(state): State<AppState>,
    Json(req): Json<ReprocessRequest>,
) -> impl IntoResponse {
    match (req.video_ids, req.playlist_id) {
        (Some(ids), _) if !ids.is_empty() => {
            let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
            let blocked_sql = format!(
                "SELECT COUNT(*) FROM videos WHERE lyrics_source = 'asr_gap' AND id IN ({})",
                placeholders.join(",")
            );
            let mut blocked_q = sqlx::query_scalar::<_, i64>(&blocked_sql);
            for id in &ids {
                blocked_q = blocked_q.bind(*id);
            }
            let blocked_by_asr_gap = match blocked_q.fetch_one(&state.pool).await {
                Ok(n) => n,
                Err(e) => {
                    warn!("post_reprocess asr_gap count error: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            };
            let sql = format!(
                "UPDATE videos SET lyrics_manual_priority = 1, \
                        lyrics_source = CASE \
                            WHEN lyrics_source IN ('failed', 'empty', 'no_source', 'unsupported_source') THEN NULL \
                            ELSE lyrics_source \
                        END \
                 WHERE id IN ({})",
                placeholders.join(",")
            );
            let mut q = sqlx::query(&sql);
            for id in &ids {
                q = q.bind(*id);
            }
            match q.execute(&state.pool).await {
                Ok(r) => Json(ReprocessResponse {
                    queued: r.rows_affected() as i64,
                    blocked_by_asr_gap,
                })
                .into_response(),
                Err(e) => {
                    warn!("post_reprocess error: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                }
            }
        }
        (_, Some(pid)) => {
            let blocked_by_asr_gap = match sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM videos WHERE lyrics_source = 'asr_gap' AND playlist_id = ?",
            )
            .bind(pid)
            .fetch_one(&state.pool)
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    warn!("post_reprocess asr_gap count error: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            };
            match sqlx::query(
                "UPDATE videos SET lyrics_manual_priority = 1, \
                        lyrics_source = CASE \
                            WHEN lyrics_source IN ('failed', 'empty', 'no_source', 'unsupported_source') THEN NULL \
                            ELSE lyrics_source \
                        END \
                 WHERE playlist_id = ?",
            )
            .bind(pid)
            .execute(&state.pool)
            .await
            {
                Ok(r) => Json(ReprocessResponse {
                    queued: r.rows_affected() as i64,
                    blocked_by_asr_gap,
                })
                .into_response(),
                Err(e) => {
                    warn!("post_reprocess error: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                }
            }
        }
        _ => (StatusCode::BAD_REQUEST, "need video_ids or playlist_id").into_response(),
    }
}

pub async fn post_reprocess_all_stale(State(state): State<AppState>) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;

    // Count asr_gap-parked rows at a stale pipeline_version (#101). NOTE:
    // this SELECT is intentionally COMPLEMENTARY to the UPDATE below — the
    // UPDATE targets `has_lyrics = 1` (rows that will be re-queued); the
    // COUNT targets `has_lyrics = 0 AND lyrics_source = 'asr_gap'` (rows
    // the all-stale sweep would MISS because they were quarantined under
    // the asr_gap escape hatch). The dashboard banner uses the count to
    // tell the operator how much work is blocked behind asr_gap and out of
    // reach of the "Reprocess all stale" button.
    //
    // Asymmetry vs `post_reprocess` (targeted): the targeted path counts
    // asr_gap rows WITHOUT a pipeline_version filter because the operator
    // explicitly picked those rows and the banner should fire regardless
    // of version. The all-stale sweep is implicitly scoped to stale-only,
    // so its asr_gap count matches that scope.
    let blocked_by_asr_gap = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM videos \
         WHERE has_lyrics = 0 AND lyrics_source = 'asr_gap' \
         AND lyrics_pipeline_version < ?",
    )
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .fetch_one(&state.pool)
    .await
    {
        Ok(n) => n,
        Err(e) => {
            warn!("post_reprocess_all_stale asr_gap count error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let res = sqlx::query(
        "UPDATE videos SET lyrics_manual_priority = 1 \
         WHERE has_lyrics = 1 AND lyrics_pipeline_version < ? \
         AND lyrics_manual_priority = 0",
    )
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&state.pool)
    .await;
    match res {
        Ok(r) => Json(ReprocessResponse {
            queued: r.rows_affected() as i64,
            blocked_by_asr_gap,
        })
        .into_response(),
        Err(e) => {
            warn!("post_reprocess_all_stale error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

pub async fn post_clear_manual(State(state): State<AppState>) -> impl IntoResponse {
    let res = sqlx::query("UPDATE videos SET lyrics_manual_priority = 0")
        .execute(&state.pool)
        .await;
    match res {
        Ok(r) => Json(ReprocessResponse {
            queued: r.rows_affected() as i64,
            blocked_by_asr_gap: 0,
        })
        .into_response(),
        Err(e) => {
            warn!("post_clear_manual error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Request body for `POST /api/v1/lyrics/quarantine`.
///
/// `reason` is optional free-text; it is logged via `tracing::warn` for an
/// audit trail but never persisted to the DB. Keeping it out of the schema
/// avoids a migration. See
/// `docs/superpowers/specs/2026-05-12-asr-gap-quarantine-design.md`.
#[derive(Debug, Deserialize)]
pub struct QuarantineRequest {
    pub video_id: i64,
    #[serde(default)]
    pub reason: String,
}

/// Response body for `POST /api/v1/lyrics/quarantine`. `previous_source` is
/// JSON `null` when the row had `lyrics_source IS NULL` before the call.
#[derive(Debug, Serialize)]
pub struct QuarantineResponse {
    pub video_id: i64,
    pub youtube_id: String,
    pub previous_source: Option<String>,
    pub deleted_cache_file: bool,
}

/// POST /api/v1/lyrics/quarantine
///
/// Park a song with unrecoverable ASR transcription. Sets the row's
/// `lyrics_source` to `'asr_gap'`, clears `has_lyrics` and
/// `lyrics_manual_priority`, stamps the current `LYRICS_PIPELINE_VERSION`,
/// and best-effort deletes the cached `_lyrics.json` file. See
/// `db::models::quarantine_video_lyrics` for the DB-level contract.
#[cfg_attr(test, mutants::skip)] // Thin glue: parse request → call helper →
// map RowNotFound to 404 → wrap outcome in 200 JSON. Both branches plus
// the JSON shape are covered by `quarantine_endpoint_marks_row_and_deletes_cache`
// and `quarantine_endpoint_returns_404_for_missing_video` in routes_tests.rs.
pub async fn quarantine_lyrics(
    State(state): State<crate::AppState>,
    Json(req): Json<QuarantineRequest>,
) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    match crate::db::models::quarantine_video_lyrics(
        &state.pool,
        req.video_id,
        &state.cache_dir,
        &req.reason,
        LYRICS_PIPELINE_VERSION,
    )
    .await
    {
        Ok(outcome) => Json(QuarantineResponse {
            video_id: req.video_id,
            youtube_id: outcome.youtube_id,
            previous_source: outcome.previous_source,
            deleted_cache_file: outcome.deleted_cache_file,
        })
        .into_response(),
        Err(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("quarantine_lyrics error for video {}: {e}", req.video_id);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub video_id: i64,
}

// HTTP handler: dispatches to lyrics::probe::probe_sources_impl. Behavior
// (per-provider availability) is covered by probe_tests.rs unit tests;
// this handler is thin Axum glue + the 404 / 200 + JSON-shape integration
// tests in this file's test module.
#[cfg_attr(test, mutants::skip)]
pub async fn post_probe_sources(
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> impl IntoResponse {
    // Load the video row.
    let row_opt: Option<crate::db::models::VideoLyricsRow> =
        sqlx::query_as::<_, crate::db::models::VideoLyricsRow>(
            "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
                COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, \
                p.youtube_url, v.lyrics_override_text, v.lyrics_time_offset_ms, \
                v.spotify_track_id, v.spotify_resolved_at \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE v.id = ?",
        )
        .bind(req.video_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    let Some(row) = row_opt else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Pull ytdlp path + genius token (best-effort; probe degrades gracefully).
    let ytdlp_path = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|tp| tp.ytdlp.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent/ytdlp"));
    let genius_token = crate::db::models::get_setting(&state.pool, "genius_access_token")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let client = reqwest::Client::new();

    let report = crate::lyrics::probe::probe_sources_impl(
        Some(&state.ai_client),
        &ytdlp_path,
        &state.cache_dir,
        &client,
        &row,
        &genius_token,
    )
    .await;

    Json(report).into_response()
}

#[path = "lyrics_tests.rs"]
#[cfg(test)]
mod tests;
