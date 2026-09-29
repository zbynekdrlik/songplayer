//! #136: the metadata provider chain on the API.
//!
//! - [`status_block`] → `GET /api/v1/status.metadata`: the size of the repair
//!   queue and every provider's health (last answer, last error).
//! - `POST /api/v1/metadata/probe {youtube_id, title}` ([`probe`]) runs EACH
//!   provider of the production chain on its own, concurrently, each bounded
//!   by [`PROBE_TIMEOUT`], and returns each outcome. It writes nothing to the
//!   DB (a probe call that completes still lands in the providers' health
//!   record — it is a real call). The post-deploy gate
//!   (`e2e/post-deploy-metadata.spec.ts`) probes a fixed real video, so a
//!   broken key format, a retired model or an unauthenticated proxy fails the
//!   deploy instead of shipping raw YouTube titles to the wall.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;
use crate::metadata::MetadataProvider;
use crate::metadata::health::{MetadataStatus, bounded_error, failed_videos};

/// Bound of one provider in a probe: below the post-deploy spec's 220 s
/// request timeout, so a hung provider fails the gate WITH its name instead
/// of as a bare Playwright timeout (Claude's own client allows 300 s + retries).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(180);

/// Longest `youtube_id` / `title` a probe accepts (characters): a YouTube id
/// is 11, a title rarely over 150 — anything longer is refused before it is
/// logged or sent to two paid providers.
pub const MAX_PROBE_ID_CHARS: usize = 64;
pub const MAX_PROBE_TITLE_CHARS: usize = 500;

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

/// `POST /api/v1/metadata/probe` (module doc). 400 on an empty or over-long
/// `youtube_id` / `title` (a missing field is axum's 422).
pub async fn probe(
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> impl IntoResponse {
    let youtube_id = req.youtube_id.trim();
    let title = req.title.trim();
    if youtube_id.is_empty() || title.is_empty() {
        return (StatusCode::BAD_REQUEST, "youtube_id and title are required").into_response();
    }
    if youtube_id.chars().count() > MAX_PROBE_ID_CHARS
        || title.chars().count() > MAX_PROBE_TITLE_CHARS
    {
        return (StatusCode::BAD_REQUEST, "youtube_id or title too long").into_response();
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
            .map(|p| probe_one(&**p, youtube_id, title, PROBE_TIMEOUT)),
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

/// One provider's probe, bounded by `limit`. The error is kept to
/// `health::MAX_ERROR_CHARS`.
async fn probe_one(
    provider: &dyn MetadataProvider,
    youtube_id: &str,
    title: &str,
    limit: Duration,
) -> ProbeOutcome {
    let started = Instant::now();
    let result = tokio::time::timeout(limit, provider.extract(youtube_id, title)).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let name = provider.name().to_string();
    let error = match result {
        Ok(Ok(meta)) => {
            return ProbeOutcome {
                name,
                ok: true,
                song: Some(meta.song),
                artist: Some(meta.artist),
                error: None,
                elapsed_ms,
            };
        }
        Ok(Err(e)) => bounded_error(&e.to_string()),
        Err(_) => format!("no answer within {} s", limit.as_secs_f64()),
    };
    ProbeOutcome {
        name,
        ok: false,
        song: None,
        artist: None,
        error: Some(error),
        elapsed_ms,
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
