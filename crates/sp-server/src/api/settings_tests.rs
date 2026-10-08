//! #229 `api::settings` through the real router: `GET /api/v1/settings`
//! shows every secret setting, and each peer's secrets inside `peers`, as
//! the mask; a PATCH that sends a mask back keeps the stored value; an
//! exchange setting that does not hold refuses the whole PATCH (400, nothing
//! written).

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::config::{
    SECRET_MASK, SECRET_SETTINGS, SETTING_CACHE_DIR, SETTING_GEMINI_API_KEY, SETTING_GEMINI_MODEL,
    SETTING_GENIUS_ACCESS_TOKEN, SETTING_NODE_NAME, SETTING_OBS_WEBSOCKET_PASSWORD,
    SETTING_PEER_API_KEY, SETTING_PEERS, SETTING_REMOTE_WS_PASSWORD,
};
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use crate::peer::config::PeerConfig;

/// A peer key of exactly `MIN_KEY_LEN` (32) characters.
const KEY: &str = "example-peer-key-for-the-tests-1";
/// A Cloudflare Access client secret.
const CF_SECRET: &str = "cf-secret-example";
/// A retired credential a node's database can still hold.
const RETIRED: &str = "replicate_api_token";

/// Every secret the tests seed, in clear: the five on `SECRET_SETTINGS` and a
/// retired one.
const SECRETS: &[(&str, &str)] = &[
    (
        SETTING_GEMINI_API_KEY,
        "example-gemini-one, example-gemini-two",
    ),
    (SETTING_GENIUS_ACCESS_TOKEN, "example-genius-token"),
    (SETTING_OBS_WEBSOCKET_PASSWORD, "example-obs-pass"),
    (SETTING_PEER_API_KEY, KEY),
    (SETTING_REMOTE_WS_PASSWORD, "example-remote-pass"),
    (RETIRED, "example-replicate-token"),
];

fn snv() -> PeerConfig {
    PeerConfig {
        name: "snv".into(),
        base_url: "https://sp.newlevel.media".into(),
        key: KEY.into(),
        cf_client_id: Some("client-id.access".into()),
        cf_client_secret: Some(CF_SECRET.into()),
    }
}

fn peers_text(peers: &[PeerConfig]) -> String {
    serde_json::to_string(peers).unwrap()
}

/// A PATCH body.
pub(super) fn body(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

pub(super) async fn stored(pool: &SqlitePool, key: &str) -> Option<String> {
    crate::db::models::get_setting(pool, key).await.unwrap()
}

async fn seed_secrets(state: &AppState) {
    for (key, value) in SECRETS {
        store(&state.pool, key, value).await;
    }
}

/// `GET /api/v1/settings` as text (it must answer 200).
async fn get_text(state: &AppState) -> String {
    let req = Request::builder()
        .uri("/api/v1/settings")
        .body(Body::empty())
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Every setting `GET /api/v1/settings` answers.
async fn get_all(state: &AppState) -> HashMap<String, String> {
    serde_json::from_str(&get_text(state).await).unwrap()
}

/// `PATCH /api/v1/settings` with `map`: the status and the body text.
pub(super) async fn patch(state: &AppState, map: &HashMap<String, String>) -> (StatusCode, String) {
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/v1/settings")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(map).unwrap()))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn every_secret_setting_reads_as_the_mask() {
    for key in SECRET_SETTINGS {
        assert!(
            SECRETS.iter().any(|(k, _)| k == key),
            "the test seeds {key}"
        );
    }
    let state = test_state().await;
    seed_secrets(&state).await;
    store(&state.pool, SETTING_GEMINI_MODEL, "model-x").await;
    store(&state.pool, SETTING_CACHE_DIR, "/tmp/cache").await;
    store(&state.pool, "assemblyai_api_key", "").await;

    let text = get_text(&state).await;
    for (key, clear) in SECRETS {
        for part in clear.split(", ") {
            assert!(!text.contains(part), "{key} in clear in the GET");
        }
    }
    let all: HashMap<String, String> = serde_json::from_str(&text).unwrap();
    for (key, _) in SECRETS {
        assert!(all[*key] == SECRET_MASK, "{key} does not read as the mask");
    }
    assert_eq!(
        all["assemblyai_api_key"], "",
        "an empty secret reveals nothing and stays empty"
    );
    assert_eq!(all[SETTING_GEMINI_MODEL], "model-x");
    assert_eq!(all[SETTING_CACHE_DIR], "/tmp/cache");
}

#[tokio::test]
async fn the_peers_list_shows_each_peers_secrets_masked() {
    let state = test_state().await;
    store(&state.pool, SETTING_PEERS, &peers_text(&[snv()])).await;

    let text = get_text(&state).await;
    assert!(!text.contains(KEY), "the peer key in clear");
    assert!(!text.contains(CF_SECRET), "the Cloudflare secret in clear");
    let all: HashMap<String, String> = serde_json::from_str(&text).unwrap();
    let peers: Vec<PeerConfig> = serde_json::from_str(&all[SETTING_PEERS]).unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].name, "snv");
    assert_eq!(peers[0].base_url, "https://sp.newlevel.media");
    assert!(
        peers[0].key == SECRET_MASK,
        "the peer key reads as the mask"
    );
    assert_eq!(peers[0].cf_client_id.as_deref(), Some("client-id.access"));
    assert!(
        peers[0].cf_client_secret.as_deref() == Some(SECRET_MASK),
        "the cf secret reads as the mask"
    );
}

/// The dashboard's save: read the map, change one field, send the whole map
/// back with its masks.
#[tokio::test]
async fn a_settings_map_saved_back_with_masks_keeps_every_secret() {
    let state = test_state().await;
    seed_secrets(&state).await;
    store(&state.pool, SETTING_NODE_NAME, "pp").await;
    store(&state.pool, SETTING_PEERS, &peers_text(&[snv()])).await;

    let mut all = get_all(&state).await;
    all.insert(SETTING_GEMINI_MODEL.to_string(), "model-y".to_string());
    let (status, text) = patch(&state, &all).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");

    for (key, clear) in SECRETS {
        assert!(
            stored(&state.pool, key).await.as_deref() == Some(*clear),
            "{key} kept"
        );
    }
    let peers: Vec<PeerConfig> =
        serde_json::from_str(&stored(&state.pool, SETTING_PEERS).await.unwrap()).unwrap();
    assert_eq!(peers, vec![snv()], "the peer keeps its key and CF secret");
    assert_eq!(
        stored(&state.pool, SETTING_NODE_NAME).await.as_deref(),
        Some("pp")
    );
    assert_eq!(
        stored(&state.pool, SETTING_GEMINI_MODEL).await.as_deref(),
        Some("model-y")
    );
}

#[tokio::test]
async fn a_new_secret_replaces_the_stored_one_and_an_empty_one_clears_it() {
    let state = test_state().await;
    seed_secrets(&state).await;

    let sent = body(&[
        (SETTING_GEMINI_API_KEY, "example-gemini-three"),
        (SETTING_REMOTE_WS_PASSWORD, ""),
    ]);
    let (status, text) = patch(&state, &sent).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert!(
        stored(&state.pool, SETTING_GEMINI_API_KEY).await.as_deref()
            == Some("example-gemini-three"),
        "the new gemini_api_key replaces the stored one"
    );
    assert!(
        stored(&state.pool, SETTING_REMOTE_WS_PASSWORD)
            .await
            .as_deref()
            == Some(""),
        "the empty remote_ws_password clears the stored one"
    );
}

#[tokio::test]
async fn the_mask_keeps_only_a_masked_setting() {
    let state = test_state().await;
    store(&state.pool, SETTING_GEMINI_MODEL, "model-x").await;

    let (status, text) = patch(&state, &body(&[(SETTING_GEMINI_MODEL, SECRET_MASK)])).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert_eq!(
        stored(&state.pool, SETTING_GEMINI_MODEL).await.as_deref(),
        Some(SECRET_MASK),
        "a plain setting takes the mask as sent"
    );
}

#[tokio::test]
async fn a_bad_peer_list_is_refused_and_nothing_is_written() {
    let state = test_state().await;

    let sent = body(&[
        (SETTING_GEMINI_MODEL, "model-z"),
        (SETTING_PEERS, "[{\"name\":\"SNV\"}]"),
    ]);
    let (status, text) = patch(&state, &sent).await;
    assert!(
        !text.contains("SNV"),
        "the reason never echoes the sent peers text"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains(SETTING_PEERS),
        "the reason names peers: {text}"
    );
    assert_eq!(stored(&state.pool, SETTING_GEMINI_MODEL).await, None);
    assert_eq!(stored(&state.pool, SETTING_PEERS).await, None);
}

#[tokio::test]
async fn a_peer_api_key_needs_32_characters() {
    let state = test_state().await;
    let short = &KEY[..31];

    let (status, text) = patch(&state, &body(&[(SETTING_PEER_API_KEY, short)])).await;
    assert!(!text.contains(short), "the key never appears");
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        stored(&state.pool, SETTING_PEER_API_KEY).await.is_none(),
        "a 31-character peer_api_key was written"
    );

    let (status, text) = patch(&state, &body(&[(SETTING_PEER_API_KEY, KEY)])).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert!(
        stored(&state.pool, SETTING_PEER_API_KEY).await.as_deref() == Some(KEY),
        "the 32-character peer_api_key is written"
    );
}

#[tokio::test]
async fn a_node_name_that_is_not_a_name_is_refused() {
    let state = test_state().await;

    let (status, text) = patch(&state, &body(&[(SETTING_NODE_NAME, "PP")])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(stored(&state.pool, SETTING_NODE_NAME).await, None);
}

/// A `peers` sent back as the mask is kept, so it is not "sent": the node
/// name is checked against the STORED peers (`NodeConfig::load` refuses a
/// node named like its peer).
#[tokio::test]
async fn a_masked_peer_list_does_not_hide_a_node_named_like_a_peer() {
    let state = test_state().await;
    let peers = peers_text(&[snv()]);
    store(&state.pool, SETTING_PEERS, &peers).await;

    let sent = body(&[(SETTING_NODE_NAME, "snv"), (SETTING_PEERS, SECRET_MASK)]);
    let (status, text) = patch(&state, &sent).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(stored(&state.pool, SETTING_NODE_NAME).await, None);
    assert!(
        stored(&state.pool, SETTING_PEERS).await == Some(peers),
        "the stored peers are untouched"
    );
}

/// The PATCH has no login and the API answers any origin, so the GET's
/// masked peer list sent back with a new `base_url` must not re-point the
/// stored key and Cloudflare token at that host.
#[tokio::test]
async fn a_masked_peer_key_cannot_follow_a_new_base_url() {
    let state = test_state().await;
    store(&state.pool, SETTING_PEERS, &peers_text(&[snv()])).await;

    let mut all = get_all(&state).await;
    let mut shown: Vec<PeerConfig> = serde_json::from_str(&all[SETTING_PEERS]).unwrap();
    assert!(
        shown[0].key == SECRET_MASK,
        "the GET shows the peer key masked"
    );
    shown[0].base_url = "https://attacker.example".into();
    all.insert(SETTING_PEERS.to_string(), peers_text(&shown));
    let (status, text) = patch(&state, &all).await;
    assert!(!text.contains(KEY), "the reason never names the key");
    assert!(
        !text.contains("attacker.example"),
        "the reason never names the URL: {text}"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains("snv") && text.contains("base_url"),
        "the reason names the peer and its base_url: {text}"
    );
    let kept: Vec<PeerConfig> =
        serde_json::from_str(&stored(&state.pool, SETTING_PEERS).await.unwrap()).unwrap();
    assert_eq!(kept, vec![snv()], "the stored peer is untouched");
}
