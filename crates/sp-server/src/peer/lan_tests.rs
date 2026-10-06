//! #229 `GET /api/v1/exchange/status` through the exchange's own router: the
//! node, whether it serves, the pause, why its settings do not hold, and its
//! peers — never a key or a Cloudflare secret.
//! (Repo convention: names the parent only imports are imported explicitly.)

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::config::{
    SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEER_TRANSFERS_PAUSED, SETTING_PEERS,
};
use sqlx::SqlitePool;
use tower::ServiceExt;

use super::*;
use crate::peer::Exchange;
use crate::peer::config::PeerConfig;

/// A peer key of exactly `MIN_KEY_LEN` (32) characters.
const KEY: &str = "example-peer-key-for-the-tests-1";
/// A Cloudflare Access client secret.
const CF_SECRET: &str = "cf-secret-example";

/// The exchange over a migrated in-memory database (its own per test); the
/// temporary cache dir lives as long as the returned guard.
async fn exchange() -> (Arc<Exchange>, tempfile::TempDir) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    (Exchange::new(pool, dir.path().to_path_buf()), dir)
}

async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

/// The `snv` peer, with or without a Cloudflare Access service token.
fn snv(cf: bool) -> PeerConfig {
    PeerConfig {
        name: "snv".into(),
        base_url: "https://sp.newlevel.media".into(),
        key: KEY.into(),
        cf_client_id: cf.then(|| "client-id.access".into()),
        cf_client_secret: cf.then(|| CF_SECRET.into()),
    }
}

/// `GET /api/v1/exchange/status`: the status code and the body.
async fn get_status(ex: &Arc<Exchange>) -> (StatusCode, String) {
    let req = Request::builder()
        .uri("/api/v1/exchange/status")
        .body(Body::empty())
        .unwrap();
    let resp = crate::peer::router(ex.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn status_shows_the_node_and_its_peers_without_secrets() {
    let (ex, _dir) = exchange().await;
    let peers = serde_json::to_string(&[snv(true)]).unwrap();
    store(&ex.pool, SETTING_NODE_NAME, "pp").await;
    store(&ex.pool, SETTING_PEERS, &peers).await;
    store(&ex.pool, SETTING_PEER_TRANSFERS_PAUSED, "true").await;
    let (code, body) = get_status(&ex).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains(KEY) && !body.contains(CF_SECRET), "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert_eq!(s.node_name.as_deref(), Some("pp"));
    assert!(!s.serving, "no peer_api_key here");
    assert!(s.transfers_paused);
    assert_eq!(s.config_error, None);
    assert_eq!(
        s.peers,
        vec![PeerStatus {
            name: "snv".into(),
            base_url: "https://sp.newlevel.media".into(),
            has_key: true,
            cf_access: true,
        }]
    );
}

#[tokio::test]
async fn status_names_a_setting_that_does_not_hold() {
    let (ex, _dir) = exchange().await;
    let peers = serde_json::to_string(&[snv(true)]).unwrap();
    store(&ex.pool, SETTING_NODE_NAME, "PP").await;
    // A key and a peer that hold on their own: a config that does not hold
    // acts as off as a whole, so neither shows.
    store(&ex.pool, SETTING_PEER_API_KEY, KEY).await;
    store(&ex.pool, SETTING_PEERS, &peers).await;
    let (code, body) = get_status(&ex).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains(KEY) && !body.contains(CF_SECRET), "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert!(
        s.config_error
            .as_deref()
            .is_some_and(|e| e.contains("node_name")),
        "{:?}",
        s.config_error
    );
    assert_eq!(s.node_name, None);
    assert!(!s.serving);
    assert!(!s.transfers_paused);
    assert_eq!(s.peers, Vec::<PeerStatus>::new());
}

#[tokio::test]
async fn a_named_node_with_its_key_serves_and_lists_no_peer() {
    let (ex, _dir) = exchange().await;
    store(&ex.pool, SETTING_NODE_NAME, "snv").await;
    store(&ex.pool, SETTING_PEER_API_KEY, KEY).await;
    let (code, body) = get_status(&ex).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains(KEY), "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert_eq!(s.node_name.as_deref(), Some("snv"));
    assert!(s.serving);
    assert!(!s.transfers_paused);
    assert_eq!(s.config_error, None);
    assert_eq!(s.peers, Vec::<PeerStatus>::new());
}

/// `GET uri` on `app`: the status code and the body.
async fn get_on(app: &Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// The router `lib.rs` serves: the app's (with its SPA fallback) merged with
/// the exchange's. axum panics on a merge of two fallbacks, and the SPA must
/// still serve every path no route takes.
#[tokio::test]
async fn the_exchange_routes_merge_with_the_app_router_and_its_spa_fallback() {
    let state = crate::api::routes::tests::test_state().await;
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<p>the dashboard</p>").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let ex = Exchange::new(state.pool.clone(), cache.path().to_path_buf());
    let app =
        crate::api::router(state, Some(dist.path().to_path_buf())).merge(crate::peer::router(ex));

    let (code, body) = get_on(&app, "/api/v1/exchange/status").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert_eq!(s.config_error, None);
    assert_eq!(s.node_name, None);
    assert!(!s.serving);

    let (code, body) = get_on(&app, "/some/spa/path").await;
    assert_eq!(code, StatusCode::OK, "the SPA fallback still serves");
    assert_eq!(body, "<p>the dashboard</p>");

    let (code, body) = get_on(&app, "/api/v1/settings").await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_peer_without_a_cloudflare_token_has_no_cf_access() {
    let (ex, _dir) = exchange().await;
    let peers = serde_json::to_string(&[snv(false)]).unwrap();
    store(&ex.pool, SETTING_PEERS, &peers).await;
    let (code, body) = get_status(&ex).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains(KEY), "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert_eq!(s.config_error, None);
    assert_eq!(
        s.peers,
        vec![PeerStatus {
            name: "snv".into(),
            base_url: "https://sp.newlevel.media".into(),
            has_key: true,
            cf_access: false,
        }]
    );
}
