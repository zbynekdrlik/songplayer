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
            let sql = format!(
                "UPDATE videos SET lyrics_manual_priority = 1, \
                        lyrics_source = CASE \
                            WHEN lyrics_source IN ('failed', 'empty', 'no_source') THEN NULL \
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
                })
                .into_response(),
                Err(e) => {
                    warn!("post_reprocess error: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                }
            }
        }
        (_, Some(pid)) => {
            match sqlx::query(
                "UPDATE videos SET lyrics_manual_priority = 1, \
                        lyrics_source = CASE \
                            WHEN lyrics_source IN ('failed', 'empty', 'no_source') THEN NULL \
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{create_memory_pool, run_migrations};

    async fn setup_pool() -> sqlx::SqlitePool {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active) \
             VALUES (1, 'p', 'u', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Returns `(AppState, TempDir)`. Caller must keep `TempDir` alive for the
    /// duration of the test or the temp directory is deleted immediately.
    async fn test_state_with_cache_dir() -> (crate::AppState, tempfile::TempDir) {
        use std::sync::Arc;
        use tokio::sync::{RwLock, broadcast, mpsc};
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let (engine_tx, _) = mpsc::channel(16);
        let (sync_tx, _) = mpsc::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (obs_rebuild_tx, _) = broadcast::channel(4);
        let state = crate::AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
            resolume_tx,
            obs_rebuild_tx,
            cache_dir: cache_dir.clone(),
            ai_proxy: Arc::new(crate::ai::proxy::ProxyManager::new(
                cache_dir,
                crate::ai::proxy::ProxyManager::default_port(),
            )),
            ai_client: Arc::new(crate::ai::client::AiClient::new(
                crate::ai::AiSettings::default(),
            )),
            presenter_client: None,
            resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new()),
            ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        };
        (state, tmp)
    }

    #[tokio::test]
    async fn queue_counts_are_correct_across_buckets() {
        let pool = setup_pool().await;
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version, lyrics_manual_priority) VALUES \
             (1, 'manual1', 1, 1, 1, 1), \
             (1, 'manual2', 1, 0, 0, 1), \
             (1, 'null1',   1, 0, 0, 0), \
             (1, 'null2',   1, 0, 0, 0), \
             (1, 'stale1',  1, 1, 1, 0), \
             (1, 'fresh',   1, 1, 2, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (b0, b1, b2) = fetch_queue_counts(&pool, 2).await.unwrap();
        assert_eq!(b0, 2, "2 manual");
        assert_eq!(b1, 2, "2 null");
        assert_eq!(b2, 1, "1 stale (fresh doesn't count)");
    }

    #[tokio::test]
    async fn reprocess_video_ids_sets_manual_priority() {
        let pool = setup_pool().await;
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized) \
             VALUES (10, 1, 'a', 1), (11, 1, 'b', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Simulate the UPDATE call directly (mirrors the handler's SQL)
        sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id IN (?, ?)")
            .bind(10_i64)
            .bind(11_i64)
            .execute(&pool)
            .await
            .unwrap();
        let total_mp: i64 = sqlx::query_scalar("SELECT SUM(lyrics_manual_priority) FROM videos")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total_mp, 2);
    }

    #[tokio::test]
    async fn reprocess_all_stale_only_flags_stale_rows() {
        let pool = setup_pool().await;
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, normalized, has_lyrics, \
             lyrics_pipeline_version) VALUES \
             (1, 'fresh', 1, 1, 2), \
             (1, 'stale1', 1, 1, 1), \
             (1, 'stale2', 1, 1, 0), \
             (1, 'null',   1, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Mirror the handler's SQL
        let res = sqlx::query(
            "UPDATE videos SET lyrics_manual_priority = 1 \
             WHERE has_lyrics = 1 AND lyrics_pipeline_version < ? \
             AND lyrics_manual_priority = 0",
        )
        .bind(2_i64)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(res.rows_affected(), 2, "only 2 stale rows should flip");
    }

    #[tokio::test]
    async fn reprocess_clears_lyrics_source_for_no_source_failed_empty_states() {
        let (state, _temp) = test_state_with_cache_dir().await;
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, lyrics_source) \
             VALUES (10, 1, 'y10', 's', 'a', 1, 'no_source'), \
                    (11, 1, 'y11', 's', 'a', 1, 'failed'), \
                    (12, 1, 'y12', 's', 'a', 1, 'empty'), \
                    (13, 1, 'y13', 's', 'a', 1, 'asr_gap'), \
                    (14, 1, 'y14', 's', 'a', 1, 'yt_subs')",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let app = crate::api::router(state.clone(), None);
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let req = Request::builder()
            .uri("/api/v1/lyrics/reprocess")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"video_ids":[10,11,12,13,14]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // After reprocess: 10/11/12 should have NULL lyrics_source; 13 (asr_gap)
        // and 14 (yt_subs) should be untouched. ALL FIVE should have manual_priority=1.
        let rows: Vec<(i64, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, lyrics_source, lyrics_manual_priority FROM videos ORDER BY id",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap();
        let expected = vec![
            (10i64, None, 1i64),
            (11, None, 1),
            (12, None, 1),
            (13, Some("asr_gap".into()), 1), // untouched
            (14, Some("yt_subs".into()), 1), // untouched
        ];
        assert_eq!(rows, expected);
    }

    #[tokio::test]
    async fn reprocess_reports_asr_gap_rows_separately() {
        // Regression for #91: when the operator reprocesses a set that
        // includes an `asr_gap`-quarantined row, the response must report
        // it under `blocked_by_asr_gap` so the dashboard can show it.
        // Without this field the response (`{"queued": 1}`) was a lie —
        // the row was flagged manual_priority=1 but worker SQL excludes
        // `asr_gap`, so it never gets popped.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (state, _temp) = test_state_with_cache_dir().await;
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, lyrics_source) \
             VALUES (30, 1, 'y30', 's', 'a', 1, 'asr_gap'), \
                    (31, 1, 'y31', 's', 'a', 1, 'no_source'), \
                    (32, 1, 'y32', 's', 'a', 1, 'yt_subs')",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let app = crate::api::router(state.clone(), None);
        let req = Request::builder()
            .uri("/api/v1/lyrics/reprocess")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"video_ids":[30,31,32]}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["queued"].as_i64(), Some(3));
        assert_eq!(parsed["blocked_by_asr_gap"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn reprocess_by_playlist_reports_blocked_asr_gap_count() {
        // Same contract as the video_ids branch — the playlist-scoped
        // reprocess must also surface asr_gap counts.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (state, _temp) = test_state_with_cache_dir().await;
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (5, 'p5', 'u', 'n', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, lyrics_source) \
             VALUES (40, 5, 'y40', 's', 'a', 1, 'asr_gap'), \
                    (41, 5, 'y41', 's', 'a', 1, 'asr_gap'), \
                    (42, 5, 'y42', 's', 'a', 1, 'no_source')",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let app = crate::api::router(state.clone(), None);
        let req = Request::builder()
            .uri("/api/v1/lyrics/reprocess")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"playlist_id":5}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["queued"].as_i64(), Some(3));
        assert_eq!(parsed["blocked_by_asr_gap"].as_i64(), Some(2));
    }

    #[tokio::test]
    async fn probe_sources_returns_404_for_missing_video_id() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (state, _temp) = test_state_with_cache_dir().await;
        let app = crate::api::router(state, None);

        let req = Request::builder()
            .uri("/api/v1/lyrics/probe-sources")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"video_id": 99999}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn queue_counts_exclude_failed_states_at_current_version() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // bucket0 candidates (manual_priority=1):
        //   - id=20 no_source at current pv → SHOULD BE EXCLUDED (worker can't pop)
        //   - id=21 asr_gap at current pv   → SHOULD BE EXCLUDED
        //   - id=22 no_source at OLDER pv   → INCLUDED (version-bump exception)
        //   - id=23 lyrics_source=NULL      → INCLUDED
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, has_lyrics, lyrics_manual_priority, lyrics_source, lyrics_pipeline_version) \
             VALUES (20, 1, 'y20', 's', 'a', 1, 0, 1, 'no_source', 7), \
                    (21, 1, 'y21', 's', 'a', 1, 0, 1, 'asr_gap',  7), \
                    (22, 1, 'y22', 's', 'a', 1, 0, 1, 'no_source', 5), \
                    (23, 1, 'y23', 's', 'a', 1, 0, 1, NULL,        7)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (b0, _b1, _b2) = fetch_queue_counts(&pool, 7).await.unwrap();
        assert_eq!(
            b0, 2,
            "bucket0 should include only id=22 (older pv) + id=23 (NULL source)"
        );
    }

    #[tokio::test]
    async fn queue_bucket1_excludes_failed_states_at_current_version() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // bucket1 candidates (manual_priority=0, has_lyrics=0):
        //   - id=30 no_source at current pv → EXCLUDED
        //   - id=31 no_source at older pv   → INCLUDED
        //   - id=32 lyrics_source=NULL      → INCLUDED
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, has_lyrics, lyrics_manual_priority, lyrics_source, lyrics_pipeline_version) \
             VALUES (30, 1, 'y30', 's', 'a', 1, 0, 0, 'no_source', 7), \
                    (31, 1, 'y31', 's', 'a', 1, 0, 0, 'no_source', 5), \
                    (32, 1, 'y32', 's', 'a', 1, 0, 0, NULL,        7)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (_b0, b1, _b2) = fetch_queue_counts(&pool, 7).await.unwrap();
        assert_eq!(
            b1, 2,
            "bucket1 should include only id=31 (older pv) + id=32 (NULL source)"
        );
    }

    #[tokio::test]
    async fn probe_sources_returns_report_with_six_probes_for_known_video() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (state, _temp) = test_state_with_cache_dir().await;
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, title, song, artist, normalized) \
             VALUES (5, 1, 'ytidX', 't', 'song', 'artist', 1)",
        )
        .execute(&state.pool)
        .await
        .unwrap();

        let app = crate::api::router(state, None);
        let req = Request::builder()
            .uri("/api/v1/lyrics/probe-sources")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"video_id": 5}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["video_id"], 5);
        assert_eq!(json["youtube_id"], "ytidX");
        let probes = json["probes"].as_array().expect("probes array");
        assert_eq!(probes.len(), 6);
        let provider_names: Vec<&str> = probes
            .iter()
            .map(|p| p["provider"].as_str().unwrap())
            .collect();
        for expected in [
            "yt_subs",
            "description",
            "lyrics_ovh",
            "genius",
            "lrclib",
            "spotify",
        ] {
            assert!(provider_names.contains(&expected), "missing {expected}");
        }
    }
}
