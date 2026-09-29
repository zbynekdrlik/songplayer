//! #136: the Gemini provider on the `gemini_api_key` key LIST, against a mock
//! `generateContent` server (`GeminiProvider::with_api_root`).
//!
//! Before #136 the provider sent the whole comma-separated setting as ONE
//! `x-goog-api-key`; Google refused every call, and every video fell back to
//! the title parser. These tests pin one key per attempt, the rotation rules,
//! and an error that never carries a key.

use super::*;
use crate::metadata::test_support::{
    ARTIST, SONG, TITLE, VIDEO, gemini_answer, key_invalid_body, received_keys,
};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gemini-test";
const ENDPOINT: &str = "/v1beta/models/gemini-test:generateContent";

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

fn provider_at(server: &MockServer, list: &[&str]) -> GeminiProvider {
    GeminiProvider::with_api_root(keys(list), MODEL.into(), &server.uri())
}

/// `key` gets `status` + `body` for every request.
async fn key_answers(server: &MockServer, key: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .and(header("x-goog-api-key", key))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn key_answers_text(server: &MockServer, key: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .and(header("x-goog-api-key", key))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

// ---- one key per attempt, rotation --------------------------------------

#[tokio::test]
async fn every_request_carries_one_key_of_the_list() {
    let server = MockServer::start().await;
    key_answers(&server, "k1", 200, gemini_answer(SONG, ARTIST)).await;

    let meta = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect("the first key of the list answers");

    assert_eq!(meta.song, SONG);
    assert_eq!(meta.artist, ARTIST);
    assert!(!meta.gemini_failed);
    assert_eq!(
        received_keys(&server).await,
        ["k1", "k1"],
        "the search AND the clean-up pass each send ONE key, never the list"
    );
}

#[tokio::test]
async fn a_refused_key_moves_to_the_next_and_the_clean_pass_uses_the_key_that_answered() {
    let server = MockServer::start().await;
    key_answers(&server, "k1", 400, key_invalid_body()).await;
    key_answers(&server, "k2", 200, gemini_answer(SONG, ARTIST)).await;

    let meta = provider_at(&server, &["k1", "k2", "k3"])
        .extract(VIDEO, TITLE)
        .await
        .expect("k2 answers after k1 is refused");

    assert_eq!(meta.song, SONG);
    assert_eq!(received_keys(&server).await, ["k1", "k2", "k2"]);
}

#[tokio::test]
async fn a_forbidden_key_moves_to_the_next() {
    let server = MockServer::start().await;
    key_answers_text(&server, "k1", 403, "PERMISSION_DENIED: project suspended").await;
    key_answers(&server, "k2", 200, gemini_answer(SONG, ARTIST)).await;

    let meta = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect("k2 answers after k1 is forbidden");

    assert_eq!(meta.artist, ARTIST);
    assert_eq!(received_keys(&server).await, ["k1", "k2", "k2"]);
}

#[tokio::test]
async fn a_rate_limited_key_moves_to_the_next() {
    let server = MockServer::start().await;
    key_answers_text(&server, "k1", 429, "RESOURCE_EXHAUSTED: quota").await;
    key_answers(&server, "k2", 200, gemini_answer(SONG, ARTIST)).await;

    let meta = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect("k2 answers after k1 is rate-limited");

    assert_eq!(meta.song, SONG);
    assert_eq!(received_keys(&server).await, ["k1", "k2", "k2"]);
}

#[tokio::test]
async fn a_status_that_is_not_about_the_key_stops_at_the_first_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404).set_body_string("models/gemini-test is not found"))
        .mount(&server)
        .await;

    let err = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("an unknown model fails on every key");

    let text = err.to_string();
    assert!(matches!(err, MetadataError::ApiError(_)), "{text}");
    assert!(text.contains("key 1 of 2: HTTP 404"), "{text}");
    assert!(text.contains("is not found"), "{text}");
    assert_eq!(received_keys(&server).await, ["k1"], "no second key tried");
}

#[tokio::test]
async fn every_key_refused_names_the_last_key_status_and_body_never_a_key() {
    let server = MockServer::start().await;
    key_answers_text(
        &server,
        "fake-key-one",
        400,
        "API_KEY_INVALID for fake-key-one",
    )
    .await;
    key_answers_text(
        &server,
        "fake-key-two",
        403,
        "PERMISSION_DENIED fake-key-two blocked",
    )
    .await;

    let err = provider_at(&server, &["fake-key-one", "fake-key-two"])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("both keys refused");

    let text = err.to_string();
    assert!(matches!(err, MetadataError::ApiError(_)), "{text}");
    assert!(text.contains("all 2 keys failed"), "{text}");
    assert!(text.contains("key 2 of 2: HTTP 403"), "{text}");
    assert!(text.contains("PERMISSION_DENIED <key> blocked"), "{text}");
    assert!(
        !text.contains("fake-key"),
        "an error must never carry a key: {text}"
    );
}

#[tokio::test]
async fn every_key_rate_limited_is_a_rate_limit_naming_the_last_status() {
    let server = MockServer::start().await;
    key_answers_text(&server, "k1", 429, "RESOURCE_EXHAUSTED one").await;
    key_answers_text(&server, "k2", 429, "RESOURCE_EXHAUSTED two").await;

    let err = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("both keys rate-limited");

    let detail = match err {
        MetadataError::RateLimited(detail) => detail,
        other => panic!("every key 429 must be RateLimited (the reprocess cooldown): {other}"),
    };
    assert!(
        detail.contains("key 2 of 2: HTTP 429: RESOURCE_EXHAUSTED two"),
        "{detail}"
    );
}

#[tokio::test]
async fn a_rate_limit_then_a_refusal_is_still_a_rate_limit() {
    let server = MockServer::start().await;
    key_answers_text(&server, "k1", 429, "RESOURCE_EXHAUSTED").await;
    key_answers(&server, "k2", 400, key_invalid_body()).await;

    let err = provider_at(&server, &["k1", "k2"])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("no key answered");

    let detail = match err {
        MetadataError::RateLimited(detail) => detail,
        other => panic!("a rate-limited key makes the whole failure a rate limit: {other}"),
    };
    assert!(detail.contains("key 2 of 2: HTTP 400"), "{detail}");
}

#[tokio::test]
async fn an_empty_key_list_fails_without_a_request() {
    let server = MockServer::start().await;

    let err = provider_at(&server, &[])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("no key, no call");

    assert!(err.to_string().contains("gemini_api_key is empty"), "{err}");
    assert!(received_keys(&server).await.is_empty());
}

#[tokio::test]
async fn a_transport_failure_stops_and_names_no_key() {
    // Nothing listens on port 1: the connect fails.
    let provider = GeminiProvider::with_api_root(
        keys(&["fake-key-one", "fake-key-two"]),
        MODEL.into(),
        "http://127.0.0.1:1/",
    );

    let err = provider.extract(VIDEO, TITLE).await.expect_err("no server");

    let text = err.to_string();
    assert!(text.contains("key 1 of 2: request failed"), "{text}");
    assert!(!text.contains("fake-key"), "{text}");
}

#[tokio::test]
async fn an_answer_without_text_is_an_invalid_response() {
    let server = MockServer::start().await;
    key_answers(&server, "k1", 200, json!({"candidates": []})).await;

    let err = provider_at(&server, &["k1"])
        .extract(VIDEO, TITLE)
        .await
        .expect_err("no candidate text");

    assert!(matches!(err, MetadataError::InvalidResponse(_)), "{err}");
}

// ---- the clean-up pass ---------------------------------------------------

/// The search pass answers `first`; the clean-up pass answers `clean`.
async fn two_pass_server(first: serde_json::Value, clean: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("google_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("Clean them for LED wall display"))
        .respond_with(clean)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn the_clean_pass_answer_is_what_ships() {
    let first = gemini_answer(
        "Stand On Your Promise (Live)",
        "The Emerging Sound x Maddie Fong",
    );
    let clean = ResponseTemplate::new(200).set_body_json(gemini_answer(SONG, ARTIST));
    let server = two_pass_server(first, clean).await;

    let meta = provider_at(&server, &["k1"])
        .extract(VIDEO, TITLE)
        .await
        .unwrap();

    assert_eq!((meta.song.as_str(), meta.artist.as_str()), (SONG, ARTIST));
}

#[tokio::test]
async fn a_failed_or_partial_clean_pass_keeps_the_first_answer_where_it_has_nothing() {
    let first_song = "Stand On Your Promise (Live)";
    let cases = [
        // (clean-up answer, expected song, expected artist)
        (ResponseTemplate::new(429), first_song, ARTIST),
        (ResponseTemplate::new(500), first_song, ARTIST),
        (
            ResponseTemplate::new(200).set_body_json(json!({"candidates": []})),
            first_song,
            ARTIST,
        ),
        (
            ResponseTemplate::new(200).set_body_json(gemini_answer("", "Emerging")),
            first_song,
            "Emerging",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"candidates": [{"content":
                {"parts": [{"text": "{\"song\": \"Stand On Your Promise\"}"}]}}]})),
            SONG,
            ARTIST,
        ),
    ];
    for (clean, want_song, want_artist) in cases {
        let server = two_pass_server(gemini_answer(first_song, ARTIST), clean).await;

        let meta = provider_at(&server, &["k1"])
            .extract(VIDEO, TITLE)
            .await
            .unwrap();

        assert_eq!(
            (meta.song.as_str(), meta.artist.as_str()),
            (want_song, want_artist)
        );
    }
}

// ---- pure helpers --------------------------------------------------------

#[test]
fn only_a_rate_limit_or_a_key_refusal_moves_to_the_next_key() {
    assert_eq!(classify(429, ""), KeyOutcome::RateLimited);
    assert_eq!(classify(403, ""), KeyOutcome::Refused);
    assert_eq!(
        classify(400, "reason: API_KEY_INVALID"),
        KeyOutcome::Refused
    );
    assert_eq!(
        classify(400, "API key expired. Please renew."),
        KeyOutcome::Refused
    );
    assert_eq!(
        classify(400, "Invalid JSON payload received."),
        KeyOutcome::Stop
    );
    assert_eq!(classify(401, "API key"), KeyOutcome::Stop);
    assert_eq!(classify(404, "models/x is not found"), KeyOutcome::Stop);
    assert_eq!(classify(500, "API_KEY"), KeyOutcome::Stop);
}

#[test]
fn an_excerpt_is_one_line_redacted_and_cut_after_the_redaction() {
    let p = GeminiProvider::new(keys(&["secret-one", "secret-two"]), MODEL.into());
    assert_eq!(
        p.excerpt("x secret-one y\n\n  secret-two z"),
        "x <key> y <key> z"
    );
    let long = "a".repeat(500);
    assert_eq!(p.excerpt(&long).chars().count(), BODY_EXCERPT_CHARS);
    // A key straddling the cut is redacted first, so no prefix survives.
    let edge = format!("{}secret-one", "a".repeat(195));
    assert_eq!(p.excerpt(&edge), format!("{}<key>", "a".repeat(195)));
}

#[test]
fn an_empty_key_never_redacts_between_characters() {
    let p = GeminiProvider::new(keys(&["", "k9"]), MODEL.into());
    assert_eq!(p.excerpt("abc k9"), "abc <key>");
}

#[test]
fn the_answer_text_joins_every_non_thought_part() {
    let answer = |parts: serde_json::Value| json!({"candidates": [{"content": {"parts": parts}}]});
    assert_eq!(
        response_text(&answer(
            json!([{"text": "{\"song\": "}, {"text": "\"A\"}"}])
        ))
        .as_deref(),
        Some("{\"song\": \"A\"}")
    );
    assert_eq!(
        response_text(&answer(json!([
            {"text": "thinking about it", "thought": true},
            {"text": "{}", "thought": false}
        ])))
        .as_deref(),
        Some("{}")
    );
    assert_eq!(response_text(&answer(json!([{"text": "  "}]))), None);
    assert_eq!(response_text(&answer(json!([]))), None);
    assert_eq!(response_text(&json!({"candidates": []})), None);
}
