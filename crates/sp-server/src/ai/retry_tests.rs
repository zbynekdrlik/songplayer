//! #145: the AI client's retry policy against CLIProxyAPI's credential
//! cooldown (60 s by default; a refusal inside it is a 503 with
//! `Retry-After`). Pure: no request, no sleep.
//! Wired via `#[cfg(test)] #[path = "retry_tests.rs"] mod tests;`.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

use super::*;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn without_a_retry_after_the_retries_wait_5_20_60_s_then_stop() {
    let p = RetryPolicy::SPANNING;
    let waits: Vec<_> = (1..=4).map(|n| p.retry_delay(503, None, n)).collect();
    assert_eq!(waits, [Some(secs(5)), Some(secs(20)), Some(secs(60)), None]);
    let total: Duration = p.fallback.iter().sum();
    assert!(
        total > secs(60),
        "the waits outlast CLIProxyAPI's default 60 s cooldown: {total:?}"
    );
    assert_eq!(p.retry_delay(503, None, 0), None, "attempt 0 is no retry");
}

#[test]
fn a_retry_after_in_seconds_is_honoured_up_to_120_s() {
    let p = RetryPolicy::SPANNING;
    assert_eq!(p.retry_delay(503, Some("55"), 1), Some(secs(55)));
    assert_eq!(p.retry_delay(503, Some(" 7 "), 2), Some(secs(7)));
    assert_eq!(p.retry_delay(429, Some("120"), 3), Some(secs(120)));
    assert_eq!(
        p.retry_delay(429, Some("1800"), 1),
        Some(secs(120)),
        "capped"
    );
    assert_eq!(p.retry_delay(503, Some("0"), 1), Some(secs(0)));
    assert_eq!(
        p.retry_delay(503, Some("55"), 4),
        None,
        "the budget holds with a Retry-After too"
    );
}

#[test]
fn a_retry_after_that_is_no_number_of_seconds_falls_back() {
    let p = RetryPolicy::SPANNING;
    for value in ["Wed, 30 Sep 2026 10:00:00 GMT", "soon", "", "-5", "1.5"] {
        assert_eq!(
            p.retry_delay(502, Some(value), 1),
            Some(secs(5)),
            "{value:?}"
        );
    }
}

#[test]
fn only_429_and_5xx_are_retried() {
    let p = RetryPolicy::SPANNING;
    for status in [429, 500, 502, 503, 504, 599] {
        assert!(p.retry_delay(status, None, 1).is_some(), "{status}");
        assert!(is_retried(status), "{status}");
    }
    for status in [200, 400, 401, 404, 428, 430, 499, 600] {
        assert_eq!(p.retry_delay(status, Some("5"), 1), None, "{status}");
        assert!(!is_retried(status), "{status}");
    }
}

#[test]
fn a_response_s_retry_after_header_is_read() {
    let p = RetryPolicy::SPANNING;
    let mut headers = HeaderMap::new();
    assert_eq!(
        p.after_response(StatusCode::SERVICE_UNAVAILABLE, &headers, 1),
        Some(secs(5))
    );
    headers.insert(RETRY_AFTER, HeaderValue::from_static("42"));
    assert_eq!(
        p.after_response(StatusCode::SERVICE_UNAVAILABLE, &headers, 2),
        Some(secs(42))
    );
    assert_eq!(p.after_response(StatusCode::BAD_REQUEST, &headers, 1), None);
}

#[test]
fn the_test_policy_keeps_the_budget_and_never_waits() {
    let p = RetryPolicy::NO_WAIT;
    assert_eq!(p.fallback.len(), RetryPolicy::SPANNING.fallback.len());
    assert_eq!(p.retry_delay(503, Some("60"), 1), Some(Duration::ZERO));
    assert_eq!(p.retry_delay(503, None, 3), Some(Duration::ZERO));
    assert_eq!(p.retry_delay(503, None, 4), None);
}

#[test]
fn a_refused_body_is_logged_by_its_first_300_characters() {
    assert_eq!(body_excerpt("no auth available"), "no auth available");
    let long = "é".repeat(500);
    let excerpt = body_excerpt(&long);
    assert_eq!(excerpt.chars().count(), BODY_EXCERPT_CHARS);
    assert_eq!(BODY_EXCERPT_CHARS, 300);
    assert_eq!(body_excerpt(""), "");
}
