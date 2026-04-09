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
    /// Maps clip token (e.g. `"#song-name-a"`) to clip info.
    pub(crate) clip_mapping: HashMap<String, ClipInfo>,
    /// Tracks which lane is active per playlist (`false` = A, `true` = B).
    pub(crate) lane_state: HashMap<i64, bool>,
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
            lane_state: HashMap::new(),
            endpoint_cache: None,
        }
    }

    /// Main run loop: processes commands, periodically refreshes clip mapping,
    /// and shuts down on signal.
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<ResolumeCommand>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        // Initial mapping refresh.
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

    /// Handle a single command.
    async fn handle_command(&mut self, cmd: ResolumeCommand) {
        match cmd {
            ResolumeCommand::UpdateTitle {
                playlist_id,
                song,
                artist,
            } => {
                if let Err(e) = handlers::crossfade_title(self, playlist_id, &song, &artist).await {
                    warn!(host = %self.host, playlist_id, %e, "crossfade_title failed");
                }
            }
            ResolumeCommand::ClearTitle { playlist_id } => {
                if let Err(e) = handlers::clear_title(self, playlist_id).await {
                    warn!(host = %self.host, playlist_id, %e, "clear_title failed");
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
        let resp = self.apply_host_header(req, &ep).send().await?;
        let body: serde_json::Value = resp.json().await?;

        let new_mapping = parse_composition(&body);
        if new_mapping != self.clip_mapping {
            info!(
                host = %self.host,
                clips = new_mapping.len(),
                "updated Resolume clip mapping"
            );
            self.clip_mapping = new_mapping;
        }

        Ok(())
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

    /// Build a request with the Host header set if needed.
    fn apply_host_header(
        &self,
        builder: reqwest::RequestBuilder,
        ep: &ResolvedEndpoint,
    ) -> reqwest::RequestBuilder {
        if let Some(ref host) = ep.host_header {
            builder.header("Host", host)
        } else {
            builder
        }
    }

    /// Set text on a clip parameter.
    ///
    /// `PUT /api/v1/parameter/by-id/{param_id}`
    pub(crate) async fn set_text(
        &mut self,
        param_id: i64,
        text: &str,
    ) -> Result<(), anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
        let req = self
            .client
            .put(&url)
            .json(&serde_json::json!({ "value": text }));
        self.apply_host_header(req, &ep).send().await?;
        Ok(())
    }

    /// Trigger (connect) a clip.
    ///
    /// `POST /api/v1/composition/clips/by-id/{clip_id}/connect`
    pub(crate) async fn trigger_clip(&mut self, clip_id: i64) -> Result<(), anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!(
            "{}/api/v1/composition/clips/by-id/{clip_id}/connect",
            ep.base_url
        );
        let req = self.client.post(&url);
        self.apply_host_header(req, &ep).send().await?;
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
pub fn parse_composition(composition: &serde_json::Value) -> HashMap<String, ClipInfo> {
    let mut mapping = HashMap::new();

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
                mapping.insert(
                    token.to_string(),
                    ClipInfo {
                        clip_id,
                        text_param_id,
                    },
                );
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
        assert_eq!(song_a.clip_id, 100);
        assert_eq!(song_a.text_param_id, 200);

        let song_b = &mapping["#song-name-b"];
        assert_eq!(song_b.clip_id, 101);
        assert_eq!(song_b.text_param_id, 201);

        let artist_a = &mapping["#artist-name-a"];
        assert_eq!(artist_a.clip_id, 102);
        assert_eq!(artist_a.text_param_id, 202);

        let artist_b = &mapping["#artist-name-b"];
        assert_eq!(artist_b.clip_id, 103);
        assert_eq!(artist_b.text_param_id, 203);

        let clear = &mapping["#song-clear"];
        assert_eq!(clear.clip_id, 104);
        assert_eq!(clear.text_param_id, 204);
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
        assert_eq!(mapping["#tag-one"].clip_id, 50);
        assert_eq!(mapping["#tag-two"].clip_id, 50);
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
    fn clip_discovery_uses_param_text_valuetype() {
        let comp = serde_json::json!({
            "layers": [{
                "clips": [{
                    "id": 1683810383769_i64,
                    "name": { "value": "#spfast-title" },
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
        let clip = &mapping["#spfast-title"];
        assert_eq!(clip.clip_id, 1683810383769);
        assert_eq!(clip.text_param_id, 1775761488634);
    }

    #[test]
    fn host_driver_new() {
        let driver = HostDriver::new("192.168.1.10".to_string(), 8080);
        assert!(driver.clip_mapping.is_empty());
        assert!(driver.lane_state.is_empty());
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
}
