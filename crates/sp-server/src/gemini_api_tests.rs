//! #136: the shared Gemini key-list contract.

use super::*;

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

#[test]
fn the_same_key_retry_schedule_is_2_4_8_16_seconds() {
    let secs: Vec<u64> = RETRY_BACKOFFS.iter().map(|d| d.as_secs()).collect();
    assert_eq!(secs, [2, 4, 8, 16]);
}
