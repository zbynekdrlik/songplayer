//! Per-host Resolume Arena driver — connects to one Resolume instance
//! and manages clip discovery and command handling.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::resolume::ResolumeCommand;
use crate::resolume::handlers;

const RESOLUTION_TTL: Duration = Duration::from_secs(300); // 5 minutes

#[derive(Debug, Clone)]
pub(crate) struct ResolvedEndpoint {
    pub base_url: String,
    pub host_header: Option<String>,
    pub resolved_at: Instant,
}

impl ResolvedEndpoint {
    fn from_ip(ip: &str, port: u16) -> Self {
        Self {
            base_url: format!("http://{ip}:{port}"),
            host_header: None,
            resolved_at: Instant::now(),
        }
    }

    fn from_resolved(ip: &str, hostname: &str, port: u16) -> Self {
        Self {
            base_url: format!("http://{ip}:{port}"),
            host_header: Some(format!("{hostname}:{port}")),
            resolved_at: Instant::now(),
        }
    }

    /// Return `true` if the endpoint should be re-resolved.
    ///
    /// Thin wrapper around `is_expired_at(Instant::now())`. Skipped from
    /// mutation testing because every observable behavior of this wrapper
    /// is already covered by `is_expired_at` tests (which use synthetic
    /// clocks to hit the `>` boundary exactly). A `-> false` mutant on
    /// this wrapper cannot be caught without waiting `RESOLUTION_TTL`
    /// real-time seconds or backdating `resolved_at` (which underflows
    /// Windows' monotonic clock on freshly booted CI runners).
    #[cfg_attr(test, mutants::skip)]
    fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    /// Pure function form of `is_expired` — takes an explicit `now` parameter
    /// so tests can construct synthetic "future" clocks without subtracting
    /// a large duration from `Instant::now()` (which underflows on Windows
    /// CI runners where the monotonic clock starts near zero).
    fn is_expired_at(&self, now: Instant) -> bool {
        now.duration_since(self.resolved_at) > RESOLUTION_TTL
    }
}

fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Information about a discovered Resolume clip.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub clip_id: i64,
    pub text_param_id: i64,
}

/// Per-host worker that communicates with a single Resolume Arena instance.
pub struct HostDriver {
    host: String,
    port: u16,
    client: reqwest::Client,
    /// Maps clip token (e.g. `"#sp-title"`) to list of matching clips.
    /// A single token can appear in multiple clips across layers/columns/decks
    /// and all of them are updated in parallel.
    pub(crate) clip_mapping: HashMap<String, Vec<ClipInfo>>,
    /// Cached DNS resolution for hostname-based hosts.
    endpoint_cache: Option<ResolvedEndpoint>,
    /// Set true after a successful refresh, false after a failure.
    last_refresh_ok: bool,
    /// Wall-clock timestamp of the last completed refresh attempt
    /// (success or failure). `None` until first attempt.
    last_refresh_ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Number of consecutive refresh failures. Reset to 0 on success.
    consecutive_failures: u32,
    /// Whether the circuit breaker has tripped (≥30s of failures).
    circuit_breaker_open: bool,
    /// Set via `with_recovery_channel` builder; never accessed directly.
    recovery_tx: Option<tokio::sync::broadcast::Sender<crate::resolume::RecoveryEvent>>,
    /// Set via `with_health_channel` builder; never accessed directly.
    health_tx: Option<tokio::sync::watch::Sender<crate::resolume::HostHealthSnapshot>>,
}

impl HostDriver {
    pub fn new(host: String, port: u16) -> Self {
        Self {
            host,
            port,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build reqwest client"),
            clip_mapping: HashMap::new(),
            endpoint_cache: None,
            last_refresh_ok: false,
            last_refresh_ts: None,
            consecutive_failures: 0,
            circuit_breaker_open: false,
            recovery_tx: None,
            health_tx: None,
        }
    }

    pub fn with_health_channel(
        mut self,
        tx: tokio::sync::watch::Sender<crate::resolume::HostHealthSnapshot>,
    ) -> Self {
        self.health_tx = Some(tx);
        self
    }

    pub fn with_recovery_channel(
        mut self,
        tx: tokio::sync::broadcast::Sender<crate::resolume::RecoveryEvent>,
    ) -> Self {
        self.recovery_tx = Some(tx);
        self
    }

    /// Main run loop: processes commands, periodically refreshes clip mapping,
    /// and shuts down on signal.
    /// Top-level worker loop. Tested via integration / live verification on
    /// win-resolume rather than unit-mutation tests — the loop integrates
    /// tokio::select!, the refresh interval, and the command channel.
    #[cfg_attr(test, mutants::skip)]
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<ResolumeCommand>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        if let Err(e) = self.refresh_mapping().await {
            warn!(host = %self.host, %e, "initial clip mapping refresh failed");
        }

        let mut refresh_interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(cmd) = rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = refresh_interval.tick() => {
                    if let Err(e) = self.refresh_mapping().await {
                        debug!(host = %self.host, %e, "clip mapping refresh failed");
                    }
                }
                _ = shutdown.recv() => {
                    info!(host = %self.host, "Resolume driver shutting down");
                    break;
                }
            }
        }
    }

    /// Handle a single command. Pure dispatch to handlers — each branch is
    /// covered by wiremock tests in `handlers.rs`.
    #[cfg_attr(test, mutants::skip)]
    async fn handle_command(&mut self, cmd: ResolumeCommand) {
        match cmd {
            ResolumeCommand::ShowTitle { song, artist } => {
                if let Err(e) = handlers::show_title(self, &song, &artist).await {
                    warn!(host = %self.host, %e, "show_title failed");
                }
            }
            ResolumeCommand::HideTitle => {
                if let Err(e) = handlers::hide_title(self).await {
                    warn!(host = %self.host, %e, "hide_title failed");
                }
            }
            ResolumeCommand::ShowSubtitles {
                en,
                next_en,
                sk,
                next_sk,
                suppress_en,
            } => {
                if let Err(e) = handlers::set_subtitles(
                    self,
                    &en,
                    &next_en,
                    sk.as_deref(),
                    next_sk.as_deref(),
                    suppress_en,
                )
                .await
                {
                    warn!(host = %self.host, %e, "subtitle set failed");
                }
            }
            ResolumeCommand::HideSubtitles => {
                if let Err(e) = handlers::clear_subtitles(self).await {
                    warn!(host = %self.host, %e, "subtitle clear failed");
                }
            }
            ResolumeCommand::RefreshMapping => {
                if let Err(e) = self.refresh_mapping().await {
                    warn!(host = %self.host, %e, "refresh_mapping failed");
                }
            }
            ResolumeCommand::Shutdown => {
                info!(host = %self.host, "received shutdown command");
            }
        }
    }

    const FAIL_WARN_THRESHOLD: u32 = 2;
    const CIRCUIT_OPEN_THRESHOLD: u32 = 3;

    #[cfg_attr(test, mutants::skip)] // log-only diagnostic; CIRCUIT_OPEN_THRESHOLD covers system-critical boundary
    fn should_emit_repeated_failure_warn(failures: u32) -> bool {
        failures >= Self::FAIL_WARN_THRESHOLD
    }

    /// Fetch composition JSON from Resolume and build clip mapping from
    /// `#token` tags found in clip names.
    ///
    /// `GET /api/v1/composition`
    pub(crate) async fn refresh_mapping(&mut self) -> Result<(), anyhow::Error> {
        let result = self.fetch_mapping_inner().await;
        let outcome = match result {
            Ok(new_mapping) => {
                self.last_refresh_ok = true;
                self.last_refresh_ts = Some(chrono::Utc::now());
                let was_failing = self.consecutive_failures > 0;
                self.consecutive_failures = 0;
                if self.circuit_breaker_open {
                    self.circuit_breaker_open = false;
                    info!(host = %self.host, "circuit breaker closed — Resolume recovered");
                }
                if was_failing {
                    if let Some(tx) = &self.recovery_tx {
                        let _ = tx.send(crate::resolume::RecoveryEvent {
                            host: self.host.clone(),
                        });
                    }
                    info!(host = %self.host, "Resolume recovery — RecoveryEvent fired");
                }
                if new_mapping != self.clip_mapping {
                    let total: usize = new_mapping.values().map(|v| v.len()).sum();
                    info!(
                        host = %self.host,
                        tokens = new_mapping.len(),
                        clips = total,
                        "updated Resolume clip mapping"
                    );
                    self.clip_mapping = new_mapping;
                }
                Ok(())
            }
            Err(e) => {
                self.last_refresh_ok = false;
                self.last_refresh_ts = Some(chrono::Utc::now());
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if Self::should_emit_repeated_failure_warn(self.consecutive_failures) {
                    warn!(
                        host = %self.host,
                        consecutive_failures = self.consecutive_failures,
                        "Resolume refresh failing repeatedly"
                    );
                }
                if self.consecutive_failures >= Self::CIRCUIT_OPEN_THRESHOLD
                    && !self.circuit_breaker_open
                {
                    self.circuit_breaker_open = true;
                    self.clip_mapping = HashMap::new();
                    warn!(host = %self.host, "circuit breaker opened — clip cache evicted");
                }
                Err(e)
            }
        };
        if let Some(tx) = &self.health_tx {
            let snapshot = crate::resolume::HostHealthSnapshot {
                host: self.host.clone(),
                last_refresh_ts: self.last_refresh_ts,
                last_refresh_ok: self.last_refresh_ok,
                consecutive_failures: self.consecutive_failures,
                circuit_breaker_open: self.circuit_breaker_open,
                // Only SongPlayer-relevant tokens. The driver scans the
                // entire composition for `#`-prefixed names, but operators
                // have many of their own tokens (#bible-*, #timer,
                // #translate-*-u-re, etc.) that are noise to this dashboard.
                clips_by_token: [
                    crate::resolume::TITLE_TOKEN,
                    crate::resolume::SUBS_TOKEN,
                    crate::resolume::SUBS_NEXT_TOKEN,
                    crate::resolume::SUBS_SK_TOKEN,
                ]
                .iter()
                .map(|t| {
                    (
                        (*t).to_string(),
                        self.clip_mapping.get(*t).map(|v| v.len()).unwrap_or(0),
                    )
                })
                .collect(),
            };
            let _ = tx.send(snapshot);
        }
        outcome
    }

    async fn fetch_mapping_inner(
        &mut self,
    ) -> Result<HashMap<String, Vec<ClipInfo>>, anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/composition", ep.base_url);
        let req = self.client.get(&url);
        let resp = Self::apply_host_header(req, &ep).send().await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(parse_composition(&body))
    }

    /// Ensure the endpoint cache is populated. Call before parallel operations
    /// that need to use `set_text`/`set_clip_opacity` concurrently via `&self`.
    pub(crate) async fn ensure_endpoint(&mut self) -> Result<(), anyhow::Error> {
        let _ = self.endpoint().await?;
        Ok(())
    }

    /// Get the cached endpoint (must call `ensure_endpoint` first).
    fn cached_endpoint(&self) -> Option<&ResolvedEndpoint> {
        self.endpoint_cache.as_ref().filter(|ep| !ep.is_expired())
    }

    /// Resolve the host to an endpoint, caching the result for 5 minutes.
    /// For IP literals, no DNS lookup is needed. For hostnames, we resolve
    /// via DNS and store the IP in the URL with the original hostname in the
    /// Host header (required by Resolume when addressed by hostname).
    async fn endpoint(&mut self) -> Result<ResolvedEndpoint, anyhow::Error> {
        if let Some(ref cached) = self.endpoint_cache {
            if !cached.is_expired() {
                return Ok(cached.clone());
            }
        }
        let ep = if is_ip_literal(&self.host) {
            ResolvedEndpoint::from_ip(&self.host, self.port)
        } else {
            let lookup = format!("{}:{}", self.host, self.port);
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host(&lookup).await?.collect();
            let addr = addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(addrs.first())
                .ok_or_else(|| {
                    anyhow::anyhow!("DNS lookup returned no addresses for {}", self.host)
                })?;
            ResolvedEndpoint::from_resolved(&addr.ip().to_string(), &self.host, self.port)
        };
        self.endpoint_cache = Some(ep.clone());
        Ok(ep)
    }

    /// Build a request with the `Host` header set if the endpoint requires it.
    ///
    /// Uses the typed `reqwest::header::HOST` constant which reqwest/hyper
    /// treats as a replacement for the auto-generated Host header derived from
    /// the URL authority. Passing the header as a raw string `"Host"` would
    /// append rather than replace, leading to undefined behavior.
    fn apply_host_header(
        builder: reqwest::RequestBuilder,
        ep: &ResolvedEndpoint,
    ) -> reqwest::RequestBuilder {
        if let Some(ref host) = ep.host_header {
            builder.header(reqwest::header::HOST, host)
        } else {
            builder
        }
    }

    /// Set text on a clip parameter.
    ///
    /// `PUT /api/v1/parameter/by-id/{param_id}`
    ///
    /// Takes `&self` so multiple calls can be driven in parallel via
    /// `FuturesUnordered`. Caller MUST have called `ensure_endpoint` first.
    pub(crate) async fn set_text(&self, param_id: i64, text: &str) -> Result<(), anyhow::Error> {
        let ep = self
            .cached_endpoint()
            .ok_or_else(|| anyhow::anyhow!("endpoint cache empty - call ensure_endpoint first"))?
            .clone();
        let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
        let req = self
            .client
            .put(&url)
            .json(&serde_json::json!({ "value": text }));
        Self::apply_host_header(req, &ep)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Set the opacity of a clip.
    ///
    /// `PUT /api/v1/composition/clips/by-id/{clip_id}`
    ///
    /// Takes `&self` so multiple calls can be driven in parallel via
    /// `FuturesUnordered`. Caller MUST have called `ensure_endpoint` first.
    pub(crate) async fn set_clip_opacity(
        &self,
        clip_id: i64,
        opacity: f64,
    ) -> Result<(), anyhow::Error> {
        let ep = self
            .cached_endpoint()
            .ok_or_else(|| anyhow::anyhow!("endpoint cache empty - call ensure_endpoint first"))?
            .clone();
        let url = format!("{}/api/v1/composition/clips/by-id/{clip_id}", ep.base_url);
        let req = self
            .client
            .put(&url)
            .json(&serde_json::json!({"video":{"opacity":{"value": opacity}}}));
        Self::apply_host_header(req, &ep)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Extract the text parameter ID from a clip by scanning `video.sourceparams`
/// for the entry with `"valuetype": "ParamText"`.
///
/// Resolume Arena versions use different key names (`Text`, `Text1`, etc.)
/// so we cannot rely on a fixed key — instead we match on `valuetype`.
fn extract_text_param_id(clip: &serde_json::Value) -> Option<i64> {
    let params = clip["video"]["sourceparams"].as_object()?;
    for (_key, param) in params {
        if param["valuetype"].as_str() == Some("ParamText") {
            return param["id"].as_i64();
        }
    }
    None
}

/// Parse a Resolume composition JSON and extract clip tokens.
///
/// Scans `layers[].clips[].name.value` for words starting with `#`. Each
/// token is mapped to the clip's ID and the text source parameter ID
/// (found by scanning `sourceparams` for `valuetype == "ParamText"`).
pub fn parse_composition(composition: &serde_json::Value) -> HashMap<String, Vec<ClipInfo>> {
    let mut mapping: HashMap<String, Vec<ClipInfo>> = HashMap::new();

    let layers = match composition["layers"].as_array() {
        Some(l) => l,
        None => return mapping,
    };

    for layer in layers {
        let clips = match layer["clips"].as_array() {
            Some(c) => c,
            None => continue,
        };

        for clip in clips {
            let clip_id = match clip["id"].as_i64() {
                Some(id) => id,
                None => continue,
            };

            let name = match clip["name"]["value"].as_str() {
                Some(n) => n,
                None => continue,
            };

            // Extract #tokens from the name.
            let tokens: Vec<&str> = name
                .split_whitespace()
                .filter(|w| w.starts_with('#'))
                .collect();

            if tokens.is_empty() {
                continue;
            }

            // Find the text source parameter ID by valuetype scan.
            let text_param_id = match extract_text_param_id(clip) {
                Some(id) => id,
                None => continue,
            };

            for token in tokens {
                mapping
                    .entry(token.to_string())
                    .or_default()
                    .push(ClipInfo {
                        clip_id,
                        text_param_id,
                    });
            }
        }
    }

    mapping
}

// Implement PartialEq for ClipInfo so we can compare mappings.
impl PartialEq for ClipInfo {
    fn eq(&self, other: &Self) -> bool {
        self.clip_id == other.clip_id && self.text_param_id == other.text_param_id
    }
}

impl Eq for ClipInfo {}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
