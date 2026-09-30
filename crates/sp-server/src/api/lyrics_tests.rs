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
        ndi_burn_registry: Arc::new(crate::playback::ndi_burn::NdiBurnRegistry::new()),
        preview_registry: Arc::new(crate::playback::preview::PreviewRegistry::new()),
        program_bus: Arc::new(crate::playback::program_bus::ProgramBus::new()),
        lan_status: crate::mdns::new_status_handle(),
        metadata_chain: std::sync::Arc::new(crate::metadata::ProviderChain::new(vec![])),
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

/// #144: an operator reprocess of a video (manual priority) must clear the
/// durable retry backoff so the manual retry runs immediately, not after the
/// exponential `lyrics_next_attempt_at` elapses.
#[tokio::test]
async fn reprocess_clears_lyrics_retry_backoff() {
    let (state, _temp) = test_state_with_cache_dir().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    // A row the worker deferred: backed off into the future with attempts > 0.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, \
             lyrics_source, lyrics_attempts, lyrics_next_attempt_at) \
             VALUES (10, 1, 'y10', 's', 'a', 1, 'no_source', 3, '2999-01-01T00:00:00.000Z')",
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
        .body(Body::from(r#"{"video_ids":[10]}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let (attempts, next_attempt_at): (i64, Option<String>) =
        sqlx::query_as("SELECT lyrics_attempts, lyrics_next_attempt_at FROM videos WHERE id = 10")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0, "reprocess must reset lyrics_attempts to 0");
    assert!(
        next_attempt_at.is_none(),
        "reprocess must clear lyrics_next_attempt_at so the manual retry is immediate"
    );
}

// ---------------------------------------------------------------------------
// #144: ONE per-song reprocess path, and it never blanks the lyrics the wall
// serves. `POST /api/v1/videos/{id}/lyrics/reprocess` used to call
// `reset_video_lyrics` (`has_lyrics = 0, lyrics_source = NULL`), so every
// queued song showed NO lyrics on the wall until the worker reached it —
// 211 songs for ~6 h on 30.9.2026. `POST /api/v1/lyrics/reprocess` is the one
// per-song path: manual priority, the served lyrics stay while the song waits
// in the queue.
// ---------------------------------------------------------------------------

const SERVED_SOURCE: &str = "description+mtl@rev1/g35t-ok";
const SERVED_TRACK: &str = r#"{"version":22,"source":"description+mtl@rev1/g35t-ok","language_source":"en","language_translation":"sk","lines":[{"start_ms":1000,"end_ms":4000,"en":"Holy is the Lord","sk":"Svätý je Pán"}]}"#;

/// Seed video 49 as a song the wall is serving: `has_lyrics = 1`, a ★
/// source, and its `<youtube_id>_lyrics.json` on disk.
async fn seed_served_song(state: &crate::AppState) {
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, \
             has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_manual_priority) \
             VALUES (49, 1, 'y49', 's', 'a', 1, 1, ?, 22, 0)",
    )
    .bind(SERVED_SOURCE)
    .execute(&state.pool)
    .await
    .unwrap();
    tokio::fs::write(state.cache_dir.join("y49_lyrics.json"), SERVED_TRACK)
        .await
        .unwrap();
}

/// Send one request through the real router; return its status and body.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> (axum::http::StatusCode, Vec<u8>) {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let req = Request::builder()
        .uri(uri)
        .method(method)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// `(has_lyrics, lyrics_source, lyrics_manual_priority)` of video 49.
async fn served_row(pool: &sqlx::SqlitePool) -> (i64, Option<String>, i64) {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, lyrics_manual_priority FROM videos WHERE id = 49",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_per_video_reprocess_request_never_blanks_the_served_lyrics() {
    use axum::http::StatusCode;
    let (state, _temp) = test_state_with_cache_dir().await;
    seed_served_song(&state).await;
    let app = crate::api::router(state.clone(), None);
    let (status, served_before) = send(&app, "GET", "/api/v1/videos/49/lyrics", "").await;
    assert_eq!(status, StatusCode::OK, "the seeded song serves its lyrics");

    let (reprocess_status, _) = send(&app, "POST", "/api/v1/videos/49/lyrics/reprocess", "").await;

    let (has_lyrics, source, _) = served_row(&state.pool).await;
    assert_eq!(
        has_lyrics, 1,
        "a reprocess request must never set has_lyrics = 0: the wall would show no \
         lyrics for the song until the worker reaches it"
    );
    assert_eq!(
        source.as_deref(),
        Some(SERVED_SOURCE),
        "a reprocess request must keep lyrics_source"
    );
    let (status, served_after) = send(&app, "GET", "/api/v1/videos/49/lyrics", "").await;
    assert_eq!(status, StatusCode::OK, "the song still serves its lyrics");
    assert_eq!(
        served_after, served_before,
        "the served lyrics are unchanged"
    );
    assert_eq!(
        reprocess_status,
        StatusCode::NOT_FOUND,
        "the per-video path is deleted: POST /api/v1/lyrics/reprocess is the one per-song path"
    );
}

#[tokio::test]
async fn the_one_reprocess_path_keeps_the_served_lyrics_and_sets_manual_priority() {
    use axum::http::StatusCode;
    let (state, _temp) = test_state_with_cache_dir().await;
    seed_served_song(&state).await;
    let app = crate::api::router(state.clone(), None);
    let (status, served_before) = send(&app, "GET", "/api/v1/videos/49/lyrics", "").await;
    assert_eq!(status, StatusCode::OK, "the seeded song serves its lyrics");

    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/lyrics/reprocess",
        r#"{"video_ids":[49]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["queued"].as_i64(), Some(1));

    assert_eq!(
        served_row(&state.pool).await,
        (1, Some(SERVED_SOURCE.to_owned()), 1),
        "the one path queues the song through manual priority and keeps has_lyrics \
         and lyrics_source"
    );
    let (status, served_after) = send(&app, "GET", "/api/v1/videos/49/lyrics", "").await;
    assert_eq!(status, StatusCode::OK, "the song still serves its lyrics");
    assert_eq!(
        served_after, served_before,
        "the wall keeps the served lyrics while the song waits in the queue"
    );
}
