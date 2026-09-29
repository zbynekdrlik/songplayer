//! `POST /api/v1/ai/proxy/complete-login` through the real axum router: its
//! body is untrusted LAN input (#221, main-session decision 5882671183 item 1).
//! Shares `test_state`/`app` with `routes_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ai_tests.rs"] mod tests;`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

/// POST `body` (raw JSON text) to the complete-login route.
async fn complete_login(body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/ai/proxy/complete-login")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(test_state().await).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// The answer to a body without a usable `callback_url`.
fn required() -> Value {
    json!({ "ok": false, "error": "callback_url is required" })
}

/// serde_json's `raw_value` feature is on in this build (`rust-workspace.md`),
/// and with it `serde_json::Value`'s own `Deserialize` treats a map whose
/// first key is `$serde_json::private::RawValue` as a raw value: it re-parses
/// that key's STRING as JSON (with a fresh 128-level budget each time). The
/// L2b facade closed this class; this route's body must not re-open it. The
/// key is an ordinary, unknown (ignored) field of the typed body.
#[tokio::test]
async fn a_private_raw_value_key_is_an_ordinary_ignored_key() {
    // The hazard is live in this build: `Value` parses the key's string.
    let broken = r#"{"$serde_json::private::RawValue":"[1"}"#;
    assert!(serde_json::from_str::<Value>(broken).is_err());
    let smuggled = r#"{"$serde_json::private::RawValue":"{\"callback_url\":\"no-query\"}"}"#;
    assert_eq!(
        serde_json::from_str::<Value>(smuggled).unwrap(),
        json!({ "callback_url": "no-query" })
    );

    // The route never parses the string: no extraction error, and no
    // `callback_url` smuggled out of it.
    assert_eq!(complete_login(broken).await, (StatusCode::OK, required()));
    assert_eq!(complete_login(smuggled).await, (StatusCode::OK, required()));
}

#[tokio::test]
async fn a_missing_empty_or_null_callback_url_is_required() {
    for body in [
        "{}",
        r#"{"callback_url":""}"#,
        r#"{"callback_url":null}"#,
        r#"{"other":1}"#,
    ] {
        assert_eq!(
            complete_login(body).await,
            (StatusCode::OK, required()),
            "{body}"
        );
    }
}

/// A `callback_url` reaches the proxy (without a query it is refused there,
/// before any network call).
#[tokio::test]
async fn a_callback_url_is_forwarded_to_the_proxy() {
    let (status, json) = complete_login(r#"{"callback_url":"no-query"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], false);
    let error = json["error"].as_str().unwrap();
    assert!(error.contains("must contain query parameters"), "{error}");
}

/// `GET /api/v1/ai/status` names the Claude model SongPlayer sends (#145):
/// the post-deploy AI step reads it to make a real completion with exactly
/// that model and to compare it against the newest `claude-opus-*` the proxy
/// lists. The test state's client uses `AiSettings::default()`, i.e. the
/// default model.
#[tokio::test]
async fn ai_status_names_the_model_songplayer_sends() {
    let req = Request::builder()
        .uri("/api/v1/ai/status")
        .body(Body::empty())
        .unwrap();
    let resp = app(test_state().await).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["model"], json!(sp_core::config::DEFAULT_AI_MODEL));
    assert_eq!(json["apiUrl"], json!("http://127.0.0.1:18787/v1"));
}
