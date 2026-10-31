//! Router tests of `POST /api/v1/diag/decode-bench {"probe": true}` (#279
//! P0), through the real router. Off Windows the probe is a 501 that frees
//! the bench; on the Windows job the real probe runs on the runner's
//! adapters (no GPU: a 500 naming no adapter, or the answer).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::test_state;
use crate::diag::decode_bench::DecodeBench;

/// A state with its own bench gate, so the 409 test cannot race another.
async fn state() -> AppState {
    let mut state = test_state().await;
    let dir = std::env::temp_dir().join("sp-gpu-probe-unused");
    state.decode_bench = Arc::new(DecodeBench::new(dir));
    state
}

async fn post(state: &AppState, body: &str) -> (StatusCode, String) {
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

/// A probe names nothing else; a decode run names its file and seconds:
/// anything else is the handler's 400 (the bench stays free).
#[tokio::test]
async fn a_request_that_is_neither_a_probe_nor_a_whole_run_is_400() {
    let state = state().await;
    for body in [
        r#"{"probe":true,"file":"x.mp4"}"#,
        r#"{"probe":true,"seconds":1}"#,
        r#"{"probe":true,"hw":true}"#,
        r#"{"probe":true,"resident":true}"#,
    ] {
        let (status, text) = post(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
        assert_eq!(
            text, "a probe takes no file, seconds, hw or resident",
            "{body}"
        );
    }
    for body in [
        r#"{}"#,
        r#"{"file":"x.mp4"}"#,
        r#"{"seconds":1}"#,
        r#"{"probe":false}"#,
    ] {
        let (status, text) = post(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
        assert_eq!(
            text, "file and seconds are required (or \"probe\": true)",
            "{body}"
        );
    }
    assert!(state.decode_bench.try_start().is_some());
}

/// `probe` is an optional bool; anything else is axum's 422.
#[tokio::test]
async fn probe_must_be_a_bool() {
    let state = state().await;
    for probe in [r#""yes""#, "1", "null"] {
        let body = format!(r#"{{"probe":{probe}}}"#);
        let (status, text) = post(&state, &body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{probe}: {text}");
    }
}

/// One bench at a time: a held slot is 409 for the probe too.
#[tokio::test]
async fn a_probe_while_a_bench_runs_is_409() {
    let state = state().await;
    let slot = state.decode_bench.try_start().expect("the bench is free");
    let (status, text) = post(&state, r#"{"probe":true}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert_eq!(text, "a bench run is in progress");
    drop(slot);
}

#[cfg(not(windows))]
#[tokio::test]
async fn without_direct3d_the_probe_is_501_and_frees_the_bench() {
    let state = state().await;
    for _ in 0..2 {
        let (status, text) = post(&state, r#"{"probe":true}"#).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{text}");
        assert_eq!(text, "the GPU probe needs Windows Direct3D 11");
    }
    assert!(state.decode_bench.try_start().is_some());
}

/// On the Windows job the real probe runs: `windows-latest` has no GPU, so
/// `pick_adapter` finds none (500 naming it), or, on a runner with one, the
/// answer with every step. Its body goes to stderr, past libtest's capture.
#[cfg(windows)]
#[tokio::test]
async fn the_real_probe_answers_or_names_why_not() {
    use std::io::Write;
    let state = state().await;
    let (status, text) = post(&state, r#"{"probe":true}"#).await;
    writeln!(
        std::io::stderr().lock(),
        "#279 P0 decode-bench probe on the runner: {status} {text}"
    )
    .unwrap();
    match status {
        StatusCode::OK => {
            let answer: serde_json::Value = serde_json::from_str(&text).unwrap();
            for step in [
                "extended_resource_sharing",
                "nv12_shared_nthandle",
                "nv12_keyed_mutex_roundtrip",
                "nv12_srv_r8_r8g8",
                "uyvy_target_and_staging_do_not_wait",
            ] {
                assert!(answer["probe"][step]["ok"].is_boolean(), "{step}: {text}");
            }
            assert!(answer["verdict"]["kind"].is_string(), "{text}");
            assert!(answer["adapters"].is_array(), "{text}");
        }
        StatusCode::INTERNAL_SERVER_ERROR => {
            assert!(text.starts_with("the GPU probe failed: "), "{text}");
        }
        other => panic!("{other}: {text}"),
    }
    assert!(state.decode_bench.try_start().is_some());
}
