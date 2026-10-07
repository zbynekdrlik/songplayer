//! #229: this node asking its peers — a catalog, a video's row (an artifact:
//! `fetch.rs`). A peer behind Cloudflare Access gets its service token as
//! `CF-Access-Client-Id/Secret`; redirects are NEVER followed, because Access
//! refuses a bad token with a 302 to its login page (the app has
//! `auto_redirect_to_identity`), which must read as a refusal, never as a
//! catalog. Bodies are bounded and parsed into typed structs (no
//! `serde_json::Value`); a parse error names only its position. A good
//! catalog is kept [`CATALOG_TTL`]. No error text holds a key or a URL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::api::PEER_KEY_HEADER;
use super::config::PeerConfig;
use super::wire::{Catalog, PeerVideo, ms_to_rfc3339, now_ms};
use crate::downloader::cache::is_valid_video_id;

/// How long a good catalog read is reused.
pub const CATALOG_TTL: Duration = Duration::from_secs(60);
/// The largest catalog read (SNV's ~1 000 songs × 6 kinds ≈ 1 MB).
pub const MAX_CATALOG_BYTES: usize = 32 << 20;
/// The largest video row read.
const MAX_VIDEO_BYTES: usize = 64 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Between two reads of a body; an artifact has no total bound (minutes at
/// the upload cap), a stall is bounded by this.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// A catalog or a video row, whole.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CF_ACCESS_CLIENT_ID: &str = "cf-access-client-id";
const CF_ACCESS_CLIENT_SECRET: &str = "cf-access-client-secret";

/// Why a peer did not give what was asked. The text never holds a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    #[error("unreachable: {0}")]
    Unreachable(String),
    #[error("refused by Cloudflare Access (HTTP {0}) - check the service token")]
    AccessRefused(u16),
    #[error("refused this node's peer key (HTTP 401)")]
    KeyRefused,
    #[error("not found (HTTP 404); for a catalog: the peer API is off there")]
    NotFound,
    #[error("paused there (HTTP 503)")]
    Paused,
    #[error("bad answer: {0}")]
    BadResponse(String),
    #[error("sha256 mismatch: the catalog says {expected}, the bytes are {got}")]
    ShaMismatch { expected: String, got: String },
    /// Nothing was transferred: this node cannot tell yet whether the peer's
    /// copy fits it (`peer::audio`).
    #[error("not taken yet: {0}")]
    NotYet(String),
    #[error("local: {0}")]
    Io(String),
}

impl From<std::io::Error> for PeerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// A database step of an adopter (`peer::{download, stems, lyrics}`:
/// recording a fetched artifact on this node's row) is a local failure too.
impl From<sqlx::Error> for PeerError {
    fn from(e: sqlx::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// The error an HTTP status means; `None` for a success.
pub fn status_error(status: u16) -> Option<PeerError> {
    match status {
        200 | 206 => None,
        300..=399 | 403 => Some(PeerError::AccessRefused(status)),
        401 => Some(PeerError::KeyRefused),
        404 => Some(PeerError::NotFound),
        503 => Some(PeerError::Paused),
        other => Some(PeerError::BadResponse(format!("HTTP {other}"))),
    }
}

/// A catalog read at `at` is still good at `now`.
pub fn fresh(at: Instant, now: Instant) -> bool {
    now.duration_since(at) < CATALOG_TTL
}

/// The last catalog read of a peer, for the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRead {
    pub ok: bool,
    /// When it was read (RFC 3339, UTC).
    pub at: String,
    pub artifacts: usize,
    pub jobs: usize,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// One per process (`Exchange::client`): the HTTP client, the catalog cache,
/// the last reads and the transfer slots.
pub struct PeerClient {
    http: reqwest::Client,
    max_catalog_bytes: usize,
    cache: Mutex<HashMap<String, (Instant, Arc<Catalog>)>>,
    reads: Mutex<HashMap<String, LastRead>>,
    /// One transfer at a time per peer (`fetch.rs`).
    pub(crate) slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl PeerClient {
    pub(crate) fn new() -> Self {
        Self::with_max_catalog_bytes(MAX_CATALOG_BYTES)
    }

    pub(crate) fn with_max_catalog_bytes(max_catalog_bytes: usize) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(format!("SongPlayer/{}", sp_core::config::VERSION))
            .build()
            .expect("the peer HTTP client builds from static options");
        Self {
            http,
            max_catalog_bytes,
            cache: Mutex::new(HashMap::new()),
            reads: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// A GET of `path` on `peer`, with its key and (when it has one) its
    /// Access service token.
    pub(crate) fn get(&self, peer: &PeerConfig, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", peer.base_url.trim_end_matches('/'));
        let mut req = self.http.get(url).header(PEER_KEY_HEADER, &peer.key);
        if let (Some(id), Some(secret)) = (&peer.cf_client_id, &peer.cf_client_secret) {
            req = req
                .header(CF_ACCESS_CLIENT_ID, id)
                .header(CF_ACCESS_CLIENT_SECRET, secret);
        }
        req
    }

    /// `peer`'s catalog, read now; a good one refreshes the cache. The
    /// outcome is kept for the status ([`PeerClient::last_reads`]).
    pub async fn read_catalog(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let started = Instant::now();
        let result = self.catalog_now(peer).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let read = match &result {
            Ok(c) => LastRead {
                ok: true,
                at: ms_to_rfc3339(now_ms()),
                artifacts: c.artifacts.len(),
                jobs: c.jobs.len(),
                latency_ms,
                error: None,
            },
            Err(e) => LastRead {
                ok: false,
                at: ms_to_rfc3339(now_ms()),
                artifacts: 0,
                jobs: 0,
                latency_ms,
                error: Some(e.to_string()),
            },
        };
        lock(&self.reads).insert(peer.name.clone(), read);
        if let Ok(c) = &result {
            lock(&self.cache).insert(peer.name.clone(), (Instant::now(), Arc::clone(c)));
        }
        result
    }

    async fn catalog_now(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let resp = self
            .get(peer, "/api/v1/peer/catalog")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_err)?;
        if let Some(e) = status_error(resp.status().as_u16()) {
            return Err(e);
        }
        let body = read_bounded(resp, self.max_catalog_bytes).await?;
        let catalog: Catalog = serde_json::from_slice(&body).map_err(|e| {
            PeerError::BadResponse(format!(
                "not a catalog (line {}, column {})",
                e.line(),
                e.column()
            ))
        })?;
        Ok(Arc::new(catalog.sanitized()))
    }

    /// `peer`'s catalog, read at most once per [`CATALOG_TTL`] (only a good
    /// read is kept).
    pub async fn catalog(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let now = Instant::now();
        let cached = lock(&self.cache)
            .get(&peer.name)
            .filter(|(at, _)| fresh(*at, now))
            .map(|(_, c)| Arc::clone(c));
        match cached {
            Some(c) => Ok(c),
            None => self.read_catalog(peer).await,
        }
    }

    /// Drop `peer`'s cached catalog (after a sha mismatch: it may be stale).
    pub fn forget_catalog(&self, peer: &str) {
        lock(&self.cache).remove(peer);
    }

    /// The last catalog read of each peer, by name.
    pub fn last_reads(&self) -> HashMap<String, LastRead> {
        lock(&self.reads).clone()
    }

    /// `GET /api/v1/peer/videos/{youtube_id}` on `peer`.
    pub async fn video(&self, peer: &PeerConfig, youtube_id: &str) -> Result<PeerVideo, PeerError> {
        if !is_valid_video_id(youtube_id) {
            return Err(PeerError::BadResponse(format!(
                "{youtube_id:?} is no YouTube id"
            )));
        }
        let resp = self
            .get(peer, &format!("/api/v1/peer/videos/{youtube_id}"))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_err)?;
        if let Some(e) = status_error(resp.status().as_u16()) {
            return Err(e);
        }
        let body = read_bounded(resp, MAX_VIDEO_BYTES).await?;
        let video: PeerVideo = serde_json::from_slice(&body).map_err(|e| {
            PeerError::BadResponse(format!(
                "not a video row (line {}, column {})",
                e.line(),
                e.column()
            ))
        })?;
        if video.metadata.youtube_id != youtube_id {
            return Err(PeerError::BadResponse("the row is of another video".into()));
        }
        Ok(video)
    }
}

/// The map, even after a panic elsewhere poisoned the lock: nothing that
/// changes these maps can panic half-way.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A transport error with its causes (connect, DNS, TLS, timeout), without
/// its URL and never a header (a key). A TLS name mismatch names the peer's
/// host, which the status shows anyway (`base_url`).
pub(crate) fn unreachable_err(e: reqwest::Error) -> PeerError {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    PeerError::Unreachable(text)
}

/// The whole body, refused once it passes `max` bytes.
async fn read_bounded(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, PeerError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(unreachable_err)? {
        if body.len() + chunk.len() > max {
            return Err(PeerError::BadResponse(format!(
                "the answer is over {max} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
