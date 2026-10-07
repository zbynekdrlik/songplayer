//! #233: `audio_outputs` / `audio_network_rate` through `PATCH /api/v1/settings`
//! (the real router): a good list is stored normalized, a bad one refuses the
//! whole PATCH (400, the reason names the entry and the field, nothing
//! written), the network rate is checked.

use axum::http::StatusCode;
use sp_core::config::{SETTING_AUDIO_NETWORK_RATE, SETTING_AUDIO_OUTPUTS, SETTING_GEMINI_MODEL};

use super::tests::{body, patch, stored};
use crate::api::routes::tests::test_state;

const ONE: &str = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":6980}}]"#;

#[tokio::test]
async fn a_valid_list_is_stored_normalized_with_its_defaults() {
    let state = test_state().await;
    let (status, _) = patch(&state, &body(&[(SETTING_AUDIO_OUTPUTS, ONE)])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_OUTPUTS).await.unwrap(),
        r#"[{"id":"out-1","name":"a","type":"vban","enabled":true,"rate":"network","delay_ms":0,"vban":{"host":"h","port":6980,"stream_name":"sp-program","format":"int24"}}]"#
    );
}

#[tokio::test]
async fn a_bad_list_refuses_the_whole_patch_and_writes_nothing() {
    let state = test_state().await;
    let bad = r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":0}}]"#;
    let (status, text) = patch(
        &state,
        &body(&[
            (SETTING_AUDIO_OUTPUTS, bad),
            (SETTING_GEMINI_MODEL, "model-x"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text, "entry 1 (id out-1): vban.port must be 1-65535");
    assert_eq!(stored(&state.pool, SETTING_AUDIO_OUTPUTS).await, None);
    assert_eq!(
        stored(&state.pool, SETTING_GEMINI_MODEL).await,
        None,
        "nothing written"
    );
}

#[tokio::test]
async fn a_type_error_names_the_field_never_the_value() {
    let state = test_state().await;
    let bad =
        r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"h","port":"secret-ish-text"}}]"#;
    let (status, text) = patch(&state, &body(&[(SETTING_AUDIO_OUTPUTS, bad)])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!text.contains("secret-ish"), "no echo of the input");
    assert_eq!(text, "entry 1 (id out-1): vban.port has the wrong type");
}

#[tokio::test]
async fn the_network_rate_is_checked() {
    let state = test_state().await;
    let (status, _) = patch(&state, &body(&[(SETTING_AUDIO_NETWORK_RATE, "96000")])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_NETWORK_RATE)
            .await
            .unwrap(),
        "96000"
    );
    let (status, text) = patch(&state, &body(&[(SETTING_AUDIO_NETWORK_RATE, "32000")])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text.starts_with("audio_network_rate must be one of"));
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_NETWORK_RATE)
            .await
            .unwrap(),
        "96000",
        "the refused value wrote nothing"
    );
}
