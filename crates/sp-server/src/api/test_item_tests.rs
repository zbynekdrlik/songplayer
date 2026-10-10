//! #228: `/api/v1/test-item` and the hidden test playlist, through the real
//! router. Wired via `#[cfg(test)] #[path = "test_item_tests.rs"] mod
//! tests;` in `api/test_item.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use crate::AppState;
use crate::EngineCommand;
use crate::api::routes::tests::{app, test_state, test_state_with_engine_rx};
use crate::diag::decode_bench::DecodeBench;
use crate::downloader::tools::ToolPaths;
use crate::test_item::{TEST_CLIP_FILE, ensure_playlist};

/// One request through the real router: its status and its JSON body
/// (`Null` for a body that is not JSON).
async fn call(
    state: AppState,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app(state).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// `state` with its sample dir at `dir` and an ffmpeg path (never run here).
async fn with_samples(mut state: AppState, dir: &std::path::Path) -> AppState {
    state.decode_bench = Arc::new(DecodeBench::new(dir.to_path_buf()));
    *state.tool_paths.write().await = Some(ToolPaths {
        ytdlp: PathBuf::from("tools").join("yt-dlp.exe"),
        ffmpeg: PathBuf::from("tools").join("ffmpeg.exe"),
        python: None,
        deno: None,
    });
    state
}

/// The test playlist and its downloaded video, as an import leaves them.
async fn imported(state: &AppState) -> (i64, i64) {
    let playlist = ensure_playlist(&state.pool).await.unwrap();
    let video: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, duration_ms) \
         VALUES (?, 'measure-v01', 1, 128000) RETURNING id",
    )
    .bind(playlist)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    (playlist, video)
}

#[tokio::test]
async fn the_test_item_route_names_the_clip_before_the_import() {
    let state = test_state().await;
    let (status, json) = call(state, "GET", "/api/v1/test-item", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["imported"], json!(false));
    assert!(json["item"].is_null());
    assert_eq!(json["clip"], json!(TEST_CLIP_FILE));
    let sha = json["sha256"].as_str().unwrap();
    assert_eq!(sha.len(), 64);
    assert!(sha.starts_with("a0118ad7"), "{sha}");
}

#[tokio::test]
async fn the_test_item_route_serves_its_ids_once_imported() {
    let state = test_state().await;
    let (playlist, video) = imported(&state).await;
    let (_, json) = call(state, "GET", "/api/v1/test-item", None).await;
    assert_eq!(json["imported"], json!(true));
    assert_eq!(
        json["item"],
        json!({
            "playlist_id": playlist,
            "video_id": video,
            "youtube_id": "measure-v01",
            "ndi_output_name": "SP-test",
            "scene": "sp-test",
            "duration_ms": 128000,
        })
    );
}

#[tokio::test]
async fn an_import_names_a_bare_file_in_the_sample_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = with_samples(test_state().await, dir.path()).await;
    let bad = Some(json!({ "file": "../songplayer.db" }));
    let (status, _) = call(state.clone(), "POST", "/api/v1/test-item/import", bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let missing = Some(json!({ "file": TEST_CLIP_FILE }));
    let (status, _) = call(state, "POST", "/api/v1/test-item/import", missing).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_import_waits_for_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(TEST_CLIP_FILE), b"clip").unwrap();
    let mut state = test_state().await;
    state.decode_bench = Arc::new(DecodeBench::new(dir.path().to_path_buf()));
    let body = Some(json!({ "file": TEST_CLIP_FILE }));
    let (status, _) = call(state, "POST", "/api/v1/test-item/import", body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A file that is not the pinned clip: 422 with both hashes, nothing made.
#[tokio::test]
async fn an_import_of_another_file_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(TEST_CLIP_FILE), b"some other video").unwrap();
    let state = with_samples(test_state().await, dir.path()).await;
    let body = Some(json!({ "file": TEST_CLIP_FILE }));
    let (status, json) = call(state.clone(), "POST", "/api/v1/test-item/import", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"], json!("not the measurement clip"));
    assert_eq!(json["expected"], json!(crate::test_item::clip_sha256()));
    assert_eq!(
        json["found"],
        json!(crate::peer::hash::sha256_hex(b"some other video"))
    );
    let playlists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlists")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(playlists, 0);
}

#[tokio::test]
async fn start_plays_the_item_from_0_and_stop_pauses_it() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    let (playlist, video) = imported(&state).await;
    let (status, _) = call(state.clone(), "POST", "/api/v1/test-item/start", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    match engine.try_recv() {
        Ok(EngineCommand::PlayVideo {
            playlist_id,
            video_id,
            position_ms,
        }) => assert_eq!(
            (playlist_id, video_id, position_ms),
            (playlist, video, Some(0))
        ),
        other => panic!("expected PlayVideo, got {other:?}"),
    }
    let (status, _) = call(state, "POST", "/api/v1/test-item/stop", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    match engine.try_recv() {
        Ok(EngineCommand::Pause { playlist_id }) => assert_eq!(playlist_id, playlist),
        other => panic!("expected Pause, got {other:?}"),
    }
}

#[tokio::test]
async fn start_and_stop_need_the_imported_item() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    for route in ["/api/v1/test-item/start", "/api/v1/test-item/stop"] {
        let (status, _) = call(state.clone(), "POST", route, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
    }
    assert!(engine.try_recv().is_err(), "nothing told to the engine");
}

/// The test playlist is not an operator playlist: the playlist list (the
/// dashboard, the Program control, the Live page) leaves it out.
#[tokio::test]
async fn the_playlist_list_leaves_the_test_playlist_out() {
    let state = test_state().await;
    imported(&state).await;
    sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('fast', 'u', 'SP-fast')")
        .execute(&state.pool)
        .await
        .unwrap();
    let (status, json) = call(state, "GET", "/api/v1/playlists", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fast"]);
}
