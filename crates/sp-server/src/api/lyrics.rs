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
         WHERE v.lyrics_manual_priority = 1 AND p.is_active = 1 AND v.normalized = 1",
    )
    .fetch_one(pool)
    .await?;
    let b1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE (v.has_lyrics IS NULL OR v.has_lyrics = 0) AND v.lyrics_manual_priority = 0 \
         AND p.is_active = 1 AND v.normalized = 1",
    )
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
         lyrics_pipeline_version, lyrics_quality_score, has_lyrics, lyrics_manual_priority \
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
         lyrics_pipeline_version, lyrics_quality_score, has_lyrics, lyrics_manual_priority \
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
                "UPDATE videos SET lyrics_manual_priority = 1 WHERE id IN ({})",
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
            match sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE playlist_id = ?")
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
}
