//! #229: this node's exchange on the LAN API (no peer key):
//! `GET /api/v1/exchange/status`.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sp_core::config::{SETTING_PEER_TRANSFERS_PAUSED, peer_transfers_paused};
use tracing::warn;

use super::Exchange;
use super::config::NodeConfig;

/// `GET /api/v1/exchange/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeStatus {
    pub node_name: Option<String>,
    /// The peer API answers (`node_name` + `peer_api_key` set).
    pub serving: bool,
    pub transfers_paused: bool,
    /// Why the exchange settings do not hold (the exchange then acts as off).
    /// It names settings and peers, never a secret (`peer::config`).
    pub config_error: Option<String>,
    pub peers: Vec<PeerStatus>,
}

/// One configured peer, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub base_url: String,
    pub has_key: bool,
    /// A Cloudflare Access service token is configured for it.
    pub cf_access: bool,
}

pub fn router(ex: Arc<Exchange>) -> Router {
    Router::new()
        .route("/api/v1/exchange/status", get(status))
        .with_state(ex)
}

pub async fn status(State(ex): State<Arc<Exchange>>) -> Json<ExchangeStatus> {
    let paused = crate::db::models::get_setting(&ex.pool, SETTING_PEER_TRANSFERS_PAUSED)
        .await
        .inspect_err(|e| {
            warn!("exchange status: reading {SETTING_PEER_TRANSFERS_PAUSED} failed: {e}")
        })
        .ok()
        .flatten();
    let (cfg, config_error) = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => (cfg, None),
        Err(e) => (NodeConfig::default(), Some(e)),
    };
    Json(ExchangeStatus {
        node_name: cfg.node_name.clone(),
        serving: cfg.serving(),
        transfers_paused: peer_transfers_paused(paused.as_deref()),
        config_error,
        peers: cfg
            .peers
            .iter()
            .map(|p| PeerStatus {
                name: p.name.clone(),
                base_url: p.base_url.clone(),
                has_key: !p.key.is_empty(),
                cf_access: p.cf_client_id.is_some(),
            })
            .collect(),
    })
}

#[cfg(test)]
#[path = "lan_tests.rs"]
mod tests;
