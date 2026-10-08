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

/// #233 lane 3: an ASIO entry through the real router — stored normalized;
/// a second entry on the same driver refuses the whole PATCH.
#[tokio::test]
async fn an_asio_entry_is_stored_and_one_driver_twice_is_refused() {
    let state = test_state().await;
    let dvs = r#"{"id":"out-2","name":"DVS","type":"asio","asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}"#;
    let one = format!("[{dvs}]");
    let (status, _) = patch(&state, &body(&[(SETTING_AUDIO_OUTPUTS, &one)])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let normalized = r#"[{"id":"out-2","name":"DVS","type":"asio","enabled":true,"rate":"network","delay_ms":0,"asio":{"driver":"Dante Virtual Soundcard (x64)","channels":[0,1]}}]"#;
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_OUTPUTS).await.unwrap(),
        normalized
    );
    let twice = format!("[{dvs},{}]", dvs.replace("out-2", "out-3"));
    let (status, text) = patch(&state, &body(&[(SETTING_AUDIO_OUTPUTS, &twice)])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        text,
        "entry 2 (id out-3): asio.driver is already used by an earlier ASIO entry (a driver takes one client)"
    );
    assert_eq!(
        stored(&state.pool, SETTING_AUDIO_OUTPUTS).await.unwrap(),
        normalized,
        "the refused list wrote nothing"
    );
}
