//! #229: this node's exchange on the LAN API (no peer key):
//! `GET /api/v1/exchange/status`.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::Exchange;
use super::catalog::{self, CatalogCounts};
use super::config::NodeConfig;
use super::wire::CatalogJob;

/// `GET /api/v1/exchange/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeStatus {
    pub node_name: Option<String>,
    /// Serving = `node_name` + `peer_api_key` are set: the peer API answers.
    pub serving: bool,
    pub transfers_paused: bool,
    /// Why the exchange settings do not hold (the exchange then acts as off).
    /// It names settings and peers, never a secret (`peer::config`).
    pub config_error: Option<String>,
    pub peers: Vec<PeerStatus>,
    /// The files this node's rows name, how many its catalog lists (hashed)
    /// and the queued job entries it lists; `None` when the rows cannot be
    /// read.
    pub catalog: Option<CatalogCounts>,
    /// The jobs this node runs now (its catalog lists them as running).
    pub jobs: Vec<CatalogJob>,
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
    let transfers_paused = ex.transfers_paused().await;
    let (cfg, config_error) = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => (cfg, None),
        Err(e) => (NodeConfig::default(), Some(e)),
    };
    let catalog = catalog::counts(&ex)
        .await
        .inspect_err(|e| warn!("exchange status: reading the catalog failed: {e}"))
        .ok();
    let jobs = ex.board.snapshot(cfg.node_name.as_deref().unwrap_or(""));
    Json(ExchangeStatus {
        node_name: cfg.node_name.clone(),
        serving: cfg.serving(),
        transfers_paused,
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
        catalog,
        jobs,
    })
}

#[cfg(test)]
#[path = "lan_tests.rs"]
mod tests;
