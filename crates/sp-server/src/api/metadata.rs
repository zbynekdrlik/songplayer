//! #136: the metadata provider chain on the API.
//!
//! - [`status_block`] → `GET /api/v1/status.metadata`: the size of the repair
//!   queue and every provider's health (last answer, last error).
//! - `POST /api/v1/metadata/probe {youtube_id, title}` ([`probe`]) runs EACH
//!   provider of the production chain on its own, concurrently, and returns
//!   each outcome. It writes nothing to the DB (a probe call still lands in
//!   the providers' health record — it is a real call). The post-deploy gate
//!   (`e2e/post-deploy-metadata.spec.ts`) probes a fixed real video, so a
//!   broken key format, a retired model or an unauthenticated proxy fails the
//!   deploy instead of shipping raw YouTube titles to the wall.

use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;
use crate::metadata::MetadataProvider;
use crate::metadata::health::{MetadataStatus, failed_videos};

/// `status.metadata` (module doc).
pub async fn status_block(state: &AppState) -> MetadataStatus {
    let failed_videos = failed_videos(&state.pool)
        .await
        .inspect_err(|e| warn!("status.metadata: counting the repair queue failed: {e}"))
        .ok();
    MetadataStatus {
        failed_videos,
        providers: state.metadata_chain.health(),
    }
}

/// `POST /api/v1/metadata/probe` body. A typed struct (never a
/// `serde_json::Value`, `.claude/rules/rust-workspace.md`).
#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub youtube_id: String,
    pub title: String,
}

/// One provider's answer to a probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    pub name: String,
    pub ok: bool,
    pub song: Option<String>,
    pub artist: Option<String>,
    /// The provider's error (never a key: `gemini::GeminiProvider` redacts).
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// `POST /api/v1/metadata/probe` answer: the chain's providers, in order.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub youtube_id: String,
    pub providers: Vec<ProbeOutcome>,
}

/// `POST /api/v1/metadata/probe` (module doc). 400 without a `youtube_id`
/// or a `title`.
pub async fn probe(
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> impl IntoResponse {
    let youtube_id = req.youtube_id.trim();
    let title = req.title.trim();
    if youtube_id.is_empty() || title.is_empty() {
        return (StatusCode::BAD_REQUEST, "youtube_id and title are required").into_response();
    }
    info!(
        youtube_id,
        title, "metadata probe: asking every provider of the chain"
    );
    let providers = futures::future::join_all(
        state
            .metadata_chain
            .providers()
            .iter()
            .map(|p| probe_one(p, youtube_id, title)),
    )
    .await;
    for p in &providers {
        info!(provider = %p.name, ok = p.ok, elapsed_ms = p.elapsed_ms, song = ?p.song,
            artist = ?p.artist, error = ?p.error, "metadata probe outcome");
    }
    Json(ProbeResponse {
        youtube_id: youtube_id.to_string(),
        providers,
    })
    .into_response()
}

async fn probe_one(provider: &dyn MetadataProvider, youtube_id: &str, title: &str) -> ProbeOutcome {
    let started = Instant::now();
    let result = provider.extract(youtube_id, title).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let name = provider.name().to_string();
    match result {
        Ok(meta) => ProbeOutcome {
            name,
            ok: true,
            song: Some(meta.song),
            artist: Some(meta.artist),
            error: None,
            elapsed_ms,
        },
        Err(e) => ProbeOutcome {
            name,
            ok: false,
            song: None,
            artist: None,
            error: Some(e.to_string()),
            elapsed_ms,
        },
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
