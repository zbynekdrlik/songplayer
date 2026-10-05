//! #136: the shared Gemini key-list contract.

use super::*;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn gemini_keys_from_setting_splits_trims_and_drops_empties() {
    assert_eq!(
        gemini_keys_from_setting(" key1 , key2,, key3 "),
        vec!["key1".to_string(), "key2".to_string(), "key3".to_string()]
    );
    assert_eq!(gemini_keys_from_setting(""), Vec::<String>::new());
    assert_eq!(
        gemini_keys_from_setting("onlyone"),
        vec!["onlyone".to_string()]
    );
}

#[test]
fn a_rate_limit_or_a_key_refusal_moves_to_the_next_key() {
    assert_eq!(
        key_verdict(429, ""),
        KeyVerdict::NextKey { rate_limited: true }
    );
    let refused = KeyVerdict::NextKey {
        rate_limited: false,
    };
    assert_eq!(key_verdict(403, ""), refused);
    assert_eq!(key_verdict(400, "reason: API_KEY_INVALID"), refused);
    assert_eq!(key_verdict(400, "API key expired. Please renew."), refused);
}

#[test]
fn a_server_error_is_retried_on_the_same_key() {
    assert_eq!(key_verdict(500, ""), KeyVerdict::RetrySameKey);
    assert_eq!(
        key_verdict(503, "model overloaded"),
        KeyVerdict::RetrySameKey
    );
    assert_eq!(key_verdict(599, "API_KEY"), KeyVerdict::RetrySameKey);
}

#[test]
fn anything_else_stops() {
    assert_eq!(
        key_verdict(400, "Invalid JSON payload received."),
        KeyVerdict::Stop
    );
    assert_eq!(key_verdict(401, "API key"), KeyVerdict::Stop);
    assert_eq!(key_verdict(404, "models/x is not found"), KeyVerdict::Stop);
    assert_eq!(key_verdict(600, ""), KeyVerdict::Stop);
}

// ---- redact_keys: one redaction for every caller (metadata, g35t, probes) ----

#[test]
fn every_key_is_redacted_and_a_key_containing_another_is_replaced_whole() {
    let keys = ["abc", "abcdef"];
    assert_eq!(
        redact_keys("x abcdef y abc z", &keys),
        "x <key> y <key> z",
        "the longest key first, so no `def` tail survives"
    );
    // The order of the list does not matter.
    assert_eq!(
        redact_keys("x abcdef y abc z", &["abcdef", "abc"]),
        "x <key> y <key> z"
    );
}

#[test]
fn an_empty_key_never_redacts_between_characters() {
    assert_eq!(redact_keys("abc k9", &["", "k9"]), "abc <key>");
    assert_eq!(redact_keys("abc", &Vec::<String>::new()), "abc");
}

#[test]
fn a_body_excerpt_is_one_line_then_redacted_then_cut() {
    // Google answers errors pretty-printed: one line, so the cut keeps text.
    assert_eq!(
        body_excerpt(
            "{\n  \"error\": {\n    \"code\": 400\n  }\n}\n",
            &["k9"],
            100
        ),
        "{ \"error\": { \"code\": 400 } }"
    );
    assert_eq!(
        body_excerpt(&"a".repeat(500), &["k9"], 200),
        "a".repeat(200)
    );
    // A key straddling the cut is redacted first, so no prefix survives.
    let edge = format!("{}secret-one", "a".repeat(195));
    assert_eq!(
        body_excerpt(&edge, &["secret-one"], 200),
        format!("{}<key>", "a".repeat(195))
    );
}

#[test]
fn the_same_key_retry_schedule_is_2_4_8_16_seconds() {
    let secs: Vec<u64> = RETRY_BACKOFFS.iter().map(|d| d.as_secs()).collect();
    assert_eq!(secs, [2, 4, 8, 16]);
}

// ---- send_on_key: the one send + same-key retry loop (lyrics AND metadata) ----

const QUICK: [Duration; 2] = [Duration::from_millis(1), Duration::from_millis(1)];

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

#[tokio::test]
async fn a_5xx_is_sent_again_on_the_same_request_until_it_answers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("model overloaded"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();

    let reply = send_on_key("test", &QUICK, || client.post(server.uri()))
        .await
        .unwrap();

    assert!(matches!(reply, KeyReply::Answered(_)), "{reply:?}");
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn a_5xx_after_every_pause_is_refused_with_its_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("model overloaded"))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();

    let reply = send_on_key("test", &QUICK, || client.post(server.uri()))
        .await
        .unwrap();

    let KeyReply::Refused {
        verdict,
        status,
        body,
    } = reply
    else {
        panic!("a 5xx after every pause is refused");
    };
    assert_eq!(
        (verdict, status, body.as_str()),
        (KeyVerdict::RetrySameKey, 503, "model overloaded")
    );
    assert_eq!(requests(&server).await, 3, "one try + one per pause");
}

#[tokio::test]
async fn a_rate_limit_is_refused_at_once_for_the_next_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("RESOURCE_EXHAUSTED"))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();

    let reply = send_on_key("test", &QUICK, || client.post(server.uri()))
        .await
        .unwrap();

    let KeyReply::Refused { verdict, .. } = reply else {
        panic!("a 429 is refused");
    };
    assert_eq!(verdict, KeyVerdict::NextKey { rate_limited: true });
    assert_eq!(
        requests(&server).await,
        1,
        "a 429 is never retried on the same key"
    );
}

#[tokio::test]
async fn a_transport_failure_is_returned_as_is() {
    let client = reqwest::Client::new();

    let reply = send_on_key("test", &QUICK, || client.post("http://127.0.0.1:1/")).await;

    assert!(reply.is_err(), "nothing listens on port 1");
}
