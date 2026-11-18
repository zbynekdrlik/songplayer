//! #321: the OpenRouter probe, the A/B route's refusals and the task model
//! settings' PATCH, through the real router.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::AppState;
use crate::ai::client::AiClient;
use crate::ai::openrouter::OpenRouterClient;
use crate::ai::retry::RetryPolicy;
use crate::api::routes::tests::{app, test_state};
use crate::db::models::{get_setting, set_setting};

const KEY: &str = "example-openrouter-key-for-tests";

/// A state whose AI client sends to a mock OpenRouter answering `ok`.
async fn routed_state() -> (AppState, MockServer) {
    let mut state = test_state().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "google/gemini-4-hydrogen-20261112",
            "choices": [{"message": {"content": " ok "}}]
        })))
        .mount(&server)
        .await;
    state.ai_client = Arc::new(
        AiClient::with_openrouter(
            state.pool.clone(),
            OpenRouterClient::new(format!("{}/api/v1", server.uri())),
        )
        .with_retry_policy(RetryPolicy::NO_WAIT),
    );
    (state, server)
}

async fn send(state: AppState, m: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(m)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn probe(state: AppState, body: &str) -> Value {
    let (status, text) = send(state, "POST", "/api/v1/ai/openrouter/probe", body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn the_probe_answers_with_the_default_model() {
    let (state, server) = routed_state().await;
    set_setting(&state.pool, "openrouter_api_key", KEY)
        .await
        .unwrap();
    let r = probe(state, "{}").await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["model"], "google/gemini-4-hydrogen");
    assert_eq!(r["answered_model"], "google/gemini-4-hydrogen-20261112");
    assert_eq!(r["sample"], "ok");
    assert_eq!(r["error"], Value::Null);
    let sent = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "google/gemini-4-hydrogen");
    assert_eq!(body["max_tokens"], 1000);
}

/// The body is optional: a bare POST (no body, no content type) probes
/// the default model and answers 200, as the route's doc says.
#[tokio::test]
async fn a_bare_post_probes_the_default_model() {
    let (state, server) = routed_state().await;
    set_setting(&state.pool, "openrouter_api_key", KEY)
        .await
        .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/ai/openrouter/probe")
        .body(Body::empty())
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "google/gemini-4-hydrogen");
}

#[tokio::test]
async fn the_probe_sends_the_model_asked_for() {
    let (state, server) = routed_state().await;
    set_setting(&state.pool, "openrouter_api_key", KEY)
        .await
        .unwrap();
    let r = probe(state, r#"{"model":"google/gemini-4-carbon"}"#).await;
    assert_eq!(
        (r["ok"].clone(), r["model"].clone()),
        (json!(true), json!("google/gemini-4-carbon"))
    );
    let sent = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "google/gemini-4-carbon");
}

#[tokio::test]
async fn the_probe_sends_nothing_while_paid_ai_is_off_or_with_no_key() {
    let (state, server) = routed_state().await;
    let r = probe(state.clone(), "{}").await;
    assert_eq!(r["ok"], false);
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("openrouter_api_key is not set"),
        "{r}"
    );
    set_setting(&state.pool, "openrouter_api_key", KEY)
        .await
        .unwrap();
    set_setting(&state.pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    let r = probe(state, "{}").await;
    assert_eq!(r["ok"], false);
    assert!(
        r["error"].as_str().unwrap().contains("paid AI is off"),
        "{r}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_ab_route_refuses_a_bad_body_and_paid_ai_off() {
    let state = test_state().await;
    let (status, text) = send(
        state.clone(),
        "POST",
        "/api/v1/ai/translate-ab",
        r#"{"youtube_ids":["nope"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("not a YouTube id"), "{text}");
    set_setting(&state.pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    let (status, text) = send(
        state,
        "POST",
        "/api/v1/ai/translate-ab",
        r#"{"youtube_ids":["abcdefghijk"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("paid AI is off"), "{text}");
}

#[tokio::test]
async fn the_ab_status_route_answers_a_state() {
    let (status, text) = send(test_state().await, "GET", "/api/v1/ai/translate-ab", "").await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v["state"].is_string(), "{v}");
}

/// A task's model is stored trimmed through the settings PATCH.
#[tokio::test]
async fn a_tasks_model_is_stored_trimmed() {
    let state = test_state().await;
    let (status, _) = send(
        state.clone(),
        "PATCH",
        "/api/v1/settings",
        r#"{"ai_model_translate":" google/gemini-4-carbon "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        get_setting(&state.pool, "ai_model_translate")
            .await
            .unwrap()
            .as_deref(),
        Some("google/gemini-4-carbon")
    );
}
