//! #228: `/api/v1/test-item` and the hidden test playlist, through the real
//! router; #272: the pinned clip list (the GET's `clips`, the start's
//! `{"clip"}`, the list import). Wired via `#[cfg(test)] #[path =
//! "test_item_tests.rs"] mod tests;` in `api/test_item.rs`.

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
use crate::test_item::{
    CAMERA_BOX, CLIPS, Clip, ClipImport, ClipOutcome, TEST_CLIP_FILE, WALL_24, WALL_25, WALL_30,
    ensure_playlist,
};

use super::{
    IMPORT_BODY_REFUSED, START_BODY_REFUSED, StartRequest, UNKNOWN_CLIP, body_or_default,
    ensure_after_list, list_imported_any, list_status, start_clip,
};

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
    let clips = json["clips"].as_array().unwrap();
    assert_eq!(clips.len(), CLIPS.len());
    for (got, clip) in clips.iter().zip(CLIPS.iter()) {
        let want = json!({
            "name": clip.name,
            "file": clip.file,
            "fps": clip.fps,
            "sha256": clip.sha256_hex(),
            "imported": false,
        });
        assert_eq!(got, &want);
    }
    assert_eq!(
        clips[0]["sha256"], json["sha256"],
        "camera-box's clip first"
    );
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
    let any: Vec<String> = CLIPS.iter().map(Clip::sha256_hex).collect();
    assert_eq!(json["expected_any"], json!(any));
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

// #272: the pinned clip list through the routes.

/// One request with a RAW body (any bytes, or none for ""): its status and
/// its body as text.
async fn call_raw(state: AppState, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// `clip`'s row in the test playlist, as its import leaves it.
async fn imported_clip(state: &AppState, clip: &Clip) -> (i64, i64) {
    let playlist = ensure_playlist(&state.pool).await.unwrap();
    let video: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, duration_ms) \
         VALUES (?, ?, 1, ?) RETURNING id",
    )
    .bind(playlist)
    .bind(clip.youtube_id)
    .bind(clip.duration_ms)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    (playlist, video)
}

async fn playlist_count(state: &AppState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM playlists")
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

/// The `clips` of `GET /api/v1/test-item`: which listed clip is imported.
#[tokio::test]
async fn the_route_tells_which_listed_clips_are_imported() {
    let state = test_state().await;
    imported_clip(&state, &WALL_25).await;
    let (status, json) = call(state.clone(), "GET", "/api/v1/test-item", None).await;
    assert_eq!(status, StatusCode::OK);
    let flags: Vec<bool> = json["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["imported"].as_bool().unwrap())
        .collect();
    assert_eq!(flags, [false, false, true, false]);
    // camera-box's own fields are as before: not imported.
    assert_eq!(json["imported"], json!(false));
    assert!(json["item"].is_null());
    imported_clip(&state, &CAMERA_BOX).await;
    let (_, json) = call(state, "GET", "/api/v1/test-item", None).await;
    assert_eq!(json["imported"], json!(true));
    assert_eq!(json["item"]["youtube_id"], json!("measure-v01"));
    assert_eq!(json["clips"][0]["imported"], json!(true));
    assert_eq!(json["clips"][2]["imported"], json!(true));
}

/// No body, an empty or blank body, `{}`, a null clip and camera-box's name
/// all start camera-box's clip; a wall clip's name starts that clip.
#[tokio::test]
async fn start_plays_the_named_clip_and_by_default_camera_box_s() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    let (playlist, camera) = imported(&state).await;
    let (same, wall) = imported_clip(&state, &WALL_25).await;
    assert_eq!(same, playlist, "one test playlist");
    let cases = [
        ("", camera),
        ("  \r\n\t", camera),
        ("{}", camera),
        (r#"{"clip": null}"#, camera),
        (r#"{"clip": "camera-box"}"#, camera),
        (r#"{"clip": "wall-25"}"#, wall),
    ];
    for (body, video) in cases {
        let (status, _) = call_raw(state.clone(), "/api/v1/test-item/start", body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
        match engine.try_recv() {
            Ok(EngineCommand::PlayVideo {
                playlist_id,
                video_id,
                position_ms,
            }) => assert_eq!(
                (playlist_id, video_id, position_ms),
                (playlist, video, Some(0)),
                "{body:?}"
            ),
            other => panic!("{body:?}: expected PlayVideo, got {other:?}"),
        }
    }
}

/// A name no clip has is 400 with a fixed text, a body that does not read
/// is 400 with another; nothing reaches the engine.
#[tokio::test]
async fn a_start_naming_no_listed_clip_or_not_reading_is_refused() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    imported(&state).await;
    imported_clip(&state, &WALL_25).await;
    for body in [
        r#"{"clip": "wall-26"}"#,
        r#"{"clip": ""}"#,
        r#"{"clip": "WALL-25"}"#,
    ] {
        let (status, text) = call_raw(state.clone(), "/api/v1/test-item/start", body).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, UNKNOWN_CLIP)
        );
    }
    let unreadable = [
        "not json",
        r#"{"clip": 25}"#,
        r#"{"clip": "wall-25", "at": 0}"#,
        r#""wall-25""#,
    ];
    for body in unreadable {
        let (status, text) = call_raw(state.clone(), "/api/v1/test-item/start", body).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, START_BODY_REFUSED),
            "{body:?}"
        );
    }
    assert!(engine.try_recv().is_err(), "nothing told to the engine");
}

/// A listed clip that is not imported yet is 404; stop pauses the test
/// playlist whichever clip is imported.
#[tokio::test]
async fn a_clip_must_be_imported_to_start_and_any_clip_lets_stop_pause() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    let (playlist, _) = imported_clip(&state, &WALL_30).await;
    for body in ["", r#"{"clip": "wall-24"}"#] {
        let (status, _) = call_raw(state.clone(), "/api/v1/test-item/start", body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    }
    assert!(engine.try_recv().is_err());
    let (status, _) = call(state, "POST", "/api/v1/test-item/stop", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    match engine.try_recv() {
        Ok(EngineCommand::Pause { playlist_id }) => assert_eq!(playlist_id, playlist),
        other => panic!("expected Pause, got {other:?}"),
    }
}

/// The list import (no body, or `{}`) of an empty sample dir: every clip
/// absent, 200, nothing made, nothing told to the engine.
#[tokio::test]
async fn the_list_import_of_an_empty_sample_dir_imports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mut engine) = test_state_with_engine_rx().await;
    let state = with_samples(state, dir.path()).await;
    for body in ["", "{}", r#"{"file": null}"#] {
        let (status, text) = call_raw(state.clone(), "/api/v1/test-item/import", body).await;
        assert_eq!(status, StatusCode::OK, "{body:?}: {text}");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let want: Vec<serde_json::Value> = CLIPS
            .iter()
            .map(|c| json!({"name": c.name, "file": c.file, "outcome": "absent"}))
            .collect();
        assert_eq!(json, json!({ "clips": want }));
    }
    assert_eq!(playlist_count(&state).await, 0);
    assert!(engine.try_recv().is_err());
}

/// A file under a listed clip's name that is not that clip: named with its
/// sha256, nothing written; the other clips are absent.
#[tokio::test]
async fn the_list_import_names_a_file_that_is_not_its_clip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(WALL_25.file), b"another video").unwrap();
    let (state, mut engine) = test_state_with_engine_rx().await;
    let state = with_samples(state, dir.path()).await;
    let (status, text) = call_raw(state.clone(), "/api/v1/test-item/import", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let outcomes: Vec<&str> = json["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["absent", "absent", "wrong_file", "absent"]);
    assert_eq!(json["clips"][2]["name"], json!("wall-25"));
    let found = crate::peer::hash::sha256_hex(b"another video");
    assert_eq!(json["clips"][2]["found"], json!(found));
    assert_eq!(playlist_count(&state).await, 0);
    assert!(engine.try_recv().is_err());
}

#[tokio::test]
async fn the_list_import_waits_for_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state().await;
    state.decode_bench = Arc::new(DecodeBench::new(dir.path().to_path_buf()));
    let (status, _) = call_raw(state, "/api/v1/test-item/import", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn an_import_body_that_does_not_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = with_samples(test_state().await, dir.path()).await;
    for body in ["nope", r#"{"file": 3}"#, r#"{"name": "clip.mp4"}"#, "7"] {
        let (status, text) = call_raw(state.clone(), "/api/v1/test-item/import", body).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::BAD_REQUEST, IMPORT_BODY_REFUSED),
            "{body:?}"
        );
    }
    assert_eq!(playlist_count(&state).await, 0);
}

#[test]
fn a_body_reads_as_its_struct_or_as_the_default() {
    for blank in [&b""[..], &b" "[..], &b" \r\n\t"[..]] {
        let req: StartRequest = body_or_default(blank).unwrap();
        assert_eq!(req.clip, None);
    }
    let req: StartRequest = body_or_default(br#"{"clip":"wall-24"}"#).unwrap();
    assert_eq!(req.clip.as_deref(), Some("wall-24"));
    assert!(body_or_default::<StartRequest>(b"{").is_err());
    assert!(body_or_default::<StartRequest>(br#"{"other":1}"#).is_err());
    assert_eq!(start_clip(&StartRequest::default()), Some(&CAMERA_BOX));
    let named = |name: &str| StartRequest {
        clip: Some(name.to_string()),
    };
    assert_eq!(start_clip(&named("wall-30")), Some(&WALL_30));
    assert_eq!(start_clip(&named("wall-24")), Some(&WALL_24));
    assert_eq!(start_clip(&named("x")), None);
}

/// The list import's outcomes, its status, whether a clip is in place.
type OutcomeRow = (&'static [ClipOutcome], StatusCode, bool);

#[test]
fn the_list_import_s_status_and_pipeline_follow_the_outcomes() {
    use ClipOutcome::{Absent, Already, Failed, Imported, WrongFile};
    let answers = |outcomes: &[ClipOutcome]| -> Vec<ClipImport> {
        let one = |outcome| ClipImport {
            name: "x",
            file: "x.mp4",
            outcome,
            found: None,
            error: None,
        };
        outcomes.iter().copied().map(one).collect()
    };
    let ok = StatusCode::OK;
    let failed = StatusCode::INTERNAL_SERVER_ERROR;
    let table: [OutcomeRow; 7] = [
        (&[], ok, false),
        (&[Absent, Absent], ok, false),
        (&[WrongFile], ok, false),
        (&[Absent, Imported], ok, true),
        (&[Already], ok, true),
        (&[Imported, Failed], failed, true),
        (&[Failed], failed, false),
    ];
    for (outcomes, status, any) in table {
        let list = answers(outcomes);
        assert_eq!(list_status(&list), status, "{outcomes:?}");
        assert_eq!(list_imported_any(&list), any, "{outcomes:?}");
    }
}

/// After the list import the engine is told to make the test playlist's
/// pipeline — only when a clip is in it.
#[tokio::test]
async fn after_the_list_import_the_test_playlist_s_pipeline_is_made() {
    let (state, mut engine) = test_state_with_engine_rx().await;
    let absent = ClipImport {
        name: WALL_25.name,
        file: WALL_25.file,
        outcome: ClipOutcome::Absent,
        found: None,
        error: None,
    };
    ensure_after_list(&state, std::slice::from_ref(&absent)).await;
    assert!(engine.try_recv().is_err(), "nothing imported, nothing told");
    let (playlist, _) = imported_clip(&state, &WALL_25).await;
    let imported = ClipImport {
        outcome: ClipOutcome::Imported,
        ..absent.clone()
    };
    ensure_after_list(&state, &[absent, imported]).await;
    match engine.try_recv() {
        Ok(EngineCommand::EnsurePipeline { playlist_id }) => assert_eq!(playlist_id, playlist),
        other => panic!("expected EnsurePipeline, got {other:?}"),
    }
}
