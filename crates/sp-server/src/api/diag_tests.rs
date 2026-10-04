//! Router tests of `POST /api/v1/diag/decode-bench` (#223 S0), through the
//! real router (`api::router`). Every state has its own bench, so the 409
//! test cannot race another test. The real decoder runs only on the Windows
//! job, on the decoder crate's own H.264 fixture. On Linux the run is a 501.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::test_state_with_cache_dir;
use crate::diag::decode_bench::{BenchEnd, BenchReport, BenchRun, DecodeBench, StreamFacts};

/// A state whose bench dir is a fresh temp dir, which lives as long as the
/// returned `TempDir`.
async fn bench_state() -> (AppState, tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("bench");
    std::fs::create_dir_all(&dir).unwrap();
    let mut state = test_state_with_cache_dir(tmp.path().join("cache")).await;
    state.decode_bench = Arc::new(DecodeBench::new(dir.clone()));
    (state, tmp, dir)
}

fn body(file: &str, seconds: u64) -> String {
    serde_json::json!({ "file": file, "seconds": seconds }).to_string()
}

async fn post_bench(state: &AppState, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/diag/decode-bench")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = crate::api::router(state.clone(), None)
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[test]
fn a_report_is_a_500_only_when_the_decoder_failed() {
    let failed = BenchReport::open_failed("x.mp4", "open: no video".to_string(), 1, None);
    assert_eq!(
        super::report_status(&failed),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let run = BenchRun {
        decode_us: vec![1_000],
        first_picture: None,
        wall_us: 1_000,
        end: BenchEnd::EndOfStream,
        error: None,
    };
    let clean = BenchReport::from_run("x.mp4", StreamFacts::default(), run, 1, None);
    assert_eq!(super::report_status(&clean), StatusCode::OK);
}

#[tokio::test]
async fn a_name_that_is_not_bare_is_400_even_when_its_target_exists() {
    let (state, tmp, _dir) = bench_state().await;
    std::fs::write(tmp.path().join("outside.mp4"), b"x").unwrap();
    for file in ["../outside.mp4", "a/b.mp4", "a\\b.mp4", "C:x.mp4", ""] {
        let (status, text) = post_bench(&state, &body(file, 5)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{file:?}: {text}");
    }
}

#[tokio::test]
async fn seconds_outside_one_to_fifteen_is_400() {
    let (state, _tmp, dir) = bench_state().await;
    std::fs::write(dir.join("x.mp4"), b"x").unwrap();
    for seconds in [0, 16] {
        let (status, text) = post_bench(&state, &body("x.mp4", seconds)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{seconds}: {text}");
        assert!(text.contains("seconds"), "{text}");
    }
}

#[tokio::test]
async fn a_missing_sample_is_404_naming_where_it_was_looked_for() {
    let (state, _tmp, dir) = bench_state().await;
    let (status, text) = post_bench(&state, &body("av1_4k.mp4", 5)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    let looked_at = dir.join("av1_4k.mp4").display().to_string();
    assert!(text.contains(&looked_at), "{text}");
}

#[tokio::test]
async fn a_second_run_while_one_holds_the_bench_is_409() {
    let (state, _tmp, dir) = bench_state().await;
    std::fs::write(dir.join("x.mp4"), b"x").unwrap();
    let slot = state.decode_bench.try_start().expect("the bench is free");
    let (status, text) = post_bench(&state, &body("x.mp4", 1)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    drop(slot);
    let (status, text) = post_bench(&state, &body("x.mp4", 1)).await;
    assert_ne!(status, StatusCode::CONFLICT, "{text}");
}

#[cfg(not(windows))]
#[tokio::test]
async fn without_media_foundation_the_run_is_501_and_frees_the_bench() {
    let (state, _tmp, dir) = bench_state().await;
    std::fs::write(dir.join("x.mp4"), b"x").unwrap();
    for _ in 0..2 {
        let (status, text) = post_bench(&state, &body("x.mp4", 1)).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{text}");
    }
    assert!(state.decode_bench.try_start().is_some());
}

/// The decoder crate's H.264 fixture: 160×120, 30 fps, 3 s
/// (`crates/sp-decoder/tests/fixtures/regen.sh`).
#[cfg(windows)]
fn decoder_fixture() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("sp-decoder")
        .join("tests")
        .join("fixtures")
        .join("black_3s.mp4")
}

#[cfg(windows)]
#[tokio::test]
async fn the_real_decoder_measures_the_fixture() {
    let (state, _tmp, dir) = bench_state().await;
    std::fs::copy(decoder_fixture(), dir.join("h264.mp4")).unwrap();
    let (status, text) = post_bench(&state, &body("h264.mp4", 15)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["file"], "h264.mp4", "{text}");
    assert_eq!(report["codec"], "H264", "{text}");
    assert_eq!(report["ended"], "end_of_stream", "{text}");
    assert!(report["error"].is_null(), "{text}");
    assert!(report["frames"].as_u64().unwrap() > 0, "{text}");
    // MF may pad the picture to 16 pixels.
    let width = report["width"].as_u64().unwrap();
    assert!((160..=176).contains(&width), "{text}");
    assert!(report["stride"].as_u64().unwrap() >= width, "{text}");
    let fps = report["source_fps"].as_f64().unwrap();
    assert!((fps - 30.0).abs() < 0.01, "{text}");
    assert!(report["budget"]["frame_period_us"].is_u64(), "{text}");
    // Started like the paced producer: CreateThread's THREAD_PRIORITY_NORMAL.
    assert_eq!(report["thread_priority"], 0, "{text}");
    let p50 = report["decode_us"]["p50"].as_u64().unwrap();
    let max = report["decode_us"]["max"].as_u64().unwrap();
    assert!(p50 <= max, "{text}");
    assert!(
        state.decode_bench.try_start().is_some(),
        "the bench is free after the run"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn a_file_the_decoder_cannot_read_is_500_with_the_error() {
    let (state, _tmp, dir) = bench_state().await;
    std::fs::write(dir.join("not_a_video.mp4"), b"not a video").unwrap();
    let (status, text) = post_bench(&state, &body("not_a_video.mp4", 1)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{text}");
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["ended"], "error", "{text}");
    assert!(!report["error"].as_str().unwrap().is_empty(), "{text}");
    assert_eq!(report["frames"], 0, "{text}");
}
