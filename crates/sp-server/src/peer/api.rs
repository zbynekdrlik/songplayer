//! #229: the peer API — `/api/v1/peer/…`, for other SongPlayer nodes only.
//!
//! - Every request carries this node's `peer_api_key` as `X-SP-Peer-Key`
//!   (compared as sha256 digests); on the public path (`sp.newlevel.media`)
//!   Cloudflare Access sits in front too. Keys are never logged.
//! - A node that does not serve (no `node_name` or no `peer_api_key`, or
//!   exchange settings that do not hold) answers 404: the API is off.
//! - `peer_transfers_paused` answers 503 `Retry-After: 600` on every route,
//!   after the key check (a caller without the key learns nothing).
//! - Every answer is `Cache-Control: no-store` (Cloudflare never caches it).
//! - Files go through tower-http `ServeFile` (Range → 206, HEAD, 416) and a
//!   body throttled to `peer_serve_max_mbps`: SNV's uplink also carries the
//!   live stream.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sp_core::config::{SETTING_PEER_SERVE_MAX_MBPS, peer_serve_max_mbps};
use tower_http::services::ServeFile;
use tracing::{info, warn};

use super::Exchange;
use super::catalog;
use super::config::NodeConfig;
use super::kind::ArtifactKind;
use super::throttle::{mbps_to_bytes, throttled};
use super::wire::rfc3339_to_ms;
use crate::downloader::cache::is_valid_video_id;

/// The header a peer sends this node's `peer_api_key` in.
pub const PEER_KEY_HEADER: &str = "x-sp-peer-key";
/// How long a paused node asks a peer to wait, in seconds.
const PAUSED_RETRY_AFTER: &str = "600";

pub fn router(ex: Arc<Exchange>) -> Router {
    Router::new()
        .route("/api/v1/peer/catalog", get(catalog_route))
        .route("/api/v1/peer/videos/{youtube_id}", get(video_route))
        .route(
            "/api/v1/peer/artifact/{youtube_id}/{kind}",
            get(artifact_route),
        )
        .route_layer(middleware::from_fn_with_state(ex.clone(), guard))
        .with_state(ex)
}

/// `given` is `expected`: the two sha256 digests are compared, so the time
/// taken says nothing about the key's bytes.
pub fn keys_match(given: &str, expected: &str) -> bool {
    Sha256::digest(given.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// 404 when this node does not serve, 401 without its key, 503 while
/// transfers are paused; otherwise the route.
async fn guard(State(ex): State<Arc<Exchange>>, req: Request, next: Next) -> Response {
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(error = %e, "peer API: the exchange settings do not hold - answering 404");
            return no_store(StatusCode::NOT_FOUND.into_response());
        }
    };
    let Some(expected) = cfg.serve_key.as_deref().filter(|_| cfg.serving()) else {
        return no_store(StatusCode::NOT_FOUND.into_response());
    };
    let given = req
        .headers()
        .get(PEER_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !keys_match(given, expected) {
        warn!(path = %req.uri().path(), "peer API: refused a request without this node's key");
        return no_store(StatusCode::UNAUTHORIZED.into_response());
    }
    if ex.transfers_paused().await {
        info!(path = %req.uri().path(), "peer API: transfers are paused - answering 503");
        let paused = (
            StatusCode::SERVICE_UNAVAILABLE,
            [(RETRY_AFTER, PAUSED_RETRY_AFTER)],
        );
        return no_store(paused.into_response());
    }
    no_store(next.run(req).await)
}

#[derive(Debug, Deserialize)]
pub struct CatalogQuery {
    /// Only files listed (hashed) strictly after this RFC 3339 time.
    pub since: Option<String>,
}

async fn catalog_route(State(ex): State<Arc<Exchange>>, Query(q): Query<CatalogQuery>) -> Response {
    let since_ms = match q.since.as_deref() {
        None => None,
        Some(text) => match rfc3339_to_ms(text) {
            Some(ms) => Some(ms),
            None => {
                return (StatusCode::BAD_REQUEST, "since must be an RFC 3339 time").into_response();
            }
        },
    };
    let node = NodeConfig::load(&ex.pool)
        .await
        .ok()
        .and_then(|c| c.node_name)
        .unwrap_or_default();
    match catalog::build(&ex, &node, since_ms).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => {
            warn!(%e, "peer API: building the catalog failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn video_route(State(ex): State<Arc<Exchange>>, Path(youtube_id): Path<String>) -> Response {
    if !is_valid_video_id(&youtube_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match catalog::peer_video(&ex.pool, &youtube_id).await {
        Ok(Some(video)) => Json(video).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, "peer API: reading a video failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn artifact_route(
    State(ex): State<Arc<Exchange>>,
    Path((youtube_id, kind_name)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(kind) = ArtifactKind::parse(&kind_name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !is_valid_video_id(&youtube_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if kind == ArtifactKind::Metadata {
        return match catalog::metadata_for(&ex.pool, Some(&youtube_id)).await {
            Ok(mut found) => match found.pop() {
                Some(m) => ([(CONTENT_TYPE, "application/json")], m.to_bytes()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            },
            Err(e) => {
                warn!(%e, youtube_id = %youtube_id, "peer API: reading a title failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }
    let files = match catalog::artifact_files(&ex.pool, &ex.cache_dir, Some(&youtube_id)).await {
        Ok(files) => files,
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, "peer API: reading the rows failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(file) = files.into_iter().find(|f| f.kind == kind) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mbps = crate::db::models::get_setting(&ex.pool, SETTING_PEER_SERVE_MAX_MBPS)
        .await
        .ok()
        .flatten();
    let rate = mbps_to_bytes(peer_serve_max_mbps(mbps.as_deref()));
    match ServeFile::new(&file.path).try_call(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            info!(
                youtube_id = %youtube_id,
                kind = kind.as_str(),
                status = parts.status.as_u16(),
                "peer API: serving an artifact"
            );
            Response::from_parts(parts, throttled(Body::new(body), rate))
        }
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, kind = kind.as_str(), "peer API: reading an artifact failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `resp` with `Cache-Control: no-store`.
fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
