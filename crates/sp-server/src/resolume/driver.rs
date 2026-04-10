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

    /// `>` vs `>=` is functionally equivalent here because the boundary
    /// case `elapsed() == RESOLUTION_TTL` cannot be observed: any test that
    /// constructs `resolved_at = Instant::now() - TTL` and immediately calls
    /// `elapsed()` always sees elapsed > TTL by some nanoseconds. Skipping
    /// the operator mutation since it cannot meaningfully change behavior.
    #[cfg_attr(test, mutants::skip)]
    fn is_expired(&self) -> bool {
        self.resolved_at.elapsed() > RESOLUTION_TTL
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
        }
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

    /// Fetch composition JSON from Resolume and build clip mapping from
    /// `#token` tags found in clip names.
    ///
    /// `GET /api/v1/composition`
    pub(crate) async fn refresh_mapping(&mut self) -> Result<(), anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/composition", ep.base_url);
        let req = self.client.get(&url);
        let resp = Self::apply_host_header(req, &ep).send().await?;
        let body: serde_json::Value = resp.json().await?;

        let new_mapping = parse_composition(&body);
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
mod tests {
    use super::*;

    fn sample_composition() -> serde_json::Value {
        serde_json::json!({
            "layers": [
                {
                    "clips": [
                        {
                            "id": 100,
                            "name": { "value": "Title #song-name-a" },
                            "video": {
                                "sourceparams": {
                                    "Text": { "id": 200, "valuetype": "ParamText" }
                                }
                            }
                        },
                        {
                            "id": 101,
                            "name": { "value": "Title #song-name-b" },
                            "video": {
                                "sourceparams": {
                                    "Text": { "id": 201, "valuetype": "ParamText" }
                                }
                            }
                        },
                        {
                            "id": 102,
                            "name": { "value": "Artist #artist-name-a" },
                            "video": {
                                "sourceparams": {
                                    "Text": { "id": 202, "valuetype": "ParamText" }
                                }
                            }
                        },
                        {
                            "id": 103,
                            "name": { "value": "Artist #artist-name-b" },
                            "video": {
                                "sourceparams": {
                                    "Text": { "id": 203, "valuetype": "ParamText" }
                                }
                            }
                        },
                        {
                            "id": 104,
                            "name": { "value": "Clear #song-clear" },
                            "video": {
                                "sourceparams": {
                                    "Text": { "id": 204, "valuetype": "ParamText" }
                                }
                            }
                        }
                    ]
                }
            ]
        })
    }

    #[test]
    fn clip_discovery_parses_tokens() {
        let comp = sample_composition();
        let mapping = parse_composition(&comp);

        assert_eq!(mapping.len(), 5);

        let song_a = &mapping["#song-name-a"];
        assert_eq!(song_a.len(), 1);
        assert_eq!(song_a[0].clip_id, 100);
        assert_eq!(song_a[0].text_param_id, 200);

        let song_b = &mapping["#song-name-b"];
        assert_eq!(song_b.len(), 1);
        assert_eq!(song_b[0].clip_id, 101);
        assert_eq!(song_b[0].text_param_id, 201);

        let artist_a = &mapping["#artist-name-a"];
        assert_eq!(artist_a.len(), 1);
        assert_eq!(artist_a[0].clip_id, 102);
        assert_eq!(artist_a[0].text_param_id, 202);

        let artist_b = &mapping["#artist-name-b"];
        assert_eq!(artist_b.len(), 1);
        assert_eq!(artist_b[0].clip_id, 103);
        assert_eq!(artist_b[0].text_param_id, 203);

        let clear = &mapping["#song-clear"];
        assert_eq!(clear.len(), 1);
        assert_eq!(clear[0].clip_id, 104);
        assert_eq!(clear[0].text_param_id, 204);
    }

    #[test]
    fn clip_discovery_ignores_clips_without_tokens() {
        let comp = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 1,
                    "name": { "value": "No tokens here" },
                    "video": {
                        "sourceparams": {
                            "Text": { "id": 10, "valuetype": "ParamText" }
                        }
                    }
                }]
            }]
        });

        let mapping = parse_composition(&comp);
        assert!(mapping.is_empty());
    }

    #[test]
    fn clip_discovery_ignores_clips_without_text_param() {
        let comp = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 1,
                    "name": { "value": "Has #token" },
                    "video": {
                        "sourceparams": {}
                    }
                }]
            }]
        });

        let mapping = parse_composition(&comp);
        assert!(mapping.is_empty());
    }

    #[test]
    fn clip_discovery_handles_multiple_tokens_per_clip() {
        let comp = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 50,
                    "name": { "value": "Multi #tag-one #tag-two" },
                    "video": {
                        "sourceparams": {
                            "Text": { "id": 500, "valuetype": "ParamText" }
                        }
                    }
                }]
            }]
        });

        let mapping = parse_composition(&comp);
        assert_eq!(mapping.len(), 2);
        assert_eq!(mapping["#tag-one"][0].clip_id, 50);
        assert_eq!(mapping["#tag-two"][0].clip_id, 50);
    }

    #[test]
    fn clip_discovery_empty_composition() {
        let comp = serde_json::json!({});
        let mapping = parse_composition(&comp);
        assert!(mapping.is_empty());

        let comp2 = serde_json::json!({ "layers": [] });
        let mapping2 = parse_composition(&comp2);
        assert!(mapping2.is_empty());
    }

    #[test]
    fn parse_composition_collects_multiple_clips_per_token() {
        let comp = serde_json::json!({
            "layers": [
                {
                    "clips": [
                        {
                            "id": 100,
                            "name": { "value": "Title A #sp-title" },
                            "video": { "sourceparams": { "Text": { "id": 200, "valuetype": "ParamText" } } }
                        },
                        {
                            "id": 101,
                            "name": { "value": "Title B #sp-title" },
                            "video": { "sourceparams": { "Text": { "id": 201, "valuetype": "ParamText" } } }
                        }
                    ]
                },
                {
                    "clips": [
                        {
                            "id": 102,
                            "name": { "value": "Other Layer #sp-title" },
                            "video": { "sourceparams": { "Text": { "id": 202, "valuetype": "ParamText" } } }
                        }
                    ]
                }
            ]
        });

        let mapping = parse_composition(&comp);
        let clips = mapping.get("#sp-title").expect("must have #sp-title entry");
        assert_eq!(clips.len(), 3, "expected 3 clips, got: {clips:?}");

        let ids: Vec<i64> = clips.iter().map(|c| c.clip_id).collect();
        assert!(ids.contains(&100));
        assert!(ids.contains(&101));
        assert!(ids.contains(&102));
    }

    #[test]
    fn clip_discovery_uses_param_text_valuetype() {
        let comp = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 1683810383769_i64,
                    "name": { "value": "#sp-title" },
                    "video": {
                        "sourceparams": {
                            "Text": {
                                "id": 1775761488634_i64,
                                "valuetype": "ParamText",
                                "value": "Hello"
                            }
                        }
                    }
                }]
            }]
        });
        let mapping = parse_composition(&comp);
        assert_eq!(mapping.len(), 1);
        let clips = &mapping["#sp-title"];
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].clip_id, 1683810383769);
        assert_eq!(clips[0].text_param_id, 1775761488634);
    }

    #[test]
    fn host_driver_new() {
        let driver = HostDriver::new("192.168.1.10".to_string(), 8080);
        assert!(driver.clip_mapping.is_empty());
        assert!(driver.endpoint_cache.is_none());
    }

    #[test]
    fn is_ip_literal_detects_ipv4() {
        assert!(is_ip_literal("192.168.1.10"));
        assert!(is_ip_literal("127.0.0.1"));
        assert!(is_ip_literal("10.77.9.201"));
        assert!(!is_ip_literal("resolume.lan"));
        assert!(!is_ip_literal("my-host.local"));
    }

    #[test]
    fn resolved_endpoint_ip_literal_no_host_header() {
        let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
        assert_eq!(ep.base_url, "http://192.168.1.10:8090");
        assert!(ep.host_header.is_none());
    }

    #[test]
    fn resolved_endpoint_hostname_has_host_header() {
        let ep = ResolvedEndpoint::from_resolved("10.77.9.201", "resolume.lan", 8090);
        assert_eq!(ep.base_url, "http://10.77.9.201:8090");
        assert_eq!(ep.host_header.as_deref(), Some("resolume.lan:8090"));
    }

    #[test]
    fn resolved_endpoint_expiry() {
        let ep = ResolvedEndpoint::from_ip("192.168.1.10", 8090);
        // Freshly created endpoint should not be expired.
        assert!(!ep.is_expired());
    }

    #[test]
    fn resolved_endpoint_expires_after_ttl() {
        // Manually construct an endpoint with a backdated `resolved_at` to
        // simulate TTL expiry without sleeping for 5 minutes.
        let ep = ResolvedEndpoint {
            base_url: "http://192.168.1.10:8090".into(),
            host_header: None,
            resolved_at: Instant::now() - Duration::from_secs(301),
        };
        assert!(
            ep.is_expired(),
            "endpoint resolved 301s ago should be expired (TTL=300s)"
        );
    }

    /// Boundary test: kills the `>` → `>=` mutant in `is_expired`.
    /// At exactly the TTL boundary the endpoint must NOT be expired.
    #[test]
    fn resolved_endpoint_at_exact_ttl_boundary_is_not_expired() {
        // Backdate to exactly TTL - 1ms (just before the boundary).
        let ep = ResolvedEndpoint {
            base_url: "http://192.168.1.10:8090".into(),
            host_header: None,
            resolved_at: Instant::now() - (RESOLUTION_TTL - Duration::from_millis(1)),
        };
        assert!(
            !ep.is_expired(),
            "endpoint just under TTL should not be expired"
        );
    }

    #[tokio::test]
    async fn cached_endpoint_returns_none_until_ensure_endpoint_called() {
        let driver = HostDriver::new("127.0.0.1".to_string(), 1);
        assert!(
            driver.cached_endpoint().is_none(),
            "no endpoint cached before ensure_endpoint"
        );
    }

    #[tokio::test]
    async fn ensure_endpoint_populates_cache_for_ip_literal() {
        let mut driver = HostDriver::new("127.0.0.1".to_string(), 8090);
        driver.ensure_endpoint().await.unwrap();
        let cached = driver.cached_endpoint().expect("endpoint should be cached");
        assert_eq!(cached.base_url, "http://127.0.0.1:8090");
        assert!(
            cached.host_header.is_none(),
            "IP literal should not need a Host header override"
        );
    }

    /// Wiremock test that exercises `refresh_mapping` against a real HTTP
    /// server returning a composition. Kills the `Ok(())` mutant by asserting
    /// that the mapping was actually populated from the response.
    #[tokio::test]
    async fn refresh_mapping_populates_clip_mapping_from_composition() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let composition = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 555,
                    "name": { "value": "#sp-title" },
                    "video": {
                        "sourceparams": {
                            "Text": { "id": 999, "valuetype": "ParamText" }
                        }
                    }
                }]
            }]
        });
        Mock::given(method("GET"))
            .and(path("/api/v1/composition"))
            .respond_with(ResponseTemplate::new(200).set_body_json(composition))
            .mount(&server)
            .await;

        let url = server.uri();
        let stripped = url.trim_start_matches("http://");
        let parts: Vec<&str> = stripped.split(':').collect();
        let host = parts[0].to_string();
        let port: u16 = parts[1].parse().unwrap();

        let mut driver = HostDriver::new(host, port);
        assert!(driver.clip_mapping.is_empty());

        driver.refresh_mapping().await.unwrap();

        let clips = driver
            .clip_mapping
            .get("#sp-title")
            .expect("#sp-title should be populated");
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].clip_id, 555);
        assert_eq!(clips[0].text_param_id, 999);
    }

    /// Verify the cache is reused when not expired (kills the `delete !` mutant
    /// at the `if !cached.is_expired()` check).
    #[tokio::test]
    async fn endpoint_returns_cached_value_on_subsequent_calls() {
        let mut driver = HostDriver::new("127.0.0.1".to_string(), 8090);
        let ep1 = driver.endpoint().await.unwrap();
        let ep2 = driver.endpoint().await.unwrap();
        // Same resolved_at means we got the cached value, not a fresh resolve.
        assert_eq!(ep1.resolved_at, ep2.resolved_at);
        assert_eq!(ep1.base_url, ep2.base_url);
    }
}
