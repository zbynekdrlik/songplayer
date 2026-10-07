//! #229: this node's exchange on the LAN API (no peer key):
//! `GET /api/v1/exchange/status`, `POST /api/v1/exchange/probe` and
//! `POST /api/v1/exchange/probe/transfer`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::Exchange;
use super::catalog::{self, CatalogCounts};
use super::client::LastRead;
use super::config::NodeConfig;
use super::transfer_probe::TransferProbe;
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
    /// The last read of its catalog (a probe, or a job asking first), since
    /// this process started.
    pub last_read: Option<LastRead>,
}

/// One peer's live catalog read (`POST /api/v1/exchange/probe`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub name: String,
    pub base_url: String,
    pub ok: bool,
    pub artifacts: usize,
    pub jobs: usize,
    pub latency_ms: u64,
    pub error: Option<String>,
}

pub fn router(ex: Arc<Exchange>) -> Router {
    Router::new()
        .route("/api/v1/exchange/status", get(status))
        .route("/api/v1/exchange/probe", post(probe))
        .route("/api/v1/exchange/probe/transfer", post(probe_transfer))
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
    let reads = ex.client.last_reads();
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
                last_read: reads.get(&p.name).cloned(),
            })
            .collect(),
        catalog,
        jobs,
    })
}

/// Read every peer's catalog now: the live gate of PP → SNV through
/// Cloudflare. 409 with the reason when the exchange settings do not hold.
pub async fn probe(State(ex): State<Arc<Exchange>>) -> Response {
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => return (StatusCode::CONFLICT, e).into_response(),
    };
    let mut results = Vec::new();
    for peer in &cfg.peers {
        let read = ex.client.read_catalog(peer).await;
        let latency_ms = ex
            .client
            .last_reads()
            .get(&peer.name)
            .map_or(0, |r| r.latency_ms);
        results.push(ProbeResult {
            name: peer.name.clone(),
            base_url: peer.base_url.clone(),
            ok: read.is_ok(),
            artifacts: read.as_ref().map_or(0, |c| c.artifacts.len()),
            jobs: read.as_ref().map_or(0, |c| c.jobs.len()),
            latency_ms,
            error: read.err().map(|e| e.to_string()),
        });
    }
    let ok = results.iter().filter(|r| r.ok).count();
    info!(peers = results.len(), ok, "exchange: probe");
    Json(results).into_response()
}

/// One real artifact from every peer, now (`peer::transfer_probe`): the
/// live gate of PP's transfers through Cloudflare. 409 when a transfer probe
/// is already running or the exchange settings do not hold.
pub async fn probe_transfer(State(ex): State<Arc<Exchange>>) -> Response {
    let Ok(_one) = ex.transfer_probe.try_lock() else {
        return (StatusCode::CONFLICT, "a transfer probe is already running").into_response();
    };
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => return (StatusCode::CONFLICT, e).into_response(),
    };
    let tmp = std::env::temp_dir();
    let mut results: Vec<TransferProbe> = Vec::new();
    for peer in &cfg.peers {
        results.push(ex.probe_transfer(peer, &tmp).await);
    }
    Json(results).into_response()
}

#[cfg(test)]
#[path = "lan_tests.rs"]
mod tests;
