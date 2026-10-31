//! #230: `GET /api/v1/background-hold` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::test_state;

async fn get_hold(state: crate::AppState) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri("/api/v1/background-hold")
        .body(Body::empty())
        .unwrap();
    let resp = crate::api::router(state, None).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn the_route_reports_no_hold_then_the_armed_one() {
    let state = test_state().await;
    let (status, json) = get_hold(state.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({
            "held": false,
            "until_utc_ms": null,
            "remaining_s": 0,
            "hold_scene": "sp-90s",
            "release_scene": "sp-slow",
            "held_jobs": [],
        })
    );
    crate::background_hold::hold_for_a_minute(&state.pool).await;
    let (status, json) = get_hold(state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["held"], true);
    assert!(json["until_utc_ms"].as_i64().is_some_and(|ms| ms > 0));
    let left = json["remaining_s"].as_u64().unwrap();
    assert!((59..=60).contains(&left), "{left}");
}

/// #275: `GET /api/v1/wall` carries whether SongPlayer's clip runs in Arena
/// (the Resolume drivers' facts, `resolume::sp_clip_store::global()`),
/// stamped with the server's clock at the answer. Running wins over every
/// other host's fact, so the process-wide store's other publishers (the
/// registry tests' drivers) cannot change it.
#[tokio::test]
async fn the_wall_route_carries_songplayer_s_clip_stamped_with_the_server_s_clock() {
    use sp_core::wall_clip::ClipView;

    use crate::resolume::sp_clips::Fact;

    let yt = ClipView {
        layer: "L# Fill".to_string(),
        clip: "Yt music".to_string(),
        source: "SP-program-MAX".to_string(),
    };
    let before = chrono::Utc::now().timestamp_millis();
    crate::resolume::sp_clip_store::global().publish(
        "route-test-275:8090",
        Fact {
            state: sp_core::wall_clip::ClipState::Running,
            clips: vec![yt],
            ..Fact::default()
        },
        before,
    );
    let req = Request::builder()
        .uri("/api/v1/wall")
        .body(Body::empty())
        .unwrap();
    let resp = crate::api::router(test_state().await, None)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let clip = &json["songplayer_clip"];
    assert_eq!(clip["state"], "running");
    assert!(
        clip["clips"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!({"layer": "L# Fill", "clip": "Yt music",
                                          "source": "SP-program-MAX"})),
        "{clip}"
    );
    assert!(clip["since_ms"].is_i64(), "{clip}");
    let now_ms = clip["now_ms"].as_i64().unwrap();
    assert!(
        now_ms >= before,
        "the answer's own clock: {now_ms} < {before}"
    );
}

/// `GET /api/v1/status` as JSON, through the real router.
async fn status_json(state: crate::AppState) -> serde_json::Value {
    let req = Request::builder()
        .uri("/api/v1/status")
        .body(Body::empty())
        .unwrap();
    let resp = crate::api::router(state, None).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// #261: `GET /api/v1/status` carries the data dir's size at its top level,
/// null until the first measure, then the last measure (`crate::data_size`;
/// the only test that sets that process-wide store).
#[tokio::test]
async fn the_status_carries_the_data_dir_size_null_until_measured() {
    use sp_core::data_size::DataSize;

    let state = test_state().await;
    let json = status_json(state.clone()).await;
    for key in ["data_dir_bytes", "cache_bytes", "data_dir_measured_ms"] {
        assert_eq!(
            json.as_object().unwrap().get(key),
            Some(&serde_json::Value::Null),
            "{key} present and null before a measure"
        );
    }
    crate::data_size::set(DataSize {
        data_dir_bytes: Some(5_000_000_000),
        cache_bytes: Some(4_000_000_000),
        data_dir_measured_ms: Some(1_760_000_000_000),
    });
    let json = status_json(state).await;
    crate::data_size::set(DataSize::default());
    assert_eq!(json["data_dir_bytes"], 5_000_000_000_u64);
    assert_eq!(json["cache_bytes"], 4_000_000_000_u64);
    assert_eq!(json["data_dir_measured_ms"], 1_760_000_000_000_i64);
}
