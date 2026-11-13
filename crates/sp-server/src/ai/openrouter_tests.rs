use std::time::Duration;

use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Ask, Completion, OpenRouterClient, read_answer, redacted_excerpt};
use crate::ai::retry::RetryPolicy;

const KEY: &str = "example-openrouter-key-for-tests";

fn ask<'a>(system: &'a str, user: &'a str) -> Ask<'a> {
    Ask {
        key: KEY,
        model: "google/gemini-4-hydrogen",
        system,
        user,
        max_tokens: 64,
        timeout: Duration::from_secs(10),
    }
}

fn client(server: &MockServer) -> OpenRouterClient {
    OpenRouterClient::new(format!("{}/api/v1", server.uri()))
        .with_retry_policy(RetryPolicy::NO_WAIT)
}

fn answer(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "gen-1",
        "model": "google/gemini-4-hydrogen-20261112",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}}]
    })
}

/// The request is the proxy client's shape with the key as a Bearer token:
/// the system message first, then the user's, the model and the bounds.
#[tokio::test]
async fn a_completion_sends_the_tasks_prompt_with_the_key_and_reads_the_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_json(serde_json::json!({
            "model": "google/gemini-4-hydrogen",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"}
            ],
            "temperature": 0.1,
            "max_tokens": 64
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("ahoj")))
        .expect(1)
        .mount(&server)
        .await;
    let got = client(&server)
        .complete(&ask("sys", "hello"))
        .await
        .unwrap();
    assert_eq!(
        got,
        Completion {
            text: "ahoj".into(),
            model: Some("google/gemini-4-hydrogen-20261112".into())
        }
    );
}

/// No system prompt (the translator, the register check) = the user's
/// message alone.
#[tokio::test]
async fn an_empty_system_prompt_sends_the_users_message_alone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .and(body_json(serde_json::json!({
            "model": "google/gemini-4-hydrogen",
            "messages": [{"role": "user", "content": "only"}],
            "temperature": 0.1,
            "max_tokens": 64
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("x")))
        .expect(1)
        .mount(&server)
        .await;
    let got = client(&server).complete(&ask("", "only")).await.unwrap();
    assert_eq!(got.text, "x");
}

/// A 503 then a 200: retried, the second answer read.
#[tokio::test]
async fn a_refusal_the_policy_retries_is_asked_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("again")))
        .mount(&server)
        .await;
    let got = client(&server).complete(&ask("", "q")).await.unwrap();
    assert_eq!(got.text, "again");
}

/// A 429 every time: the first request and the policy's 3 retries, then an
/// error naming the status and the body.
#[tokio::test]
async fn a_call_refused_every_time_gives_up_after_its_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .expect(4)
        .mount(&server)
        .await;
    let err = client(&server)
        .complete(&ask("", "q"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 429"), "{err}");
    assert!(err.contains("rate limited"), "{err}");
}

/// A 401 is final (no retry), and the key an error body echoes is cut out.
#[tokio::test]
async fn a_refused_key_fails_at_once_and_never_names_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(format!("{{\"error\":{{\"message\":\"bad key {KEY}\"}}}}")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server)
        .complete(&ask("", "q"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 401"), "{err}");
    assert!(err.contains("bad key [REDACTED]"), "{err}");
    assert!(!err.contains(KEY), "{err}");
}

/// OpenRouter's moderation refusal is typed as a content-filter refusal
/// (final, never retried), the key cut out; any other 403 is not.
#[tokio::test]
async fn a_moderation_refusal_is_a_content_filter_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string(format!(
            "{{\"error\":{{\"code\":403,\"message\":\"flagged {KEY}\",\"metadata\":{{\"reasons\":[\"x\"],\"flagged_input\":\"q\"}}}}}}"
        )))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server).complete(&ask("", "q")).await.unwrap_err();
    assert!(crate::ai::retry::content_filtered(&err));
    let text = format!("{err:#}");
    assert!(!text.contains(KEY));
    assert!(text.contains("HTTP 403"), "{text}");

    assert!(super::is_moderation_refusal(
        403,
        r#"{"error":{"message":"Moderation"}}"#
    ));
    assert!(super::is_moderation_refusal(
        403,
        r#"{"metadata":{"flagged_input":"q"}}"#
    ));
    assert!(!super::is_moderation_refusal(
        403,
        r#"{"error":{"message":"forbidden"}}"#
    ));
    assert!(!super::is_moderation_refusal(
        400,
        r#"{"metadata":{"reasons":[]}}"#
    ));
}

/// A 200 whose choice has no text is an error, never an empty translation.
#[tokio::test]
async fn an_answer_with_no_text_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": null}}]
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .complete(&ask("", "q"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no text"), "{err}");
}

#[test]
fn an_answer_is_read_into_typed_fields_only() {
    // A raw-value key at the top is an unknown field, skipped unparsed.
    let body =
        r#"{"$serde_json::private::RawValue":"[[[[1]]]]","choices":[{"message":{"content":"t"}}]}"#;
    assert_eq!(
        read_answer(body).unwrap(),
        Completion {
            text: "t".into(),
            model: None
        }
    );
    let err = read_answer("<html>").unwrap_err().to_string();
    assert!(err.contains("line 1, column 1"), "{err}");
    assert!(read_answer(r#"{"choices":[]}"#).is_err());
    assert!(read_answer(r#"{"choices":[{"message":{"content":"  "}}]}"#).is_err());
}

#[test]
fn the_excerpt_cuts_the_key_before_the_cut_and_keeps_one_line() {
    let long = format!("{}{KEY}", "x".repeat(290));
    let excerpt = redacted_excerpt(&long, KEY);
    assert!(!excerpt.contains("exam"), "{excerpt}");
    assert_eq!(excerpt.chars().count(), 300);
    assert_eq!(redacted_excerpt("a\nb", ""), "a b");
    assert_eq!(redacted_excerpt("k=abc", "abc"), "k=[REDACTED]");
}
