//! #223 S3b: the `video_hw_decode` setting's resolution and application,
//! #282: the decode cap (`max_resolution`), and the `video_decode` status
//! block. Wired via
//! `#[cfg(test)] #[path = "video_decode_tests.rs"] mod tests;`.
//!
//! Only `the_startup_applies_the_stored_setting_to_the_process_value`
//! touches the process value ([`global`]); every other test has its own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::config::{NdiSource, VideoDecode};
use sp_decoder::{DecodeMode, HwDecodeStats};
use tokio::sync::broadcast;

use super::{
    HwDecodeSetting, VIDEO_DECODE_SETTINGS_POLL, VideoDecodeStatus, apply_setting, global,
    load_hw_decode, load_max_height, run_settings_task, start, status, status_of,
};
use crate::db::models::set_setting;
use crate::playback::gpu_resident::{PipelineDecode, Resident, RingStatus};

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
    assert_eq!(setting.max_height(), 1440, "auto without hardware decode");
}

#[test]
fn set_max_height_says_whether_the_cap_changed() {
    let setting = HwDecodeSetting::default();
    assert!(setting.set_max_height(1080), "1440 → 1080 is a change");
    assert_eq!(setting.max_height(), 1080);
    assert!(!setting.set_max_height(1080), "1080 → 1080 is not");
    assert!(setting.set_max_height(2160));
    assert_eq!(setting.max_height(), 2160);
}

/// #282: the decode cap is `max_resolution` through the download's own
/// rule: auto = 2160 with hardware decode, else 1440; a number clamped to
/// 480..=2160.
#[tokio::test]
async fn the_decode_cap_follows_max_resolution_and_the_decode_setting() {
    let pool = settings_pool().await;
    assert_eq!(load_max_height(&pool, false).await.unwrap(), 1440);
    assert_eq!(load_max_height(&pool, true).await.unwrap(), 2160);
    set_setting(&pool, "max_resolution", "1080").await.unwrap();
    assert_eq!(load_max_height(&pool, true).await.unwrap(), 1080);
    assert_eq!(load_max_height(&pool, false).await.unwrap(), 1080);
    set_setting(&pool, "max_resolution", " 720 ").await.unwrap();
    assert_eq!(load_max_height(&pool, true).await.unwrap(), 720);
    set_setting(&pool, "max_resolution", "100").await.unwrap();
    assert_eq!(load_max_height(&pool, true).await.unwrap(), 480);
    set_setting(&pool, "max_resolution", "").await.unwrap();
    assert_eq!(load_max_height(&pool, true).await.unwrap(), 2160);
}

#[tokio::test]
async fn apply_sets_the_cap_the_producer_reads_and_says_what_changed() {
    let pool = settings_pool().await;
    let setting = HwDecodeSetting::default();
    assert!(
        !apply_setting(&pool, &setting).await,
        "the defaults: nothing new"
    );
    assert_eq!(setting.max_height(), 1440);
    set_setting(&pool, "max_resolution", "1080").await.unwrap();
    assert!(
        apply_setting(&pool, &setting).await,
        "a cap alone is a change"
    );
    assert_eq!(setting.max_height(), 1080);
    assert!(!setting.hw());
    assert!(!apply_setting(&pool, &setting).await, "nothing changed");
    set_setting(&pool, "video_hw_decode", "true").await.unwrap();
    assert!(
        apply_setting(&pool, &setting).await,
        "the decode mode alone (the cap stays 1080) is a change"
    );
    assert_eq!(setting.mode(), DecodeMode::Hardware);
    assert_eq!(setting.max_height(), 1080);
    set_setting(&pool, "max_resolution", "").await.unwrap();
    assert!(apply_setting(&pool, &setting).await);
    assert_eq!(setting.max_height(), 2160, "auto with hardware decode");
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
    setting.set_max_height(720);
    pool.close().await;
    assert!(!apply_setting(&pool, &setting).await);
    assert!(setting.hw(), "a failed read keeps the last applied value");
    assert_eq!(setting.max_height(), 720, "and the last applied cap");
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
    assert_eq!(global().max_height(), 2160, "auto with hardware decode");
    assert_eq!(status().max_height, 2160);
    // Leave the process value as a fresh process has it (the task, polling
    // every 5 s, reads the same "false" until the test's runtime ends).
    set_setting(&pool, "video_hw_decode", "false")
        .await
        .unwrap();
    global().set(false);
    global().set_max_height(1440);
    let _ = shutdown.send(());
}

#[test]
fn the_status_carries_the_setting_and_the_counters() {
    let setting = HwDecodeSetting::default();
    setting.set(true);
    setting.set_max_height(1080);
    let stats = HwDecodeStats {
        requested: 7,
        gpu_decodes: 4,
        mf_software: 1,
        open_fallbacks: 1,
        mid_stream_fallbacks: 2,
        path_changes: 5,
        last_fallback: Some("mid-stream: device removed".into()),
        scaled_opens: 9,
        scale_refusals: 3,
        resident_opens: 4,
    };
    let rows = vec![PipelineDecode {
        playlist_id: 5,
        gpu_decoding: true,
        ring: Some(RingStatus {
            slots: 20,
            vram_mb: 249,
            ..RingStatus::default()
        }),
    }];
    assert_eq!(
        status_of(&setting, stats, rows.clone()),
        VideoDecodeStatus {
            hw_decode: true,
            hw_requested: 7,
            gpu_decodes: 4,
            mf_software: 1,
            open_fallbacks: 1,
            mid_stream_fallbacks: 2,
            path_changes: 5,
            last_fallback: Some("mid-stream: device removed".into()),
            max_height: 1080,
            scaled_opens: 9,
            scale_refusals: 3,
            video_decode: "software".into(),
            gpu_resident_opens: 4,
            gpu_resident_blocked: None,
            pipelines: rows,
        }
    );
    setting.set_video_decode(VideoDecode::Gpu);
    let blocked = status_of(&setting, HwDecodeStats::default(), Vec::new());
    assert_eq!(blocked.video_decode, "gpu");
    assert_eq!(
        blocked.gpu_resident_blocked.as_deref(),
        Some("ndi_source_cpu")
    );
    setting.set_ndi_source(NdiSource::Gpu);
    let on = status_of(&setting, HwDecodeStats::default(), Vec::new());
    assert_eq!(on.gpu_resident_blocked, None, "it takes effect");
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
        path_changes: 4,
        last_fallback: Some("open: no video device".into()),
        max_height: 1080,
        scaled_opens: 6,
        scale_refusals: 2,
        video_decode: "gpu".into(),
        gpu_resident_opens: 5,
        gpu_resident_blocked: Some("ndi_source_cpu".into()),
        pipelines: vec![PipelineDecode {
            playlist_id: 7,
            gpu_decoding: true,
            ring: Some(RingStatus {
                slots: 20,
                in_use: 12,
                waits: 1,
                full: 0,
                dead: false,
                copy_us_p99: Some(380),
                vram_mb: 249,
            }),
        }],
    })
    .unwrap();
    assert_eq!(json["hw_decode"], true);
    assert_eq!(json["hw_requested"], 3);
    assert_eq!(json["gpu_decodes"], 2);
    assert_eq!(json["mf_software"], 0);
    assert_eq!(json["open_fallbacks"], 1);
    assert_eq!(json["mid_stream_fallbacks"], 0);
    assert_eq!(json["path_changes"], 4);
    assert_eq!(json["last_fallback"], "open: no video device");
    assert_eq!(json["max_height"], 1080);
    assert_eq!(json["scaled_opens"], 6);
    assert_eq!(json["scale_refusals"], 2);
    // #279 P3a.
    assert_eq!(json["video_decode"], "gpu");
    assert_eq!(json["gpu_resident_opens"], 5);
    assert_eq!(json["gpu_resident_blocked"], "ndi_source_cpu");
    let row = &json["pipelines"][0];
    assert_eq!(row["playlist_id"], 7);
    assert_eq!(row["gpu_decoding"], true);
    for (key, value) in [
        ("slots", 20),
        ("in_use", 12),
        ("waits", 1),
        ("full", 0),
        ("copy_us_p99", 380),
        ("vram_mb", 249),
    ] {
        assert_eq!(row["ring"][key], value, "{key}");
    }
    assert_eq!(row["ring"]["dead"], false);
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
        "path_changes",
        "max_height",
        "scaled_opens",
        "scale_refusals",
        "gpu_resident_opens",
    ] {
        assert!(block[key].is_u64(), "{key}: {json}");
    }
    assert!(block["video_decode"].is_string(), "{json}");
    assert!(block["pipelines"].is_array(), "{json}");
}

/// #279 P3a: `video_decode` = `gpu` takes effect only while NDI comes from
/// the GPU; each setting alone is a change the task applies.
#[tokio::test]
async fn video_decode_takes_effect_only_while_ndi_comes_from_the_gpu() {
    let pool = settings_pool().await;
    let setting = HwDecodeSetting::default();
    assert_eq!(setting.video_decode(), VideoDecode::Software);
    assert_eq!(setting.resident(), Resident::Off);
    set_setting(&pool, "video_decode", "gpu").await.unwrap();
    assert!(apply_setting(&pool, &setting).await, "video_decode alone");
    assert_eq!(setting.video_decode(), VideoDecode::Gpu);
    assert_eq!(setting.resident(), Resident::Blocked("ndi_source_cpu"));
    assert!(!apply_setting(&pool, &setting).await, "nothing new");
    set_setting(&pool, "program_ndi_source", "gpu")
        .await
        .unwrap();
    assert!(apply_setting(&pool, &setting).await, "the NDI source alone");
    assert_eq!(setting.resident(), Resident::On);
    assert_eq!(
        setting.mode(),
        DecodeMode::Software,
        "the base mode unchanged"
    );
    set_setting(&pool, "video_decode", "software")
        .await
        .unwrap();
    assert!(apply_setting(&pool, &setting).await);
    assert_eq!(setting.resident(), Resident::Off);
}

#[test]
fn the_setting_says_whether_each_value_changed() {
    let setting = HwDecodeSetting::default();
    assert!(
        !setting.set_video_decode(VideoDecode::Software),
        "the default"
    );
    assert!(setting.set_video_decode(VideoDecode::Gpu));
    assert!(!setting.set_video_decode(VideoDecode::Gpu));
    assert!(!setting.set_ndi_source(NdiSource::Cpu), "the default");
    assert!(setting.set_ndi_source(NdiSource::Gpu));
    assert!(!setting.set_ndi_source(NdiSource::Gpu));
    assert_eq!(setting.resident(), Resident::On);
    assert!(setting.set_ndi_source(NdiSource::Cpu));
    assert_eq!(setting.resident(), Resident::Blocked("ndi_source_cpu"));
    assert!(setting.set_video_decode(VideoDecode::Software));
    assert_eq!(setting.resident(), Resident::Off);
}
