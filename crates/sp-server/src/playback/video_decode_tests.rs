//! #223 S3b: the `video_hw_decode` setting's resolution and application,
//! and the `video_decode` status block. Wired via
//! `#[cfg(test)] #[path = "video_decode_tests.rs"] mod tests;`.
//!
//! Only `the_startup_applies_the_stored_setting_to_the_process_value`
//! touches the process value ([`global`]); every other test has its own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_decoder::{DecodeMode, HwDecodeStats};
use tokio::sync::broadcast;

use super::{
    HwDecodeSetting, VIDEO_DECODE_SETTINGS_POLL, VideoDecodeStatus, apply_setting, global,
    load_hw_decode, run_settings_task, start, status, status_of,
};
use crate::db::models::set_setting;

async fn settings_pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// Poll `done` on the runtime until it holds (bounded: 20 s).
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[test]
fn a_new_setting_is_software() {
    let setting = HwDecodeSetting::default();
    assert!(!setting.hw(), "OFF until the box gate passes");
    assert_eq!(setting.mode(), DecodeMode::Software);
}

#[test]
fn set_says_whether_it_changed_and_the_mode_follows() {
    let setting = HwDecodeSetting::default();
    assert!(setting.set(true), "OFF → ON is a change");
    assert!(setting.hw());
    assert_eq!(setting.mode(), DecodeMode::Hardware);
    assert!(!setting.set(true), "ON → ON is not");
    assert!(setting.set(false), "ON → OFF is");
    assert!(!setting.hw());
    assert_eq!(setting.mode(), DecodeMode::Software);
    assert!(!setting.set(false), "OFF → OFF is not");
}

#[tokio::test]
async fn the_stored_setting_is_on_only_for_true() {
    let pool = settings_pool().await;
    assert!(!load_hw_decode(&pool).await.unwrap(), "no setting = OFF");
    set_setting(&pool, "video_hw_decode", "true").await.unwrap();
    assert!(load_hw_decode(&pool).await.unwrap());
    set_setting(&pool, "video_hw_decode", "yes").await.unwrap();
    assert!(!load_hw_decode(&pool).await.unwrap(), "only \"true\"");
}

#[tokio::test]
async fn apply_sets_the_value_the_producer_reads() {
    let pool = settings_pool().await;
    let setting = HwDecodeSetting::default();
    set_setting(&pool, "video_hw_decode", "true").await.unwrap();
    apply_setting(&pool, &setting).await;
    assert_eq!(setting.mode(), DecodeMode::Hardware);
    set_setting(&pool, "video_hw_decode", "false")
        .await
        .unwrap();
    apply_setting(&pool, &setting).await;
    assert_eq!(setting.mode(), DecodeMode::Software);
}

#[tokio::test]
async fn an_unreadable_setting_changes_nothing() {
    let pool = settings_pool().await;
    let setting = HwDecodeSetting::default();
    setting.set(true);
    pool.close().await;
    apply_setting(&pool, &setting).await;
    assert!(setting.hw(), "a failed read keeps the last applied value");
}

#[tokio::test]
async fn the_settings_task_applies_every_change_until_shutdown() {
    let pool = settings_pool().await;
    let setting = Arc::new(HwDecodeSetting::default());
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(run_settings_task(
        pool.clone(),
        setting.clone(),
        shutdown.subscribe(),
        Duration::from_millis(5),
    ));
    set_setting(&pool, "video_hw_decode", "true").await.unwrap();
    eventually("the switch-on applied", || setting.hw()).await;
    set_setting(&pool, "video_hw_decode", "false")
        .await
        .unwrap();
    eventually("the switch-off applied", || !setting.hw()).await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the task ends on shutdown")
        .unwrap();
    assert_eq!(VIDEO_DECODE_SETTINGS_POLL, Duration::from_secs(5));
}

#[tokio::test]
async fn the_startup_applies_the_stored_setting_to_the_process_value() {
    assert!(Arc::ptr_eq(&global(), &global()), "one process value");
    let pool = settings_pool().await;
    set_setting(&pool, "video_hw_decode", "true").await.unwrap();
    let (shutdown, _) = broadcast::channel(1);
    start(&pool, &shutdown).await;
    assert!(global().hw(), "applied before start returns");
    assert!(status().hw_decode, "the status reads the process value");
    // Leave the process value as a fresh process has it (the task, polling
    // every 5 s, reads the same "false" until the test's runtime ends).
    set_setting(&pool, "video_hw_decode", "false")
        .await
        .unwrap();
    global().set(false);
    let _ = shutdown.send(());
}

#[test]
fn the_status_carries_the_setting_and_the_counters() {
    let setting = HwDecodeSetting::default();
    setting.set(true);
    let stats = HwDecodeStats {
        requested: 7,
        gpu_decodes: 4,
        mf_software: 1,
        open_fallbacks: 1,
        mid_stream_fallbacks: 2,
        last_fallback: Some("mid-stream: device removed".into()),
    };
    assert_eq!(
        status_of(&setting, stats),
        VideoDecodeStatus {
            hw_decode: true,
            hw_requested: 7,
            gpu_decodes: 4,
            mf_software: 1,
            open_fallbacks: 1,
            mid_stream_fallbacks: 2,
            last_fallback: Some("mid-stream: device removed".into()),
        }
    );
}

#[test]
fn the_process_status_reads_the_decoders_counters() {
    sp_decoder::hw_counters().requested();
    assert!(status().hw_requested >= 1);
}

#[test]
fn the_status_serializes_to_the_documented_json() {
    let json = serde_json::to_value(VideoDecodeStatus {
        hw_decode: true,
        hw_requested: 3,
        gpu_decodes: 2,
        mf_software: 0,
        open_fallbacks: 1,
        mid_stream_fallbacks: 0,
        last_fallback: Some("open: no video device".into()),
    })
    .unwrap();
    assert_eq!(json["hw_decode"], true);
    assert_eq!(json["hw_requested"], 3);
    assert_eq!(json["gpu_decodes"], 2);
    assert_eq!(json["mf_software"], 0);
    assert_eq!(json["open_fallbacks"], 1);
    assert_eq!(json["mid_stream_fallbacks"], 0);
    assert_eq!(json["last_fallback"], "open: no video device");
    // A block from an older build, missing keys: each reads as its zero.
    let old: VideoDecodeStatus = serde_json::from_str(r#"{"hw_decode": true}"#).unwrap();
    assert_eq!(
        old,
        VideoDecodeStatus {
            hw_decode: true,
            ..VideoDecodeStatus::default()
        }
    );
}

/// `GET /api/v1/status` carries the block, through the real router.
#[tokio::test]
async fn the_status_route_carries_the_video_decode_block() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    sp_decoder::hw_counters().requested();
    let state = crate::api::routes::tests::test_state().await;
    let resp = crate::api::router(state, None)
        .oneshot(
            Request::builder()
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let block = &json["video_decode"];
    assert!(block["hw_decode"].is_boolean(), "{json}");
    assert!(block["hw_requested"].as_u64().unwrap() >= 1, "{json}");
    for key in [
        "gpu_decodes",
        "mf_software",
        "open_fallbacks",
        "mid_stream_fallbacks",
    ] {
        assert!(block[key].is_u64(), "{key}: {json}");
    }
}
