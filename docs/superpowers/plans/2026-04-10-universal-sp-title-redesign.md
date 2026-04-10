# Universal `#sp-title` Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace per-playlist Resolume tokens and per-playlist OBS text sources with a single universal `#sp-title` identifier. Drop the `⚠` Gemini-failed indicator. Support multiple `#sp-title` clips across Resolume layers/columns/decks updated in parallel.

**Architecture:** One hardcoded constant `#sp-title` used both as Resolume clip-name tag and OBS text source name. Driver stores `HashMap<String, Vec<ClipInfo>>` to support many clips per token, updates all in parallel via `FuturesUnordered`. Per-playlist title columns dropped via DB migration V3.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), reqwest 0.12, tokio, futures 0.3, serde_json

**Design spec:** `docs/superpowers/specs/2026-04-10-universal-sp-title-redesign.md`

---

## Task 1: Database migration V3 — drop `obs_text_source` and `resolume_title_token`

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs`
- Modify: `crates/sp-server/src/db/models.rs`
- Modify: `crates/sp-core/src/models.rs`
- Modify: `crates/sp-core/src/lib.rs`

- [ ] **Step 1: Write failing test for migration V3**

Add to `crates/sp-server/src/db/mod.rs` in `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn migration_v3_drops_per_playlist_title_columns() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(playlists)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(!cols.contains(&"obs_text_source".to_string()),
        "obs_text_source column should be dropped, columns: {cols:?}");
    assert!(!cols.contains(&"resolume_title_token".to_string()),
        "resolume_title_token column should be dropped, columns: {cols:?}");
}
```

Also update the existing `pool_creation_and_migration` test to assert `ver == 3`, and update `migrations_are_idempotent` to assert `ver == 3`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sp-server migration_v3_drops -- --nocapture`
Expected: FAIL — columns still present.

- [ ] **Step 3: Add migration V3**

In `crates/sp-server/src/db/mod.rs`:

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
];

const MIGRATION_V3: &str = "
ALTER TABLE playlists DROP COLUMN obs_text_source;
ALTER TABLE playlists DROP COLUMN resolume_title_token;
";
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p sp-server migration_v3 -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Update Playlist model in sp-core**

In `crates/sp-core/src/models.rs`, remove `obs_text_source` and `resolume_title_token` fields from `Playlist`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub youtube_url: String,
    #[serde(default)]
    pub ndi_output_name: String,
    #[serde(default)]
    pub playback_mode: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}
```

- [ ] **Step 6: Update sp-core Playlist test**

In `crates/sp-core/src/lib.rs`, find `playlist_serde_roundtrip` and remove the dropped fields from the test struct literal:

```rust
#[test]
fn playlist_serde_roundtrip() {
    let p = models::Playlist {
        id: 1,
        name: "Test".into(),
        youtube_url: "https://youtube.com/playlist?list=PLxyz".into(),
        ndi_output_name: "SP-test".into(),
        playback_mode: "continuous".into(),
        is_active: true,
        created_at: None,
        updated_at: None,
    };
    let json = serde_json::to_string(&p).unwrap();
    let back: models::Playlist = serde_json::from_str(&json).unwrap();
    assert_eq!(p, back);
}
```

- [ ] **Step 7: Update get_active_playlists**

In `crates/sp-server/src/db/models.rs`, modify `get_active_playlists` to drop the column references:

```rust
pub async fn get_active_playlists(pool: &SqlitePool) -> Result<Vec<Playlist>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, is_active
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
            is_active: r.get::<i32, _>("is_active") != 0,
            ..Default::default()
        })
        .collect())
}
```

- [ ] **Step 8: Run all sp-core and sp-server tests**

Run: `cargo test -p sp-core -p sp-server`
Expected: ALL PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/sp-core/src/lib.rs crates/sp-core/src/models.rs crates/sp-server/src/db/mod.rs crates/sp-server/src/db/models.rs
git commit -m "feat: migration V3 drop per-playlist title columns"
```

---

## Task 2: API routes — drop per-playlist title fields

**Files:**
- Modify: `crates/sp-server/src/api/routes.rs`

- [ ] **Step 1: Read the API routes file**

Read `crates/sp-server/src/api/routes.rs` to find every reference to `obs_text_source` and `resolume_title_token`.

- [ ] **Step 2: Remove fields from `CreatePlaylistRequest`**

```rust
#[derive(Debug, Deserialize)]
pub struct CreatePlaylistRequest {
    pub name: String,
    pub youtube_url: String,
    #[serde(default)]
    pub ndi_output_name: Option<String>,
    #[serde(default)]
    pub playback_mode: Option<String>,
}
```

- [ ] **Step 3: Remove fields from `UpdatePlaylistRequest`**

```rust
#[derive(Debug, Deserialize)]
pub struct UpdatePlaylistRequest {
    pub name: Option<String>,
    pub youtube_url: Option<String>,
    pub ndi_output_name: Option<String>,
    pub playback_mode: Option<String>,
    pub is_active: Option<bool>,
}
```

- [ ] **Step 4: Update `list_playlists` SELECT and JSON output**

```rust
pub async fn list_playlists(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at
         FROM playlists ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(rows) => {
            let playlists: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.get::<i64, _>("id"),
                        "name": r.get::<String, _>("name"),
                        "youtube_url": r.get::<String, _>("youtube_url"),
                        "ndi_output_name": r.get::<String, _>("ndi_output_name"),
                        "playback_mode": r.get::<String, _>("playback_mode"),
                        "is_active": r.get::<i32, _>("is_active") != 0,
                        "created_at": r.get::<String, _>("created_at"),
                        "updated_at": r.get::<String, _>("updated_at"),
                    })
                })
                .collect();
            Json(playlists).into_response()
        }
        Err(e) => {
            warn!("list_playlists error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 5: Update `create_playlist` to drop fields**

```rust
pub async fn create_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreatePlaylistRequest>,
) -> impl IntoResponse {
    let ndi = body.ndi_output_name.as_deref().unwrap_or("");
    let mode = body.playback_mode.as_deref().unwrap_or("continuous");

    let result = sqlx::query(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode)
         VALUES (?, ?, ?, ?)
         RETURNING id, name, youtube_url, ndi_output_name, playback_mode, is_active",
    )
    .bind(&body.name)
    .bind(&body.youtube_url)
    .bind(ndi)
    .bind(mode)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(row) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": row.get::<i64, _>("id"),
                "name": row.get::<String, _>("name"),
                "youtube_url": row.get::<String, _>("youtube_url"),
                "ndi_output_name": row.get::<String, _>("ndi_output_name"),
                "playback_mode": row.get::<String, _>("playback_mode"),
                "is_active": row.get::<i32, _>("is_active") != 0,
            })),
        )
            .into_response(),
        Err(e) => {
            warn!("create_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 6: Update `get_playlist` SELECT and JSON**

```rust
pub async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let result = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, playback_mode, is_active, created_at, updated_at
         FROM playlists WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await;

    match result {
        Ok(Some(row)) => Json(serde_json::json!({
            "id": row.get::<i64, _>("id"),
            "name": row.get::<String, _>("name"),
            "youtube_url": row.get::<String, _>("youtube_url"),
            "ndi_output_name": row.get::<String, _>("ndi_output_name"),
            "playback_mode": row.get::<String, _>("playback_mode"),
            "is_active": row.get::<i32, _>("is_active") != 0,
            "created_at": row.get::<String, _>("created_at"),
            "updated_at": row.get::<String, _>("updated_at"),
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("get_playlist error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 7: Remove the dynamic SET branches in `update_playlist`**

In `update_playlist`, delete the `if let Some(ref obs) = body.obs_text_source` branch and the `if let Some(ref token) = body.resolume_title_token` branch (if it exists). Keep only the remaining fields (name, youtube_url, ndi_output_name, playback_mode, is_active).

- [ ] **Step 8: Run API tests**

Run: `cargo test -p sp-server api -- --nocapture`
Expected: ALL PASS. If a test references the dropped fields, update the test to remove them.

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/src/api/routes.rs
git commit -m "feat: drop per-playlist title fields from API"
```

---

## Task 3: Define `TITLE_TOKEN` constant and simplify `ResolumeCommand`

**Files:**
- Modify: `crates/sp-server/src/resolume/mod.rs`

- [ ] **Step 1: Read the resolume mod file**

Read `crates/sp-server/src/resolume/mod.rs` fully.

- [ ] **Step 2: Add `TITLE_TOKEN` constant and simplify command enum**

Replace the `ResolumeCommand` enum and add the constant:

```rust
/// The single Resolume clip tag used for title delivery.
/// Any Resolume clip whose name contains this tag becomes a title target.
pub const TITLE_TOKEN: &str = "#sp-title";

/// Commands sent to per-host Resolume workers.
#[derive(Debug, Clone)]
pub enum ResolumeCommand {
    /// Show a song title (set text + fade in) on all `#sp-title` clips.
    ShowTitle { song: String, artist: String },
    /// Hide the title (fade out + clear text) on all `#sp-title` clips.
    HideTitle,
    /// Force a refresh of the clip mapping cache.
    RefreshMapping,
    /// Stop the worker.
    Shutdown,
}
```

- [ ] **Step 3: Update existing tests in `mod.rs`**

Find the registry tests that build `ResolumeCommand::UpdateTitle` / `ClearTitle` / `ShowTitle { playlist_id, .. }` and replace them with the new shapes:

- `ResolumeCommand::ShowTitle { song: "x".into(), artist: "y".into() }`
- `ResolumeCommand::HideTitle`

If the existing tests reference `playlist_id`, drop those fields.

- [ ] **Step 4: Run resolume tests**

Run: `cargo test -p sp-server resolume::mod -- --nocapture`
Expected: tests in this module pass; compile errors will appear in `driver.rs` and `handlers.rs` (we'll fix in next tasks).

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/resolume/mod.rs
git commit -m "feat: simplify ResolumeCommand and add TITLE_TOKEN constant"
```

---

## Task 4: Driver — multi-clip mapping (`Vec<ClipInfo>` per token)

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`

- [ ] **Step 1: Read driver.rs fully**

Read `crates/sp-server/src/resolume/driver.rs`. Note current type signatures: `clip_mapping: HashMap<String, ClipInfo>`, `parse_composition` returns `HashMap<String, ClipInfo>`.

- [ ] **Step 2: Write failing test for multi-clip parsing**

In `crates/sp-server/src/resolume/driver.rs` tests:

```rust
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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p sp-server parse_composition_collects_multiple -- --nocapture`
Expected: FAIL — current code returns `HashMap<String, ClipInfo>` (single clip).

- [ ] **Step 4: Change ClipInfo storage to Vec**

In `crates/sp-server/src/resolume/driver.rs`, change the field and parse function:

```rust
pub struct HostDriver {
    host: String,
    port: u16,
    client: reqwest::Client,
    /// Maps clip token (e.g. `"#sp-title"`) to all clips bearing that tag.
    pub(crate) clip_mapping: HashMap<String, Vec<ClipInfo>>,
    endpoint_cache: Option<ResolvedEndpoint>,
}
```

Update the `new()` constructor — `clip_mapping: HashMap::new()` is unchanged.

Rewrite `parse_composition`:

```rust
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

            let tokens: Vec<&str> = name
                .split_whitespace()
                .filter(|w| w.starts_with('#'))
                .collect();

            if tokens.is_empty() {
                continue;
            }

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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p sp-server parse_composition -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Update existing parse_composition tests**

Existing tests reference `mapping["#token"].clip_id` directly (assuming single ClipInfo). Update them to index into the Vec: `mapping["#token"][0].clip_id`. Read each test in the file and adjust.

- [ ] **Step 7: Update `refresh_mapping` log message**

Change `clips = new_mapping.len()` to count total clips, not unique tokens:

```rust
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
```

- [ ] **Step 8: Run all driver tests**

Run: `cargo test -p sp-server resolume::driver -- --nocapture`
Expected: ALL PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs
git commit -m "feat: support multiple Resolume clips per token (Vec<ClipInfo>)"
```

---

## Task 5: Driver `handle_command` — use hardcoded `TITLE_TOKEN`, drop `load_tokens`

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`

- [ ] **Step 1: Remove `load_tokens` and update `run` signature**

In `crates/sp-server/src/resolume/driver.rs`, delete `load_tokens` entirely. Change `run` to no longer accept a `pool`:

```rust
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
```

- [ ] **Step 2: Update `handle_command` signature and body**

```rust
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
```

- [ ] **Step 3: Remove unused sqlx imports**

If the file no longer needs `use sqlx::Row;` or `use sqlx::SqlitePool;`, remove them.

- [ ] **Step 4: Update Registry `add_host` to drop `pool` parameter**

In `crates/sp-server/src/resolume/mod.rs`, update `add_host`:

```rust
pub fn add_host(
    &mut self,
    host_id: i64,
    host: String,
    port: u16,
    shutdown: broadcast::Receiver<()>,
) {
    let (tx, rx) = mpsc::channel::<ResolumeCommand>(64);
    let driver = HostDriver::new(host.clone(), port);

    tokio::spawn(async move {
        driver.run(rx, shutdown).await;
    });

    info!(host_id, %host, port, "added Resolume host worker");
    self.hosts.insert(host_id, tx);
}
```

Drop `use sqlx::SqlitePool;` from `mod.rs` if no longer needed.

- [ ] **Step 5: Update tests in `mod.rs` to drop pool argument**

Find any test calling `registry.add_host(..., pool, ...)` and remove the pool argument.

- [ ] **Step 6: Run tests (expect compile errors in lib.rs)**

Run: `cargo test -p sp-server resolume -- --nocapture`
Expected: compile errors in `lib.rs` (we'll fix in Task 7).

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs crates/sp-server/src/resolume/mod.rs
git commit -m "feat: remove load_tokens and per-playlist routing in Resolume driver"
```

---

## Task 6: Handlers — parallel multi-clip fade, remove `format_title_text` gemini_failed

**Files:**
- Modify: `crates/sp-server/src/resolume/handlers.rs`
- Modify: `crates/sp-server/Cargo.toml` (add `futures` if not present)

- [ ] **Step 1: Verify futures crate is available**

Run: `grep -A2 '\[dependencies\]' crates/sp-server/Cargo.toml | head -20`

If `futures` is not listed, add to `crates/sp-server/Cargo.toml` `[dependencies]`:

```toml
futures = "0.3"
```

- [ ] **Step 2: Write failing test for `format_title_text` without gemini_failed**

In `crates/sp-server/src/resolume/handlers.rs` tests, replace existing `format_title_text` tests:

```rust
#[test]
fn format_title_text_song_and_artist() {
    assert_eq!(format_title_text("My Song", "Artist"), "My Song - Artist");
}

#[test]
fn format_title_text_song_only() {
    assert_eq!(format_title_text("My Song", ""), "My Song");
}

#[test]
fn format_title_text_artist_only() {
    assert_eq!(format_title_text("", "Artist"), "Artist");
}

#[test]
fn format_title_text_empty() {
    assert_eq!(format_title_text("", ""), "");
}

#[test]
fn format_title_text_no_warning_indicator_anywhere() {
    // Verify the function never appends a warning symbol — gemini_failed is gone.
    let result = format_title_text("Song", "Artist");
    assert!(!result.contains('\u{26A0}'));
    assert!(!result.contains('⚠'));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p sp-server format_title_text -- --nocapture`
Expected: FAIL — `format_title_text` currently takes 3 args.

- [ ] **Step 4: Rewrite `handlers.rs` end-to-end**

Replace `crates/sp-server/src/resolume/handlers.rs` with:

```rust
//! Resolume title show/hide with parallel multi-clip opacity fade.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::debug;

use crate::resolume::TITLE_TOKEN;
use crate::resolume::driver::{ClipInfo, HostDriver};

const TEXT_SETTLE_MS: u64 = 35;
const FADE_DURATION_MS: u64 = 1000;
const FADE_STEPS: u32 = 20;

/// Format title text matching legacy Python behavior — clean `Song - Artist`.
/// No warning indicator (gemini_failed is no longer surfaced in titles).
pub fn format_title_text(song: &str, artist: &str) -> String {
    match (song.is_empty(), artist.is_empty()) {
        (false, false) => format!("{song} - {artist}"),
        (false, true) => song.to_string(),
        (true, false) => artist.to_string(),
        (true, true) => String::new(),
    }
}

/// Generate `n` evenly-spaced opacity values from `step/n` to `1.0`.
pub fn fade_steps(n: u32) -> Vec<f64> {
    (1..=n).map(|i| i as f64 / n as f64).collect()
}

/// Show title across all `#sp-title` clips in parallel.
///
/// 1. Set text on all clips' text params (parallel PUT).
/// 2. Wait 35ms for Resolume to process the texture update.
/// 3. Fade opacity 0→1 in 20 steps; each step sets all clips in parallel.
pub async fn show_title(
    driver: &mut HostDriver,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(token = TITLE_TOKEN, "no Resolume clips found, skipping show_title");
            return Ok(());
        }
    };

    let text = format_title_text(song, artist);
    if text.is_empty() {
        return Ok(());
    }

    // Step 1: parallel set text on all clips.
    set_text_all(driver, &clips, &text).await?;
    debug!(token = TITLE_TOKEN, count = clips.len(), %text, "set title text on all clips");

    // Step 2: wait for texture settle.
    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    // Step 3: fade in.
    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    for opacity in fade_steps(FADE_STEPS) {
        set_opacity_all(driver, &clips, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(token = TITLE_TOKEN, count = clips.len(), "title fade-in complete");
    Ok(())
}

/// Hide title across all `#sp-title` clips in parallel.
///
/// 1. Fade opacity 1→0 in 20 steps (parallel per step).
/// 2. Final parallel set-opacity to 0.0 (ensure fully hidden).
/// 3. Parallel clear text (set empty string).
pub async fn hide_title(driver: &mut HostDriver) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(token = TITLE_TOKEN, "no Resolume clips found, skipping hide_title");
            return Ok(());
        }
    };

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    let steps: Vec<f64> = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        set_opacity_all(driver, &clips, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    set_opacity_all(driver, &clips, 0.0).await?;

    set_text_all(driver, &clips, "").await?;

    debug!(token = TITLE_TOKEN, count = clips.len(), "title fade-out complete");
    Ok(())
}

/// Set text on every clip in parallel via FuturesUnordered.
async fn set_text_all(
    driver: &mut HostDriver,
    clips: &[ClipInfo],
    text: &str,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_text(clip.text_param_id, text));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

/// Set opacity on every clip in parallel via FuturesUnordered.
async fn set_opacity_all(
    driver: &mut HostDriver,
    clips: &[ClipInfo],
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_clip_opacity(clip.clip_id, opacity));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_title_text_song_and_artist() {
        assert_eq!(format_title_text("My Song", "Artist"), "My Song - Artist");
    }

    #[test]
    fn format_title_text_song_only() {
        assert_eq!(format_title_text("My Song", ""), "My Song");
    }

    #[test]
    fn format_title_text_artist_only() {
        assert_eq!(format_title_text("", "Artist"), "Artist");
    }

    #[test]
    fn format_title_text_empty() {
        assert_eq!(format_title_text("", ""), "");
    }

    #[test]
    fn format_title_text_no_warning_indicator_anywhere() {
        let result = format_title_text("Song", "Artist");
        assert!(!result.contains('\u{26A0}'));
        assert!(!result.contains('⚠'));
    }

    #[test]
    fn fade_steps_20_steps_over_1s() {
        let steps = fade_steps(20);
        assert_eq!(steps.len(), 20);
        assert!((steps[0] - 0.05).abs() < 0.001);
        assert!((steps[19] - 1.0).abs() < 0.001);
    }

    #[test]
    fn fade_steps_values_are_monotonically_increasing() {
        let steps = fade_steps(20);
        for i in 1..steps.len() {
            assert!(steps[i] > steps[i - 1]);
        }
    }
}
```

Note: this calls `driver.set_text(...)` which is `&mut self` — but inside `FuturesUnordered` that won't compile because we'd be borrowing `&mut driver` multiple times. We need to either (a) make `set_text`/`set_clip_opacity` not require `&mut self`, or (b) build the requests into a Vec of futures that are independent of the driver.

The cleanest fix: make `set_text` and `set_clip_opacity` take `&self` by ensuring the endpoint cache is populated BEFORE the parallel requests. The driver caches the endpoint via `endpoint().await?` which is `&mut self` only for cache write. After first call, it's read-only.

- [ ] **Step 5: Refactor driver to allow parallel calls**

In `crates/sp-server/src/resolume/driver.rs`, add a helper to ensure the endpoint cache is populated, then make `set_text` and `set_clip_opacity` take `&self`:

```rust
/// Ensure the endpoint cache is populated. Call before parallel operations.
pub(crate) async fn ensure_endpoint(&mut self) -> Result<(), anyhow::Error> {
    let _ = self.endpoint().await?;
    Ok(())
}

/// Get the cached endpoint (must call ensure_endpoint first).
fn cached_endpoint(&self) -> Option<&ResolvedEndpoint> {
    self.endpoint_cache.as_ref().filter(|ep| !ep.is_expired())
}

pub(crate) async fn set_text(&self, param_id: i64, text: &str) -> Result<(), anyhow::Error> {
    let ep = self
        .cached_endpoint()
        .ok_or_else(|| anyhow::anyhow!("endpoint cache empty - call ensure_endpoint first"))?
        .clone();
    let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
    let mut req = self
        .client
        .put(&url)
        .json(&serde_json::json!({ "value": text }));
    if let Some(ref host) = ep.host_header {
        req = req.header("Host", host);
    }
    req.send().await?.error_for_status()?;
    Ok(())
}

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
    let mut req = self
        .client
        .put(&url)
        .json(&serde_json::json!({"video":{"opacity":{"value": opacity}}}));
    if let Some(ref host) = ep.host_header {
        req = req.header("Host", host);
    }
    req.send().await?.error_for_status()?;
    Ok(())
}
```

`refresh_mapping` is the only caller that currently relies on `&mut self.endpoint()`; it's fine because it runs sequentially.

- [ ] **Step 6: Update handlers.rs to call `ensure_endpoint` before parallel ops**

At the start of both `show_title` and `hide_title`, after the empty-clips check, call:

```rust
driver.ensure_endpoint().await?;
```

Then the rest of the function uses `&driver` (immutable borrow) inside FuturesUnordered. Update `set_text_all` and `set_opacity_all` to take `&HostDriver`:

```rust
async fn set_text_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    text: &str,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_text(clip.text_param_id, text));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}

async fn set_opacity_all(
    driver: &HostDriver,
    clips: &[ClipInfo],
    opacity: f64,
) -> Result<(), anyhow::Error> {
    let mut futs = FuturesUnordered::new();
    for clip in clips {
        futs.push(driver.set_clip_opacity(clip.clip_id, opacity));
    }
    while let Some(res) = futs.next().await {
        res?;
    }
    Ok(())
}
```

And in `show_title`/`hide_title`, change the calls accordingly:

```rust
pub async fn show_title(
    driver: &mut HostDriver,
    song: &str,
    artist: &str,
) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(token = TITLE_TOKEN, "no Resolume clips found, skipping show_title");
            return Ok(());
        }
    };

    let text = format_title_text(song, artist);
    if text.is_empty() {
        return Ok(());
    }

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    set_text_all(driver_ref, &clips, &text).await?;
    debug!(token = TITLE_TOKEN, count = clips.len(), %text, "set title text on all clips");

    tokio::time::sleep(Duration::from_millis(TEXT_SETTLE_MS)).await;

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    for opacity in fade_steps(FADE_STEPS) {
        set_opacity_all(driver_ref, &clips, opacity).await?;
        tokio::time::sleep(step_delay).await;
    }

    debug!(token = TITLE_TOKEN, count = clips.len(), "title fade-in complete");
    Ok(())
}

pub async fn hide_title(driver: &mut HostDriver) -> Result<(), anyhow::Error> {
    let clips: Vec<ClipInfo> = match driver.clip_mapping.get(TITLE_TOKEN) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            debug!(token = TITLE_TOKEN, "no Resolume clips found, skipping hide_title");
            return Ok(());
        }
    };

    driver.ensure_endpoint().await?;
    let driver_ref: &HostDriver = driver;

    let step_delay = Duration::from_millis(FADE_DURATION_MS / FADE_STEPS as u64);
    let steps: Vec<f64> = fade_steps(FADE_STEPS);
    for opacity in steps.iter().rev() {
        set_opacity_all(driver_ref, &clips, *opacity).await?;
        tokio::time::sleep(step_delay).await;
    }
    set_opacity_all(driver_ref, &clips, 0.0).await?;

    set_text_all(driver_ref, &clips, "").await?;

    debug!(token = TITLE_TOKEN, count = clips.len(), "title fade-out complete");
    Ok(())
}
```

- [ ] **Step 7: Run resolume tests**

Run: `cargo test -p sp-server resolume -- --nocapture`
Expected: ALL PASS.

- [ ] **Step 8: Run formatter**

Run: `cargo fmt --all`

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/Cargo.toml crates/sp-server/src/resolume/handlers.rs crates/sp-server/src/resolume/driver.rs
git commit -m "feat: parallel multi-clip Resolume title fade with hardcoded TITLE_TOKEN"
```

---

## Task 7: Wire updated registry into `lib.rs` and playback engine

**Files:**
- Modify: `crates/sp-server/src/lib.rs`
- Modify: `crates/sp-server/src/playback/mod.rs`

- [ ] **Step 1: Read lib.rs Resolume wiring section**

Read `crates/sp-server/src/lib.rs` around the "9. Resolume workers" comment to see the current wiring.

- [ ] **Step 2: Update `add_host` calls in `lib.rs`**

Drop `pool.clone()` from `add_host` calls. The block becomes:

```rust
// 9. Resolume workers
let (resolume_cmd_tx, mut resolume_cmd_rx) = mpsc::channel::<resolume::ResolumeCommand>(64);
let resolume_rows = sqlx::query("SELECT id, host, port FROM resolume_hosts WHERE is_enabled = 1")
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
let mut resolume_registry = resolume::ResolumeRegistry::new();
for row in resolume_rows {
    let host_id: i64 = row.get("id");
    let host: String = row.get("host");
    let port: i32 = row.get("port");
    resolume_registry.add_host(host_id, host, port as u16, shutdown_tx.subscribe());
}

let resolume_senders = resolume_registry.host_senders();
tokio::spawn(async move {
    while let Some(cmd) = resolume_cmd_rx.recv().await {
        for tx in &resolume_senders {
            let _ = tx.try_send(cmd.clone());
        }
    }
});
```

- [ ] **Step 3: Update playback engine to send the new command shape**

In `crates/sp-server/src/playback/mod.rs`, find the `PipelineEvent::Started` handler. Replace the existing show/hide spawn blocks:

Add a hardcoded constant near the top of the file:

```rust
/// OBS text source name used for the fallback title display (in the
/// CG OVERLAY scene). Must match the source name in OBS exactly.
const OBS_TITLE_SOURCE: &str = "#sp-title";
```

Update `get_video_title_info` to drop `gemini_failed`:

```rust
async fn get_video_title_info(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| {
        use sqlx::Row;
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        (song, artist)
    }))
}
```

Update the Started handler:

```rust
PipelineEvent::Started { duration_ms } => {
    debug!(playlist_id, duration_ms, "video started");
    if let Some(pp) = self.pipelines.get(&playlist_id) {
        if let Some(video_id) = pp.current_video_id {
            // Title show after 1.5s.
            let pool = self.pool.clone();
            let obs_cmd = self.obs_cmd_tx.clone();
            let resolume_tx = self.resolume_tx.clone();
            let pl_id = playlist_id;
            let dur = *duration_ms;

            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                if let Ok(Some((song, artist))) =
                    get_video_title_info(&pool, video_id).await
                {
                    // Format the displayed text once for OBS.
                    let text = if artist.is_empty() {
                        song.clone()
                    } else if song.is_empty() {
                        artist.clone()
                    } else {
                        format!("{song} - {artist}")
                    };

                    // OBS fallback (single hardcoded source name).
                    if let Some(cmd_tx) = obs_cmd {
                        let _ = cmd_tx
                            .send(crate::obs::ObsCommand::SetTextSource {
                                source_name: OBS_TITLE_SOURCE.to_string(),
                                text,
                            })
                            .await;
                    }

                    // Resolume — registry broadcasts to all hosts; driver targets all #sp-title clips.
                    let _ = resolume_tx
                        .send(crate::resolume::ResolumeCommand::ShowTitle { song, artist })
                        .await;

                    info!(playlist_id = pl_id, video_id, "title shown");
                }
            });

            // Title hide 3.5s before end.
            if dur > 5000 {
                let obs_cmd = self.obs_cmd_tx.clone();
                let resolume_tx = self.resolume_tx.clone();
                let pl_id = playlist_id;
                let hide_at = dur - 3500;
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(hide_at)).await;

                    if let Some(cmd_tx) = obs_cmd {
                        let _ = cmd_tx
                            .send(crate::obs::ObsCommand::SetTextSource {
                                source_name: OBS_TITLE_SOURCE.to_string(),
                                text: String::new(),
                            })
                            .await;
                    }

                    let _ = resolume_tx
                        .send(crate::resolume::ResolumeCommand::HideTitle)
                        .await;

                    debug!(playlist_id = pl_id, "title hidden");
                });
            }
        }
    }
}
```

Delete any remaining static helpers `show_title_obs` / `clear_title_obs` if present (the inline blocks above replace them).

- [ ] **Step 4: Run all sp-server tests**

Run: `cargo test -p sp-server -- --nocapture`
Expected: ALL PASS. Fix any test that constructs `ResolumeCommand` with the old shape.

- [ ] **Step 5: Run formatter**

Run: `cargo fmt --all`

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/lib.rs crates/sp-server/src/playback/mod.rs
git commit -m "feat: wire universal #sp-title into playback engine and lib"
```

---

## Task 8: CI E2E updates — drop per-playlist title fields, verify multi-clip update

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Update playlist seeding step**

In `.github/workflows/ci.yml` "Seed playlists" step, remove `obs_text_source` and `resolume_title_token` from each playlist hashtable. The new array becomes:

```powershell
$playlists = @(
    @{ name = "ytwarmup";    youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvcHRX3nVKMEPHuBdU75dBVE"; ndi_output_name = "SP-warmup" },
    @{ name = "ytpresence";  youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BveAZ9YDY4ALy9iGxQVrkGRl"; ndi_output_name = "SP-presence" },
    @{ name = "ytslow";      youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758Bvd9c7dKV-ZZFQ1jg30ahHFq"; ndi_output_name = "SP-slow" },
    @{ name = "yt90s";       youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvfM0XYF6Q2nEDnW0CqHXI17"; ndi_output_name = "SP-90s" },
    @{ name = "ytworship";   youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BveEaqE5BWIQI7ukkijjdbbG"; ndi_output_name = "SP-worship" },
    @{ name = "ytfast";      youtube_url = "https://www.youtube.com/playlist?list=PLFdHTR758BvdEXF1tZ_3g8glRuev6EC6U"; ndi_output_name = "SP-fast" }
)
```

- [ ] **Step 2: Remove the resolume_title_token PUT block**

Remove the PowerShell block that sets `resolume_title_token` via PUT for existing playlists:

```powershell
# Ensure resolume_title_token is set
$tokenBody = [System.Text.Encoding]::UTF8.GetBytes("{`"resolume_title_token`":`"$($pl.resolume_title_token)`"}")
Invoke-WebRequest -Uri "http://localhost:8920/api/v1/playlists/$id" -Method Put -Body $tokenBody -ContentType "application/json" -UseBasicParsing | Out-Null
Write-Host "Set resolume_title_token for '$($pl.name)' to '$($pl.resolume_title_token)'"
```

Delete it entirely.

- [ ] **Step 3: Update create-playlist body to drop fields**

In the body construction, drop the `obs_text_source` and `resolume_title_token` keys:

```powershell
$body = @{
    name = $pl.name
    youtube_url = $pl.youtube_url
    ndi_output_name = $pl.ndi_output_name
} | ConvertTo-Json -Compress
```

- [ ] **Step 4: Update the Resolume verification step**

Replace the existing "Verify Resolume title delivery" step body with multi-clip verification:

```yaml
      - name: Verify Resolume title delivery
        shell: powershell
        run: |
          Start-Sleep -Seconds 5

          try {
            $comp = Invoke-RestMethod -Uri "http://127.0.0.1:8090/api/v1/composition"
            $titleClips = @()
            foreach ($layer in $comp.layers) {
              foreach ($clip in $layer.clips) {
                if ($clip.name.value -match '#sp-title') {
                  $titleClips += [pscustomobject]@{
                    Name = $clip.name.value
                    Text = $clip.video.sourceparams.Text.value
                  }
                }
              }
            }

            Write-Host "Found $($titleClips.Count) #sp-title clips in Resolume composition"
            foreach ($c in $titleClips) {
              Write-Host "  $($c.Name) -> '$($c.Text)'"
            }

            if ($titleClips.Count -eq 0) {
              Write-Host "INFO: no #sp-title clips configured in Resolume yet (operator must create them)"
            }
          } catch {
            Write-Host "WARNING: Could not query Resolume API: $_"
          }
```

The step is informational only — never fails.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: drop per-playlist title fields and verify multi-clip Resolume"
```

---

## Task 9: Push, monitor CI, fix issues

- [ ] **Step 1: Local format check**

Run: `cargo fmt --all --check`
Expected: clean.

- [ ] **Step 2: Bump VERSION to 0.7.2**

```bash
echo "0.7.2" > VERSION
./scripts/sync-version.sh
git add VERSION Cargo.toml src-tauri/Cargo.toml sp-ui/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump version to 0.7.2 for universal #sp-title release"
```

- [ ] **Step 3: Push to dev**

```bash
git push origin dev
```

- [ ] **Step 4: Monitor CI until terminal**

Run: `gh run list --branch dev --limit 3`
Then: `gh run view <run-id>` repeatedly until all 17 jobs complete.

- [ ] **Step 5: Investigate any failures**

If any job fails: `gh run view <run-id> --log-failed`. Fix root cause in one commit, push once, monitor again.

- [ ] **Step 6: Verify on win-resolume after deploy**

Use the `mcp__win-resolume__Shell` tool to:

1. Check SongPlayer version: `Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status" | ConvertTo-Json` — verify `version` is `0.7.2`.
2. Trigger play on ytfast: `Invoke-WebRequest -Uri "http://localhost:8920/api/v1/playback/7/play" -Method Post -UseBasicParsing | Out-Null`
3. Wait 5 seconds, then read all `#sp-title` clips:
   ```powershell
   $comp = Invoke-RestMethod -Uri "http://127.0.0.1:8090/api/v1/composition"
   foreach ($layer in $comp.layers) {
     foreach ($clip in $layer.clips) {
       if ($clip.name.value -match '#sp-title') {
         Write-Host "$($clip.name.value): '$($clip.video.sourceparams.Text.value)' opacity=$($clip.video.opacity.value)"
       }
     }
   }
   ```
4. Verify no `⚠` character in any title text.
5. Skip and verify titles update on all clips.
