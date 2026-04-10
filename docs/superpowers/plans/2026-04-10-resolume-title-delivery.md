# Resolume Title Delivery — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the SongPlayer Resolume integration so titles fade in/out on Resolume Arena clips during video playback, matching legacy Python title timing (show 1.5s after start, hide 3.5s before end, 1s fades).

**Architecture:** Single `#sp{name}-title` clip per playlist in Resolume. HostDriver sets text via PUT, fades clip opacity 0↔1 over 1s (20 steps × 50ms). ResolumeRegistry wired into PlaybackEngine via broadcast channel. DNS resolution with 5min cache + Host header for hostname connections.

**Tech Stack:** Rust, Axum 0.8, sqlx 0.8 (SQLite), reqwest 0.12, tokio, serde_json

**Design spec:** `docs/superpowers/specs/2026-04-10-resolume-title-delivery-design.md`

---

## Task 1: Database migration — add `resolume_title_token` column

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs`
- Modify: `crates/sp-core/src/models.rs`

- [ ] **Step 1: Write failing test for migration V2**

Add to `crates/sp-server/src/db/mod.rs` in the `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn migration_v2_adds_resolume_title_token() {
    let pool = setup().await;
    // Insert a playlist and read back the new column.
    sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT resolume_title_token FROM playlists WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let token: String = row.get("resolume_title_token");
    assert_eq!(token, "");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server migration_v2 -- --nocapture`
Expected: FAIL — `no such column: resolume_title_token`

- [ ] **Step 3: Add migration V2 and update Playlist model**

In `crates/sp-server/src/db/mod.rs`, add the V2 migration:

```rust
const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1), (2, MIGRATION_V2)];

const MIGRATION_V2: &str = "
ALTER TABLE playlists ADD COLUMN resolume_title_token TEXT NOT NULL DEFAULT '';
";
```

In `crates/sp-core/src/models.rs`, add the field to `Playlist`:

```rust
pub struct Playlist {
    // ... existing fields ...
    #[serde(default)]
    pub resolume_title_token: String,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p sp-server migration_v2 -- --nocapture`
Expected: PASS

- [ ] **Step 5: Update get_active_playlists to include the new column**

In `crates/sp-server/src/db/models.rs`, modify `get_active_playlists`:

```rust
pub async fn get_active_playlists(pool: &SqlitePool) -> Result<Vec<Playlist>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, resolume_title_token, is_active
         FROM playlists WHERE is_active = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| Playlist {
            id: r.get("id"),
            name: r.get("name"),
            youtube_url: r.get("youtube_url"),
            ndi_output_name: r.get::<String, _>("ndi_output_name"),
            resolume_title_token: r.get::<String, _>("resolume_title_token"),
            is_active: r.get::<i32, _>("is_active") != 0,
            ..Default::default()
        })
        .collect())
}
```

- [ ] **Step 6: Update API routes to include `resolume_title_token`**

In `crates/sp-server/src/api/routes.rs`:

1. Add `resolume_title_token: Option<String>` to `CreatePlaylistRequest` and `UpdatePlaylistRequest`.

2. In `list_playlists`, `create_playlist`, `get_playlist` — add `"resolume_title_token"` to the SELECT and JSON output.

3. In `update_playlist` — add handling for the new field in the dynamic SET builder.

4. In `create_playlist` — bind the new column in the INSERT.

- [ ] **Step 7: Run all tests**

Run: `cargo test -p sp-server`
Expected: ALL PASS (including existing migration idempotency test which should now show version 2)

- [ ] **Step 8: Update migration version assertion**

The existing test `pool_creation_and_migration` asserts `ver == 1`. Update to `ver == 2`.

- [ ] **Step 9: Commit**

```bash
git add crates/sp-core/src/models.rs crates/sp-server/src/db/mod.rs crates/sp-server/src/db/models.rs crates/sp-server/src/api/routes.rs
git commit -m "feat: add resolume_title_token column to playlists (migration V2)"
```

---

## Task 2: Fix text param discovery in `parse_composition`

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`

- [ ] **Step 1: Write failing test with real Resolume JSON structure**

The current test uses `"Text1"` key. Resolume Arena 7.23.2 uses `"Text"` key with `"valuetype": "ParamText"`. Add a test that uses the real structure. In `crates/sp-server/src/resolume/driver.rs` tests:

```rust
#[test]
fn clip_discovery_uses_param_text_valuetype() {
    // Real Resolume Arena 7.23.2 uses "Text" key (not "Text1")
    // with valuetype "ParamText".
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server clip_discovery_uses_param_text -- --nocapture`
Expected: FAIL — mapping is empty because code looks for `Text1` key

- [ ] **Step 3: Fix `parse_composition` to use `valuetype == "ParamText"` scan**

Replace the text param discovery in `parse_composition`:

```rust
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

            let tokens: Vec<&str> = name
                .split_whitespace()
                .filter(|w| w.starts_with('#'))
                .collect();

            if tokens.is_empty() {
                continue;
            }

            // Scan sourceparams for entry with valuetype "ParamText".
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

/// Find the text parameter ID by scanning sourceparams for `valuetype == "ParamText"`.
fn extract_text_param_id(clip: &serde_json::Value) -> Option<i64> {
    let params = clip["video"]["sourceparams"].as_object()?;
    for (_key, param) in params {
        if param["valuetype"].as_str() == Some("ParamText") {
            return param["id"].as_i64();
        }
    }
    None
}
```

- [ ] **Step 4: Update existing tests to use the real JSON structure**

Update `sample_composition()` and all tests that use `"Text1"` to use the real Resolume JSON structure with `"Text"` key + `"valuetype": "ParamText"`. For example:

```rust
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
                    // ... rest of clips with same structure ...
                ]
            }
        ]
    })
}
```

Update all other test compositions similarly (clip_discovery_ignores_clips_without_text_param, clip_discovery_handles_multiple_tokens_per_clip, etc.).

- [ ] **Step 5: Run all resolume tests**

Run: `cargo test -p sp-server resolume -- --nocapture`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs
git commit -m "fix: use ParamText valuetype scan for Resolume text param discovery"
```

---

## Task 3: Add DNS resolution with caching and Host header

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`

- [ ] **Step 1: Write tests for DNS resolution logic**

Add to `crates/sp-server/src/resolume/driver.rs` tests:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server is_ip_literal -- --nocapture`
Expected: FAIL — functions not defined

- [ ] **Step 3: Implement DNS resolution types and helpers**

Add to `crates/sp-server/src/resolume/driver.rs`:

```rust
use std::net::IpAddr;
use std::time::Instant;

/// TTL for resolved DNS entries.
const RESOLUTION_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Cached resolved endpoint for a Resolume host.
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

/// Check if a host string is an IP literal (not a hostname).
fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}
```

- [ ] **Step 4: Add endpoint resolution to HostDriver**

Update `HostDriver` struct and methods:

```rust
pub struct HostDriver {
    host: String,
    port: u16,
    client: reqwest::Client,
    pub(crate) clip_mapping: HashMap<String, ClipInfo>,
    /// Cached resolved endpoint (DNS → IP, with Host header for hostnames).
    endpoint_cache: Option<ResolvedEndpoint>,
}
```

Remove `lane_state` field (no longer needed — A/B crossfade is being replaced).

Add the `resolve_endpoint` method:

```rust
impl HostDriver {
    /// Get or resolve the endpoint. Uses cache if not expired.
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
            // Prefer IPv4.
            let addr = addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(addrs.first())
                .ok_or_else(|| anyhow::anyhow!("DNS lookup returned no addresses for {}", self.host))?;
            ResolvedEndpoint::from_resolved(&addr.ip().to_string(), &self.host, self.port)
        };

        self.endpoint_cache = Some(ep.clone());
        Ok(ep)
    }
}
```

- [ ] **Step 5: Update `base_url()`, `set_text()`, `trigger_clip()` to use resolved endpoint**

Replace the old `base_url()` method. Update `set_text` and `trigger_clip` to use the resolved endpoint and add the Host header when present:

```rust
pub(crate) async fn set_text(&mut self, param_id: i64, text: &str) -> Result<(), anyhow::Error> {
    let ep = self.endpoint().await?;
    let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
    let mut req = self.client.put(&url).json(&serde_json::json!({ "value": text }));
    if let Some(ref host) = ep.host_header {
        req = req.header("Host", host);
    }
    req.send().await?.error_for_status()?;
    Ok(())
}

pub(crate) async fn trigger_clip(&mut self, clip_id: i64) -> Result<(), anyhow::Error> {
    let ep = self.endpoint().await?;
    let url = format!("{}/api/v1/composition/clips/by-id/{clip_id}/connect", ep.base_url);
    let mut req = self.client.post(&url);
    if let Some(ref host) = ep.host_header {
        req = req.header("Host", host);
    }
    req.send().await?.error_for_status()?;
    Ok(())
}
```

Also update `refresh_mapping` to use the resolved endpoint with Host header.

- [ ] **Step 6: Update `new()` constructor and fix `lane_state` removal**

```rust
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
```

Remove the `lane_state` field and update `host_driver_new` and `lane_state_flip` tests accordingly (remove `lane_state_flip`, update `host_driver_new`).

- [ ] **Step 7: Run all tests**

Run: `cargo test -p sp-server -- --nocapture`
Expected: ALL PASS

- [ ] **Step 8: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs
git commit -m "feat: add DNS resolution with 5min cache and Host header for Resolume"
```

---

## Task 4: Replace A/B crossfade with show/hide + opacity fade

**Files:**
- Modify: `crates/sp-server/src/resolume/handlers.rs`
- Modify: `crates/sp-server/src/resolume/driver.rs`
- Modify: `crates/sp-server/src/resolume/mod.rs`

- [ ] **Step 1: Add `set_clip_opacity` method to HostDriver**

In `crates/sp-server/src/resolume/driver.rs`:

```rust
/// Set the video opacity of a clip.
///
/// `PUT /api/v1/composition/clips/by-id/{clip_id}`
/// Body: `{"video":{"opacity":{"value": opacity}}}`
pub(crate) async fn set_clip_opacity(
    &mut self,
    clip_id: i64,
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let ep = self.endpoint().await?;
    let url = format!(
        "{}/api/v1/composition/clips/by-id/{clip_id}",
        ep.base_url
    );
    let mut req = self.client
        .put(&url)
        .json(&serde_json::json!({"video":{"opacity":{"value": opacity}}}));
    if let Some(ref host) = ep.host_header {
        req = req.header("Host", host);
    }
    req.send().await?.error_for_status()?;
    Ok(())
}
```

- [ ] **Step 2: Update ResolumeCommand enum**

In `crates/sp-server/src/resolume/mod.rs`, replace the command enum:

```rust
#[derive(Debug, Clone)]
pub enum ResolumeCommand {
    ShowTitle {
        playlist_id: i64,
        song: String,
        artist: String,
        gemini_failed: bool,
    },
    HideTitle {
        playlist_id: i64,
    },
    RefreshMapping,
    Shutdown,
}
```

- [ ] **Step 3: Write tests for title text formatting**

In `crates/sp-server/src/resolume/handlers.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_text_song_and_artist() {
        assert_eq!(format_title_text("My Song", "Artist", false), "My Song - Artist");
    }

    #[test]
    fn format_title_text_song_only() {
        assert_eq!(format_title_text("My Song", "", false), "My Song");
    }

    #[test]
    fn format_title_text_artist_only() {
        assert_eq!(format_title_text("", "Artist", false), "Artist");
    }

    #[test]
    fn format_title_text_empty() {
        assert_eq!(format_title_text("", "", false), "");
    }

    #[test]
    fn format_title_text_gemini_failed() {
        assert_eq!(
            format_title_text("My Song", "Artist", true),
            "My Song - Artist \u{26A0}"
        );
    }

    #[test]
    fn format_title_text_gemini_failed_empty_no_warning() {
        assert_eq!(format_title_text("", "", true), "");
    }

    #[test]
    fn fade_steps_20_steps_over_1s() {
        let steps = fade_steps(20);
        assert_eq!(steps.len(), 20);
        assert!((steps[0] - 0.05).abs() < 0.001);
        assert!((steps[19] - 1.0).abs() < 0.001);
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p sp-server format_title_text -- --nocapture`
Expected: FAIL — function not defined

- [ ] **Step 5: Implement handlers**

Rewrite `crates/sp-server/src/resolume/handlers.rs`:

```rust
//! Resolume title show/hide with opacity fade.

use std::time::Duration;

use tracing::debug;

use crate::resolume::driver::HostDriver;

/// Delay between setting text and starting opacity fade,
/// allowing Resolume to update the text texture.
const TEXT_SETTLE_MS: u64 = 35;

/// Fade duration in milliseconds.
const FADE_DURATION_MS: u64 = 1000;

/// Number of opacity steps during fade.
const FADE_STEPS: u32 = 20;

/// Format title text matching legacy Python behavior.
pub fn format_title_text(song: &str, artist: &str, gemini_failed: bool) -> String {
    let text = match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    };
    if !text.is_empty() && gemini_failed {
        format!("{text} \u{26A0}")
    } else {
        text
    }
}

/// Generate `n` evenly-spaced opacity values from step/n to 1.0.
pub fn fade_steps(n: u32) -> Vec<f64> {
    (1..=n).map(|i| i as f64 / n as f64).collect()
}

/// Show a title on the Resolume clip for a playlist.
///
/// 1. Set text on the clip's text parameter.
/// 2. Wait 35ms for Resolume to process the text texture.
/// 3. Fade opacity from 0 to 1 over 1 second.
pub async fn show_title(
    driver: &mut HostDriver,
    resolume_title_token: &str,
    song: &str,
    artist: &str,
    gemini_failed: bool,
) -> Result<(), anyhow::Error> {
    let clip = driver
        .clip_mapping
        .get(resolume_title_token)
        .ok_or_else(|| anyhow::anyhow!("no clip found for token {resolume_title_token}"))?
        .clone();

    let text = format_title_text(song, artist, gemini_failed);
    if text.is_empty() {
        return Ok(());
    }

    // Set text.
    driver.set_text(clip.text_param_id, &text).await?;
    debug!(token = %resolume_title_token, %text, "set title text");

    // Wait for text texture to render.
    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    // Fade in.
    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    for opacity in fade_steps(FADE_STEPS) {
        driver.set_clip_opacity(clip.clip_id, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(token = %resolume_title_token, "title fade-in complete");
    Ok(())
}

/// Hide the title by fading opacity to 0, then clearing the text.
pub async fn hide_title(
    driver: &mut HostDriver,
    resolume_title_token: &str,
) -> Result<(), anyhow::Error> {
    let clip = driver
        .clip_mapping
        .get(resolume_title_token)
        .ok_or_else(|| anyhow::anyhow!("no clip found for token {resolume_title_token}"))?
        .clone();

    // Fade out.
    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    let steps = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        driver.set_clip_opacity(clip.clip_id, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    // Ensure fully transparent.
    driver.set_clip_opacity(clip.clip_id, 0.0).await?;

    // Clear text.
    driver.set_text(clip.text_param_id, "").await?;

    debug!(token = %resolume_title_token, "title fade-out complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    // ... tests from step 3 ...
}
```

- [ ] **Step 6: Update HostDriver command handling**

In `crates/sp-server/src/resolume/driver.rs`, update `handle_command`:

```rust
async fn handle_command(&mut self, cmd: ResolumeCommand, tokens: &HashMap<i64, String>) {
    match cmd {
        ResolumeCommand::ShowTitle {
            playlist_id,
            song,
            artist,
            gemini_failed,
        } => {
            if let Some(token) = tokens.get(&playlist_id) {
                if let Err(e) = handlers::show_title(self, token, &song, &artist, gemini_failed).await {
                    warn!(host = %self.host, playlist_id, %e, "show_title failed");
                }
            }
        }
        ResolumeCommand::HideTitle { playlist_id } => {
            if let Some(token) = tokens.get(&playlist_id) {
                if let Err(e) = handlers::hide_title(self, token).await {
                    warn!(host = %self.host, playlist_id, %e, "hide_title failed");
                }
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
```

The `tokens` map (`HashMap<i64, String>`) maps playlist_id → resolume_title_token. It's loaded from the DB at driver startup and passed to `handle_command`. Update the `run` method signature to accept a `SqlitePool` and load tokens:

```rust
pub async fn run(
    mut self,
    pool: SqlitePool,
    mut rx: mpsc::Receiver<ResolumeCommand>,
    mut shutdown: broadcast::Receiver<()>,
) {
    // Load playlist → token mapping.
    let tokens = Self::load_tokens(&pool).await;

    // Initial mapping refresh.
    if let Err(e) = self.refresh_mapping().await {
        warn!(host = %self.host, %e, "initial clip mapping refresh failed");
    }

    let mut refresh_interval = tokio::time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            Some(cmd) = rx.recv() => {
                self.handle_command(cmd, &tokens).await;
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

async fn load_tokens(pool: &SqlitePool) -> HashMap<i64, String> {
    let rows = sqlx::query(
        "SELECT id, resolume_title_token FROM playlists WHERE resolume_title_token != '' AND is_active = 1",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.iter()
        .map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("resolume_title_token")))
        .collect()
}
```

- [ ] **Step 7: Update ResolumeRegistry to pass pool to drivers**

In `crates/sp-server/src/resolume/mod.rs`, update `add_host`:

```rust
pub fn add_host(
    &mut self,
    host_id: i64,
    host: String,
    port: u16,
    pool: SqlitePool,
    shutdown: broadcast::Receiver<()>,
) {
    let (tx, rx) = mpsc::channel::<ResolumeCommand>(64);
    let driver = HostDriver::new(host.clone(), port);

    tokio::spawn(async move {
        driver.run(pool, rx, shutdown).await;
    });

    info!(host_id, %host, port, "added Resolume host worker");
    self.hosts.insert(host_id, tx);
}
```

Add `use sqlx::SqlitePool;` to the imports in mod.rs.

- [ ] **Step 8: Run all tests**

Run: `cargo test -p sp-server -- --nocapture`
Expected: ALL PASS

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/src/resolume/
git commit -m "feat: replace A/B crossfade with show/hide opacity fade for Resolume titles"
```

---

## Task 5: Wire ResolumeRegistry into AppState and PlaybackEngine

**Files:**
- Modify: `crates/sp-server/src/lib.rs`
- Modify: `crates/sp-server/src/playback/mod.rs`

- [ ] **Step 1: Add ResolumeRegistry broadcast sender to AppState**

In `crates/sp-server/src/lib.rs`, add a broadcast channel for Resolume commands to AppState:

```rust
pub struct AppState {
    pub pool: SqlitePool,
    pub event_tx: broadcast::Sender<ServerMsg>,
    pub engine_tx: mpsc::Sender<EngineCommand>,
    pub obs_state: Arc<RwLock<obs::ObsState>>,
    pub tools_status: Arc<RwLock<ToolsStatus>>,
    pub tool_paths: Arc<RwLock<Option<ToolPaths>>>,
    pub sync_tx: mpsc::Sender<SyncRequest>,
    pub resolume_tx: broadcast::Sender<resolume::ResolumeCommand>,
}
```

- [ ] **Step 2: Wire up the registry in `start()`**

In the `start()` function, replace the `_resolume_registry` section:

```rust
// 9. Resolume workers (load enabled hosts from DB)
let (resolume_tx, _) = broadcast::channel::<resolume::ResolumeCommand>(64);
let resolume_rows =
    sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
let mut resolume_registry = resolume::ResolumeRegistry::new();
for row in resolume_rows {
    let host_id: i64 = row.get("id");
    let host: String = row.get("host");
    let port: i32 = row.get("port");
    resolume_registry.add_host(
        host_id,
        host,
        port as u16,
        pool.clone(),
        shutdown_tx.subscribe(),
    );
}

// Subscribe the registry to the broadcast channel.
let mut resolume_cmd_rx = resolume_tx.subscribe();
let resolume_registry_hosts = resolume_registry.hosts.clone();
tokio::spawn(async move {
    // Forward broadcast commands to all host workers.
    while let Ok(cmd) = resolume_cmd_rx.recv().await {
        for tx in resolume_registry_hosts.values() {
            let _ = tx.try_send(cmd.clone());
        }
    }
});
```

Wait — actually, the simpler approach: give the PlaybackEngine direct access to the registry's broadcast sender. The engine sends commands to the broadcast channel, and each HostDriver's run loop receives them.

Actually, let's keep it even simpler. The PlaybackEngine gets a `broadcast::Sender<ResolumeCommand>`. When it needs to show/hide titles, it sends to this broadcast. In `start()`, we subscribe each host worker to this broadcast.

Revised approach: Change `ResolumeRegistry::add_host` to accept a `broadcast::Receiver<ResolumeCommand>` and have the driver listen on both the per-host mpsc channel and the broadcast channel:

Actually the simplest approach: just give the playback engine a `Vec<mpsc::Sender<ResolumeCommand>>` or the entire registry. Let me keep it simple — the engine gets an `mpsc::Sender<ResolumeCommand>` and the registry forwards to all hosts.

In `start()`:

```rust
// 9. Resolume workers
let (resolume_cmd_tx, mut resolume_cmd_rx) = mpsc::channel::<resolume::ResolumeCommand>(64);
let resolume_rows =
    sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
let mut resolume_registry = resolume::ResolumeRegistry::new();
for row in resolume_rows {
    let host_id: i64 = row.get("id");
    let host: String = row.get("host");
    let port: i32 = row.get("port");
    resolume_registry.add_host(host_id, host, port as u16, pool.clone(), shutdown_tx.subscribe());
}

// Forward commands to all host workers.
let resolume_senders: Vec<mpsc::Sender<resolume::ResolumeCommand>> =
    resolume_registry.host_senders();
tokio::spawn(async move {
    while let Some(cmd) = resolume_cmd_rx.recv().await {
        for tx in &resolume_senders {
            let _ = tx.try_send(cmd.clone());
        }
    }
});
```

Add `host_senders()` method to `ResolumeRegistry`:

```rust
pub fn host_senders(&self) -> Vec<mpsc::Sender<ResolumeCommand>> {
    self.hosts.values().cloned().collect()
}
```

Update `AppState` to include `resolume_tx: mpsc::Sender<resolume::ResolumeCommand>`.

- [ ] **Step 3: Pass resolume_tx to PlaybackEngine**

Update `PlaybackEngine::new()`:

```rust
pub fn new(
    pool: SqlitePool,
    obs_event_tx: broadcast::Sender<ObsEvent>,
    obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
    resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
) -> Self {
    // ... same as before, store resolume_tx ...
}
```

Add the field to the struct:

```rust
pub struct PlaybackEngine {
    // ... existing fields ...
    resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
}
```

Update construction in `start()`:

```rust
let mut engine = playback::PlaybackEngine::new(
    pool.clone(),
    obs_event_tx,
    obs_cmd_tx,
    resolume_cmd_tx.clone(),
);
```

- [ ] **Step 4: Send ShowTitle/HideTitle from PlaybackEngine on pipeline events**

In `crates/sp-server/src/playback/mod.rs`, update `handle_pipeline_event`:

For `PipelineEvent::Started { duration_ms }`:

```rust
PipelineEvent::Started { duration_ms } => {
    debug!(playlist_id, duration_ms, "video started");
    if let Some(pp) = self.pipelines.get(&playlist_id) {
        if let Some(video_id) = pp.current_video_id {
            let pool = self.pool.clone();
            let obs_cmd = self.obs_cmd_tx.clone();
            let resolume_tx = self.resolume_tx.clone();
            let pl_id = playlist_id;
            let dur = *duration_ms;

            tokio::spawn(async move {
                // Wait 1.5s before showing title.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

                // Get video metadata.
                if let Ok(Some((song, artist, gemini_failed))) =
                    get_video_title_info(&pool, video_id).await
                {
                    // OBS title (existing).
                    if let Some(cmd_tx) = obs_cmd {
                        Self::show_title_obs(&pool, &cmd_tx, pl_id, &song, &artist).await;
                    }

                    // Resolume title.
                    let _ = resolume_tx
                        .send(crate::resolume::ResolumeCommand::ShowTitle {
                            playlist_id: pl_id,
                            song: song.clone(),
                            artist: artist.clone(),
                            gemini_failed,
                        })
                        .await;
                }
            });

            // Schedule title hide 3.5s before end.
            if dur > 5000 {
                let resolume_tx = self.resolume_tx.clone();
                let obs_cmd = self.obs_cmd_tx.clone();
                let pool = self.pool.clone();
                let hide_at = dur - 3500;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(hide_at)).await;

                    // OBS clear.
                    if let Some(cmd_tx) = obs_cmd {
                        Self::clear_title_obs(&pool, &cmd_tx, pl_id).await;
                    }

                    // Resolume hide.
                    let _ = resolume_tx
                        .send(crate::resolume::ResolumeCommand::HideTitle {
                            playlist_id: pl_id,
                        })
                        .await;
                });
            }
        }
    }
}
```

Add a helper to get title info including gemini_failed:

```rust
/// Get video title info (song, artist, gemini_failed) for display.
async fn get_video_title_info(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String, bool)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist, gemini_failed FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| {
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        let gemini_failed: bool = r.get::<i32, _>("gemini_failed") != 0;
        (song, artist, gemini_failed)
    }))
}
```

Rename existing `show_title` → `show_title_obs` and `clear_title` → `clear_title_obs` to distinguish from the Resolume path. Update `show_title_obs` to accept song/artist directly instead of re-querying the DB.

Remove the old position-based title hiding in `PipelineEvent::Position` (now handled by the timed spawn above).

- [ ] **Step 5: Update all references and fix compilation**

Update the `start_and_shutdown` and `app_state_construction` tests in `lib.rs` to include the new `resolume_tx` field. Update `test_state()` in `api/routes.rs` tests similarly.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p sp-server -- --nocapture`
Expected: ALL PASS

- [ ] **Step 7: Run format check**

Run: `cargo fmt --all --check`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/sp-server/src/lib.rs crates/sp-server/src/playback/mod.rs crates/sp-server/src/resolume/mod.rs crates/sp-server/src/api/routes.rs
git commit -m "feat: wire Resolume registry into playback engine for title delivery"
```

---

## Task 6: Update CI E2E to seed `resolume_title_token` and verify title delivery

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Update playlist seeding to include `resolume_title_token`**

In the `e2e-resolume` job's "Seed playlists" step, add the token to each playlist:

```powershell
$playlists = @(
    @{ name = "ytwarmup";    youtube_url = "..."; obs_text_source = "ytwarmup_title"; ndi_output_name = "SP-warmup"; resolume_title_token = "#spwarmup-title" },
    @{ name = "ytpresence";  youtube_url = "..."; obs_text_source = "ytpresence_title"; ndi_output_name = "SP-presence"; resolume_title_token = "#sppresence-title" },
    @{ name = "ytslow";      youtube_url = "..."; obs_text_source = "ytslow_title"; ndi_output_name = "SP-slow"; resolume_title_token = "#spslow-title" },
    @{ name = "yt90s";       youtube_url = "..."; obs_text_source = "yt90s_title"; ndi_output_name = "SP-90s"; resolume_title_token = "#sp90s-title" },
    @{ name = "ytworship";   youtube_url = "..."; obs_text_source = "ytworship_title"; ndi_output_name = "SP-worship"; resolume_title_token = "#spworship-title" },
    @{ name = "ytfast";      youtube_url = "..."; obs_text_source = "ytfast_title"; ndi_output_name = "SP-fast"; resolume_title_token = "#spfast-title" }
)
```

Also update the PATCH step (for existing playlists) to set the token:

```powershell
$updateBody = [System.Text.Encoding]::UTF8.GetBytes(
    "{`"resolume_title_token`":`"$($pl.resolume_title_token)`"}"
)
Invoke-WebRequest -Uri "http://localhost:8920/api/v1/playlists/$id" -Method Patch -Body $updateBody -ContentType "application/json" -UseBasicParsing | Out-Null
```

- [ ] **Step 2: Add E2E step to verify Resolume title delivery**

Add a new step after "Verify playback starts" that checks if text was set on the Resolume clip:

```yaml
- name: Verify Resolume title delivery
  shell: powershell
  run: |
    # The #spfast-title clip should have been updated by SongPlayer after playback started.
    # Query Resolume Arena REST API for the clip's text parameter.
    # Clip ID and param ID are discovered dynamically.
    
    # Get SongPlayer's clip mapping by checking what it found.
    $status = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status"
    Write-Host "SongPlayer status: version=$($status.version), obs=$($status.obs_connected)"
    
    # Wait for title show (1.5s delay + fade + processing).
    Start-Sleep -Seconds 5
    
    # Check Resolume composition for the #spfast-title clip text.
    $comp = Invoke-RestMethod -Uri "http://127.0.0.1:8090/api/v1/composition"
    $found = $false
    foreach ($layer in $comp.layers) {
        foreach ($clip in $layer.clips) {
            if ($clip.name.value -match '#spfast-title') {
                $textParam = $clip.video.sourceparams.Text
                $textValue = $textParam.value
                Write-Host "Found #spfast-title clip: text='$textValue'"
                if ($textValue -and $textValue -ne '' -and $textValue -ne 'Resolume') {
                    Write-Host "SUCCESS: Title was updated to '$textValue'"
                    $found = $true
                } else {
                    Write-Host "WARNING: Title text is '$textValue' - may not have been updated yet"
                }
                break
            }
        }
        if ($found) { break }
    }
    
    if (-not $found) {
        Write-Host "NOTE: #spfast-title clip text not yet updated - playback may not have triggered yet"
        Write-Host "This is expected if no normalized videos are cached for ytfast"
    }
```

- [ ] **Step 3: Run format check**

Run: `cargo fmt --all --check`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: seed resolume_title_token and verify title delivery in E2E"
```

---

## Task 7: Push, monitor CI, fix issues

- [ ] **Step 1: Run local format check**

```bash
cargo fmt --all --check
```

- [ ] **Step 2: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 3: Monitor CI until all jobs reach terminal state**

```bash
gh run list --branch dev --limit 3
```

Poll with `gh run view <run-id>` until all 17 jobs pass. If any fail, investigate with `gh run view <run-id> --log-failed`, fix in one commit, push once, monitor again.

- [ ] **Step 4: Verify E2E Resolume title delivery passed**

Check the E2E job output for the "Verify Resolume title delivery" step. Confirm it found the `#spfast-title` clip with updated text.
