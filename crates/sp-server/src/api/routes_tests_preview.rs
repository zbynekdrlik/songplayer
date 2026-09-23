//! `GET /api/v1/playback/{id}/preview.jpg` route tests (#15 part 2).
//! Included via `#[path] #[cfg(test)] mod tests;` from `api/preview.rs`;
//! reuses the shared `app`/`test_state` harness from `routes_tests.rs`.

use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct PreviewResponse {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn get_preview(state: AppState, playlist_id: i64) -> PreviewResponse {
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/playback/{playlist_id}/preview.jpg"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    PreviewResponse {
        status,
        content_type,
        body,
    }
}

#[tokio::test]
async fn preview_unknown_playlist_returns_404() {
    let state = test_state().await;
    let r = get_preview(state, 4242).await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "no pipeline for this playlist → 404"
    );
}

#[tokio::test]
async fn preview_registered_but_idle_returns_204() {
    let state = test_state().await;
    // A registered pipeline that has produced no preview frame yet.
    state.preview_registry.register(1);
    let r = get_preview(state, 1).await;
    assert_eq!(
        r.status,
        StatusCode::NO_CONTENT,
        "registered but no frame yet → 204"
    );
    assert!(r.body.is_empty(), "204 carries no body");
}

#[tokio::test]
async fn preview_returns_jpeg_when_a_frame_is_available() {
    let state = test_state().await;
    let tap = state.preview_registry.register(2);
    // Seed a published JPEG (a real minimal JPEG byte sequence).
    let seeded = vec![0xFF, 0xD8, 0xFF, 0xD9];
    tap.set_latest_jpeg_for_test(seeded.clone());

    let r = get_preview(state, 2).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.content_type.as_deref(),
        Some("image/jpeg"),
        "preview must be served as image/jpeg"
    );
    assert_eq!(
        r.body, seeded,
        "the exact published JPEG bytes are returned"
    );
}

// ── #178 item 16: WS idle deadline ───────────────────────────────────────────

#[test]
fn ws_viewer_is_idle_only_after_15s_of_silence() {
    use super::is_idle;
    // Exactly 15 s of silence is not yet idle; past 15 s is.
    assert!(!is_idle(0, 15_000), "exactly 15 s is not yet idle");
    assert!(is_idle(0, 15_001), "past 15 s of silence is idle");
    // 10 s of silence is fine.
    assert!(!is_idle(10_000, 20_000), "10 s of silence is not idle");
    // 15.002 s of silence is idle.
    assert!(is_idle(1_000, 16_002), "15.002 s of silence is idle");
}

// ── #184 round F: preview lag beacon frame ───────────────────────────────────

#[test]
fn beacon_frame_is_exact_produced_ms_json() {
    use super::beacon_frame;
    // The shim parses this exact shape; the key MUST be `produced_ms` and the
    // value the raw ms. Exact strings kill any body-replacement / wrong-key mutant.
    assert_eq!(beacon_frame(0), r#"{"produced_ms":0}"#);
    assert_eq!(beacon_frame(500), r#"{"produced_ms":500}"#);
    assert_eq!(beacon_frame(33_900), r#"{"produced_ms":33900}"#);
}

// ── #184 round G: application-level ping/pong (transport round trip) ─────────

#[test]
fn pong_frame_echoes_an_integer_ping_verbatim() {
    use super::pong_frame;
    // The shim computes rtt = performance.now() − pong, so the echoed number
    // MUST be exactly the one it sent, under the key `pong`.
    assert_eq!(
        pong_frame(r#"{"ping":12345}"#).as_deref(),
        Some(r#"{"pong":12345}"#)
    );
    assert_eq!(
        pong_frame(r#"{"ping":0}"#).as_deref(),
        Some(r#"{"pong":0}"#)
    );
}

#[test]
fn pong_frame_echoes_a_float_ping_verbatim() {
    use super::pong_frame;
    // performance.now() is a fractional ms value — it must survive the echo
    // exactly (no truncation to an integer, no rounding).
    assert_eq!(
        pong_frame(r#"{"ping":1234.5678}"#).as_deref(),
        Some(r#"{"pong":1234.5678}"#)
    );
    assert_eq!(
        pong_frame(r#"{"ping":98765.60000000009}"#).as_deref(),
        Some(r#"{"pong":98765.60000000009}"#)
    );
}

#[test]
fn pong_frame_ignores_extra_fields() {
    use super::pong_frame;
    assert_eq!(
        pong_frame(r#"{"ping":7,"note":"x","n":2}"#).as_deref(),
        Some(r#"{"pong":7}"#)
    );
}

#[test]
fn pong_frame_rejects_everything_that_is_not_a_numeric_ping() {
    use super::pong_frame;
    for text in [
        "",
        "garbage",
        "ping",
        "7",
        "[1,2]",
        "null",
        "{}",
        r#"{"ping":"7"}"#,
        r#"{"ping":null}"#,
        r#"{"ping":true}"#,
        r#"{"ping":[1]}"#,
        r#"{"pong":7}"#,
        r#"{"produced_ms":500}"#,
        r#"{"ping":7"#,
    ] {
        assert_eq!(pong_frame(text), None, "must not answer {text:?}");
    }
}
