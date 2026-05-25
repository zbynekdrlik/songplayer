//! Tests for `api::lyrics`. Sibling file referenced by `lyrics.rs`
//! under `#[path = "lyrics_tests.rs"] #[cfg(test)] mod tests;` to keep
//! the handler file under the 1000-line airuleset cap.

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
async fn reprocess_clears_lyrics_source_for_terminal_no_lyrics_states() {
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
                    (14, 1, 'y14', 's', 'a', 1, 'yt_subs'), \
                    (15, 1, 'y15', 's', 'a', 1, 'unsupported_source')",
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
        .body(Body::from(r#"{"video_ids":[10,11,12,13,14,15]}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // After reprocess: 10/11/12/15 are terminal "no lyrics produced" states
    // and must be cleared to NULL so the worker's manual-priority pop guard
    // (which excludes those sentinels at the current pipeline version) will
    // pick them up. 13 (asr_gap, deliberate quarantine) and 14 (yt_subs, a
    // real source) stay untouched. ALL SIX get manual_priority=1.
    let rows: Vec<(i64, Option<String>, i64)> =
        sqlx::query_as("SELECT id, lyrics_source, lyrics_manual_priority FROM videos ORDER BY id")
            .fetch_all(&state.pool)
            .await
            .unwrap();
    let expected = vec![
        (10i64, None, 1i64),
        (11, None, 1),
        (12, None, 1),
        (13, Some("asr_gap".into()), 1), // untouched (deliberate quarantine)
        (14, Some("yt_subs".into()), 1), // untouched (real source)
        (15, None, 1),                   // unsupported_source cleared → re-poppable
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
async fn reprocess_all_stale_reports_blocked_asr_gap_count() {
    // #101: post_reprocess_all_stale must compute blocked_by_asr_gap
    // for operator parity with the targeted reprocess endpoints. The
    // count covers asr_gap rows that have `has_lyrics = 0` and a
    // stale pipeline_version, i.e. rows the "all stale" sweep would
    // pick up if they weren't quarantined.
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let (state, _temp) = test_state_with_cache_dir().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (7, 'p7', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    // 2 stale rows (has_lyrics=1, version=0) — should be queued.
    // 3 asr_gap rows (has_lyrics=0, version=0) — should count under blocked.
    // 1 fresh row (has_lyrics=1, version=LYRICS_PIPELINE_VERSION) — neither.
    let cur = crate::lyrics::LYRICS_PIPELINE_VERSION as i64;
    sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, song, artist, normalized, has_lyrics, lyrics_source, lyrics_pipeline_version) \
             VALUES (7, 'st1', 's', 'a', 1, 1, NULL,        0), \
                    (7, 'st2', 's', 'a', 1, 1, NULL,        0), \
                    (7, 'g1',  's', 'a', 1, 0, 'asr_gap',   0), \
                    (7, 'g2',  's', 'a', 1, 0, 'asr_gap',   0), \
                    (7, 'g3',  's', 'a', 1, 0, 'asr_gap',   0), \
                    (7, 'fr',  's', 'a', 1, 1, 'yt_subs',   ?)",
        )
        .bind(cur)
        .execute(&state.pool)
        .await
        .unwrap();

    let app = crate::api::router(state.clone(), None);
    let req = Request::builder()
        .uri("/api/v1/lyrics/reprocess-all-stale")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["queued"].as_i64(),
        Some(2),
        "only the 2 stale rows with has_lyrics=1 should be queued"
    );
    assert_eq!(
        parsed["blocked_by_asr_gap"].as_i64(),
        Some(3),
        "all 3 asr_gap rows at stale pipeline_version must surface in blocked_by_asr_gap"
    );
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
