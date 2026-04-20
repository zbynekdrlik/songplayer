# Live Playlist / Click-to-Play Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a single pre-created `ytlive` custom playlist whose contents are manually curated from the catalog, plus a new `/live` dashboard page that lets the operator click any song to jump-and-play it on the `SP-live` NDI output — in time for a live youth event tonight.

**Architecture:** Introduces a second playlist *kind* (`custom`) that reuses every downstream subsystem (scene detection, NDI sender, pipeline, title delivery). Cross-playlist references are stored in a new `playlist_items` table. A new `EngineCommand::PlayVideo` handler mirrors `handle_previous` to jump to a specific video. A new Leptos `/live` page shows a two-pane catalog/set-list UI.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), Axum 0.8, Tokio, Leptos 0.7 CSR, `gloo-net`, existing `sp_core` / `sp_server` / `sp-ui` crates.

**Spec:** [`docs/superpowers/specs/2026-04-17-live-playlist-click-to-play-design.md`](../specs/2026-04-17-live-playlist-click-to-play-design.md)

---

## Scope notes

- **Emergency timing.** This ships on top of whatever is currently on `dev`. PR #38 is still open; two execution outcomes are acceptable:
  - **A)** Land this on `dev` on top of #38 and let them ship in the same release.
  - **B)** Wait for #38 to merge first, then open a sibling PR for this feature.
  The plan assumes the work lands on `dev` directly (A is the default). The operator can always split later.
- **No version bump** is required — dev is on `0.19.0-dev.1` and has not merged since the last bump.
- **Playwright E2E is deferred.** Tonight ends with a manual smoke test on win-resolume. Playwright gets its own follow-up task AFTER the event.
- **OBS scene creation is a setup step**, not a code task. It uses the `obs-resolume` MCP and is described at the end.

## File Structure

**New files:**

- `crates/sp-server/src/api/live.rs` — HTTP handlers for `playlist_items` CRUD and the play-video endpoint.
- `sp-ui/src/pages/live.rs` — new Leptos page (route target for `Page::Live`).
- `sp-ui/src/components/live_catalog.rs` — left pane (catalog list with "has lyrics only" filter + `+ Add` button).
- `sp-ui/src/components/live_setlist.rs` — right pane (set list with `▶ Play` / `✕ Remove` per row + playback controls).

**Modified files:**

- `crates/sp-server/src/db/mod.rs` — add `MIGRATION_V13` constant + append `(13, MIGRATION_V13)` to `MIGRATIONS`; bump the expected version in the existing `pool_creation_and_migration` + `migrations_are_idempotent` tests to 13; add a V13-specific test.
- `crates/sp-core/src/models.rs` — add `kind: String` and `current_position: i64` to `Playlist`; replace the derived `Default` with an explicit impl that returns `kind: "youtube"`.
- `crates/sp-server/src/db/models.rs` — extend `get_active_playlists`'s SELECT + struct init to cover the new columns; same for `insert_playlist`.
- `crates/sp-server/src/playlist/selector.rs` — branch `VideoSelector::select_next` on `kind`; add a new `select_next_custom` helper.
- `crates/sp-server/src/startup.rs` — restrict `startup_sync_active_playlists` to `kind='youtube'`.
- `crates/sp-server/src/playback/mod.rs` — add `PlaybackEngine::handle_play_video`.
- `crates/sp-server/src/lib.rs` — add `EngineCommand::PlayVideo` variant + match arm in the engine loop.
- `crates/sp-server/src/api/mod.rs` — register the new routes.
- `sp-ui/src/app.rs` — add `Page::Live` variant + nav button.
- `sp-ui/src/pages/mod.rs` — `pub mod live;`.
- `sp-ui/src/components/mod.rs` — `pub mod live_catalog; pub mod live_setlist;`.
- `sp-ui/src/api.rs` — helpers for the new endpoints.

---

## Task 1: Migration V13 — schema + ytlive seed

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `crates/sp-server/src/db/mod.rs`:

```rust
#[tokio::test]
async fn migration_v13_adds_kind_and_current_position_columns() {
    let pool = setup().await;
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(playlists)")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    assert!(cols.contains(&"kind".to_string()), "columns: {cols:?}");
    assert!(
        cols.contains(&"current_position".to_string()),
        "columns: {cols:?}"
    );
}

#[tokio::test]
async fn migration_v13_creates_playlist_items_table() {
    let pool = setup().await;
    let row = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='playlist_items'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(row.is_some(), "playlist_items table should exist");
}

#[tokio::test]
async fn migration_v13_seeds_ytlive_custom_playlist() {
    let pool = setup().await;
    let row = sqlx::query(
        "SELECT kind, ndi_output_name, playback_mode, is_active, current_position
         FROM playlists WHERE name = 'ytlive'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let kind: String = row.get("kind");
    let ndi: String = row.get("ndi_output_name");
    let mode: String = row.get("playback_mode");
    let is_active: i64 = row.get("is_active");
    let pos: i64 = row.get("current_position");
    assert_eq!(kind, "custom");
    assert_eq!(ndi, "SP-live");
    assert_eq!(mode, "continuous");
    assert_eq!(is_active, 1);
    assert_eq!(pos, 0);
}
```

Also update the two existing version-count tests:

```rust
// In pool_creation_and_migration:
assert_eq!(ver, 13);
// In migrations_are_idempotent:
assert_eq!(ver, 13);
```

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server db::tests::migration_v13
```

Expected: FAIL (`"kind"` not in columns, `playlist_items` table missing, no ytlive row, schema version is 12 not 13).

- [ ] **Step 3: Add the migration constant**

In `crates/sp-server/src/db/mod.rs` right after `MIGRATION_V12`:

```rust
// V13 introduces the "custom" playlist kind for the Live/DJ-style set list.
//
// - `kind` text defaults to 'youtube' so every existing playlist keeps its
//   behavior. `current_position` is only meaningful for kind='custom' and
//   tracks which item in the set list was last played (so Skip advances).
// - `playlist_items` stores ordered references to existing videos. Videos
//   themselves still live under their *home* youtube playlist; this table
//   just names positions.
// - The 'ytlive' row is the single pre-created custom playlist used for
//   tonight's live event. youtube_url is the empty-string sentinel
//   (the column is NOT NULL; avoiding a table recreate keeps the
//   migration safe).
const MIGRATION_V13: &str = "
ALTER TABLE playlists ADD COLUMN kind TEXT NOT NULL DEFAULT 'youtube';
ALTER TABLE playlists ADD COLUMN current_position INTEGER NOT NULL DEFAULT 0;

CREATE TABLE playlist_items (
    playlist_id INTEGER NOT NULL,
    video_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    added_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (playlist_id, position),
    FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE,
    FOREIGN KEY (video_id) REFERENCES videos(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX idx_playlist_items_playlist_video
    ON playlist_items (playlist_id, video_id);

INSERT OR IGNORE INTO playlists
    (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
VALUES
    ('ytlive', '', 'SP-live', 'continuous', 1, 'custom');
";
```

And append to the `MIGRATIONS` array:

```rust
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
    (6, MIGRATION_V6),
    (7, MIGRATION_V7),
    (8, MIGRATION_V8),
    (9, MIGRATION_V9),
    (10, MIGRATION_V10),
    (11, MIGRATION_V11),
    (12, MIGRATION_V12),
    (13, MIGRATION_V13),
];
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server db::tests
```

Expected: PASS for all V13 tests, plus the bumped existing `pool_creation_and_migration` and `migrations_are_idempotent`.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/db/mod.rs
git commit -m "feat(db): migration V13 adds custom playlist kind + items table + ytlive seed"
```

---

## Task 2: Extend `Playlist` model with `kind` and `current_position`

**Files:**
- Modify: `crates/sp-core/src/models.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/sp-core/src/models.rs` inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn playlist_default_kind_is_youtube() {
    let p = Playlist::default();
    assert_eq!(p.kind, "youtube");
    assert_eq!(p.current_position, 0);
}

#[test]
fn playlist_deserialises_kind_and_current_position() {
    let json = r#"{
        "id": 7, "name": "ytlive", "youtube_url": "",
        "ndi_output_name": "SP-live", "playback_mode": "continuous",
        "is_active": true, "kind": "custom", "current_position": 3
    }"#;
    let p: Playlist = serde_json::from_str(json).unwrap();
    assert_eq!(p.kind, "custom");
    assert_eq!(p.current_position, 3);
}

#[test]
fn playlist_missing_kind_defaults_to_youtube_via_serde() {
    let json = r#"{"id": 1, "name": "x", "youtube_url": "u"}"#;
    let p: Playlist = serde_json::from_str(json).unwrap();
    assert_eq!(p.kind, "youtube");
    assert_eq!(p.current_position, 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-core
```

Expected: FAIL (field `kind` not found on `Playlist`).

- [ ] **Step 3: Update the struct + Default impl**

Replace the `Playlist` definition in `crates/sp-core/src/models.rs` with:

```rust
/// A playlist being tracked. `kind = "youtube"` is the default YouTube-backed
/// kind; `kind = "custom"` is an operator-curated set list used by the Live
/// dashboard. `current_position` is only meaningful for custom playlists
/// (tracks which set-list item was last played).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default = "default_true")]
    pub karaoke_enabled: bool,
    #[serde(default = "default_kind_youtube")]
    pub kind: String,
    #[serde(default)]
    pub current_position: i64,
}

fn default_true() -> bool {
    true
}

fn default_kind_youtube() -> String {
    "youtube".to_string()
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            youtube_url: String::new(),
            ndi_output_name: String::new(),
            playback_mode: String::new(),
            is_active: false,
            created_at: None,
            updated_at: None,
            karaoke_enabled: true,
            kind: default_kind_youtube(),
            current_position: 0,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-core
```

Expected: PASS (new tests + existing `playlist_karaoke_enabled_defaults_to_true`).

- [ ] **Step 5: Commit**

```bash
git add crates/sp-core/src/models.rs
git commit -m "feat(models): add kind + current_position to Playlist; default kind=youtube"
```

---

## Task 3: Populate new Playlist fields in `db::models` materialization

**Files:**
- Modify: `crates/sp-server/src/db/models.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/sp-server/src/db/models.rs` inside `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn get_active_playlists_includes_ytlive_with_kind_custom() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let active = get_active_playlists(&pool).await.unwrap();
    let ytlive = active
        .iter()
        .find(|p| p.name == "ytlive")
        .expect("ytlive should be pre-seeded as active");
    assert_eq!(ytlive.kind, "custom");
    assert_eq!(ytlive.current_position, 0);
    assert_eq!(ytlive.ndi_output_name, "SP-live");
}

#[tokio::test]
async fn insert_playlist_defaults_kind_to_youtube() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let created = insert_playlist(&pool, "TestYT", "https://yt.com/test")
        .await
        .unwrap();
    assert_eq!(created.kind, "youtube");
    assert_eq!(created.current_position, 0);
}
```

Note: `mod tests` is alongside the model functions. If the file has no `mod tests` yet, add one with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // tests here
}
```

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server db::models::tests::get_active_playlists_includes_ytlive
```

Expected: FAIL (field `kind` defaults to `""` because the struct init uses `..Default::default()` — wait, the new Default sets kind="youtube" so ytlive would *wrongly* default to youtube, not "custom"). The SQL has to select the column explicitly.

- [ ] **Step 3: Update `get_active_playlists` and `insert_playlist`**

Replace the body of `get_active_playlists` in `crates/sp-server/src/db/models.rs`:

```rust
pub async fn get_active_playlists(pool: &SqlitePool) -> Result<Vec<Playlist>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, name, youtube_url, ndi_output_name, is_active,
                playback_mode, kind, current_position
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
            playback_mode: r.get::<String, _>("playback_mode"),
            is_active: r.get::<i32, _>("is_active") != 0,
            kind: r.get::<String, _>("kind"),
            current_position: r.get::<i64, _>("current_position"),
            ..Default::default()
        })
        .collect())
}
```

And replace `insert_playlist`:

```rust
pub async fn insert_playlist(
    pool: &SqlitePool,
    name: &str,
    youtube_url: &str,
) -> Result<Playlist, sqlx::Error> {
    let row = sqlx::query(
        "INSERT INTO playlists (name, youtube_url)
         VALUES (?, ?)
         RETURNING id, name, youtube_url, is_active, playback_mode, kind, current_position",
    )
    .bind(name)
    .bind(youtube_url)
    .fetch_one(pool)
    .await?;

    Ok(Playlist {
        id: row.get("id"),
        name: row.get("name"),
        youtube_url: row.get("youtube_url"),
        playback_mode: row.get::<String, _>("playback_mode"),
        is_active: row.get::<i32, _>("is_active") != 0,
        kind: row.get::<String, _>("kind"),
        current_position: row.get::<i64, _>("current_position"),
        ..Default::default()
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server db::models::tests
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/db/models.rs
git commit -m "feat(db): populate kind + current_position in Playlist materialisation"
```

---

## Task 4: Exclude custom playlists from startup YouTube sync

**Files:**
- Modify: `crates/sp-server/src/startup.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/sp-server/src/startup.rs`:

```rust
#[cfg(test)]
mod sync_filter_tests {
    use super::*;
    use crate::db;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn startup_sync_skips_custom_playlists() {
        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        // Insert one youtube playlist alongside the pre-seeded ytlive custom one.
        db::models::insert_playlist(&pool, "ytfast", "https://yt.com/fast")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel::<SyncRequest>(8);
        startup_sync_active_playlists(&pool, &tx).await.unwrap();
        drop(tx);

        let mut received_urls = Vec::new();
        while let Some(req) = rx.recv().await {
            received_urls.push(req.youtube_url);
        }

        assert_eq!(received_urls.len(), 1, "only youtube playlists should be synced");
        assert_eq!(received_urls[0], "https://yt.com/fast");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```
cargo test -p sp-server startup::sync_filter_tests::startup_sync_skips_custom_playlists
```

Expected: FAIL — `received_urls` will contain `""` (the empty-string sentinel from the ytlive seed) in addition to the real URL.

- [ ] **Step 3: Restrict the SELECT to `kind='youtube'`**

In `crates/sp-server/src/startup.rs` change the query in `startup_sync_active_playlists`:

```rust
let rows = sqlx::query(
    "SELECT id, youtube_url FROM playlists WHERE is_active = 1 AND kind = 'youtube'",
)
```

- [ ] **Step 4: Run test to verify it passes**

```
cargo test -p sp-server startup::sync_filter_tests
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/startup.rs
git commit -m "fix(startup): skip custom-kind playlists in startup YouTube sync"
```

---

## Task 5: `VideoSelector::select_next` branches on `kind` for custom playlists

**Files:**
- Modify: `crates/sp-server/src/playlist/selector.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` in `crates/sp-server/src/playlist/selector.rs`:

```rust
/// Helper: build a custom playlist with `count` items referencing the given
/// (pre-normalized) video ids. Returns (pool, custom_playlist_id).
async fn setup_custom_playlist_with_items(video_ids: &[i64]) -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    // Seed a youtube playlist + videos so playlist_items FKs resolve.
    let yt = db::models::insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    for (i, vid) in video_ids.iter().enumerate() {
        db::models::upsert_video(&pool, yt.id, &format!("yt_{i}"), Some(&format!("Song {i}")))
            .await
            .unwrap();
        sqlx::query("UPDATE videos SET normalized = 1, file_path = ?, id = ? WHERE id = (SELECT id FROM videos WHERE youtube_id = ?)")
            .bind(format!("/cache/song_{i}.mp4"))
            .bind(vid)
            .bind(format!("yt_{i}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    // Create a custom playlist by hand (no public insert helper yet).
    let custom_id: i64 = sqlx::query_scalar(
        "INSERT INTO playlists (name, youtube_url, ndi_output_name, playback_mode, is_active, kind)
         VALUES ('live', '', 'SP-live', 'continuous', 1, 'custom') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    for (pos, vid) in video_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO playlist_items (playlist_id, video_id, position)
             VALUES (?, ?, ?)",
        )
        .bind(custom_id)
        .bind(*vid)
        .bind(pos as i64)
        .execute(&pool)
        .await
        .unwrap();
    }
    (pool, custom_id)
}

#[tokio::test]
async fn custom_continuous_advances_through_items_then_stops() {
    let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;

    // First selection — current_position=0, should return item at position 0 and
    // advance to 1.
    let v1 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, None)
        .await
        .unwrap();
    assert_eq!(v1, Some(10));

    let v2 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(10))
        .await
        .unwrap();
    assert_eq!(v2, Some(20));

    let v3 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(20))
        .await
        .unwrap();
    assert_eq!(v3, Some(30));

    // Past end — return None (stops playback).
    let v4 = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, Some(30))
        .await
        .unwrap();
    assert_eq!(v4, None);
}

#[tokio::test]
async fn custom_single_does_not_auto_advance() {
    let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;

    // In Single mode the operator drives via click-to-play; select_next
    // returns None so the engine does not auto-pick a follow-up.
    let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Single, Some(10))
        .await
        .unwrap();
    assert_eq!(v, None);
}

#[tokio::test]
async fn custom_loop_returns_current_video() {
    let (pool, custom_id) = setup_custom_playlist_with_items(&[10, 20, 30]).await;

    let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Loop, Some(20))
        .await
        .unwrap();
    assert_eq!(v, Some(20));
}

#[tokio::test]
async fn custom_empty_playlist_returns_none() {
    let (pool, custom_id) = setup_custom_playlist_with_items(&[]).await;

    let v = VideoSelector::select_next(&pool, custom_id, PlaybackMode::Continuous, None)
        .await
        .unwrap();
    assert_eq!(v, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server playlist::selector::tests::custom_
```

Expected: FAIL — the existing implementation queries `videos WHERE playlist_id=?`, but custom playlists have zero `videos` rows with that FK.

- [ ] **Step 3: Add the custom-playlist branch**

Replace `VideoSelector::select_next` in `crates/sp-server/src/playlist/selector.rs`:

```rust
impl VideoSelector {
    /// Select next video for a playlist based on playback mode.
    /// Returns the video id (from `videos.id`) or `None` if nothing should
    /// play next. Custom playlists use `playlist_items` ordered by position
    /// and advance `playlists.current_position` as a side-effect.
    pub async fn select_next(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        // Read the playlist kind to branch cleanly. Missing row → None.
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
                .bind(playlist_id)
                .fetch_optional(pool)
                .await?;
        let Some(kind) = kind else { return Ok(None) };

        match kind.as_str() {
            "custom" => Self::select_next_custom(pool, playlist_id, mode, current_video_id).await,
            _ => match mode {
                PlaybackMode::Loop => {
                    if let Some(id) = current_video_id {
                        return Ok(Some(id));
                    }
                    Self::select_random_unplayed(pool, playlist_id).await
                }
                PlaybackMode::Continuous | PlaybackMode::Single => {
                    Self::select_random_unplayed(pool, playlist_id).await
                }
            },
        }
    }

    /// Custom playlist selection using `playlist_items` + `current_position`.
    async fn select_next_custom(
        pool: &SqlitePool,
        playlist_id: i64,
        mode: PlaybackMode,
        current_video_id: Option<i64>,
    ) -> Result<Option<i64>, sqlx::Error> {
        match mode {
            PlaybackMode::Loop => Ok(current_video_id),
            PlaybackMode::Single => Ok(None),
            PlaybackMode::Continuous => {
                // Read current position; compute the next position.
                let cur_pos: i64 =
                    sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
                        .bind(playlist_id)
                        .fetch_one(pool)
                        .await?;

                // First call after a restart has current_video_id = None;
                // start from position 0 (cur_pos == 0) instead of advancing
                // past it.
                let next_pos = if current_video_id.is_none() {
                    cur_pos
                } else {
                    cur_pos + 1
                };

                let next_vid: Option<i64> = sqlx::query_scalar(
                    "SELECT video_id FROM playlist_items
                     WHERE playlist_id = ? AND position = ?",
                )
                .bind(playlist_id)
                .bind(next_pos)
                .fetch_optional(pool)
                .await?;

                if next_vid.is_some() {
                    sqlx::query("UPDATE playlists SET current_position = ? WHERE id = ?")
                        .bind(next_pos)
                        .bind(playlist_id)
                        .execute(pool)
                        .await?;
                }
                Ok(next_vid)
            }
        }
    }

    /* ... existing select_random_unplayed stays here ... */
}
```

(Leave `select_random_unplayed` unchanged.)

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server playlist::selector::tests
```

Expected: PASS for all four `custom_` tests and all pre-existing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/playlist/selector.rs
git commit -m "feat(selector): custom playlist selection via playlist_items + current_position"
```

---

## Task 6: DB helpers for `playlist_items` CRUD

**Files:**
- Modify: `crates/sp-server/src/db/models.rs`

- [ ] **Step 1: Write the failing tests**

Append these tests to `#[cfg(test)] mod tests` in `crates/sp-server/src/db/models.rs`:

```rust
#[tokio::test]
async fn append_item_assigns_next_position() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v1 = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let v2 = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;

    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let p1 = append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
    let p2 = append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
    assert_eq!(p1, 0);
    assert_eq!(p2, 1);
}

#[tokio::test]
async fn append_item_duplicate_errors() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, v).await.unwrap();
    let err = append_playlist_item(&pool, ytlive_id, v).await;
    assert!(err.is_err(), "duplicate append must error");
}

#[tokio::test]
async fn remove_item_compacts_positions() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v = |idx: usize| upsert_video(&pool, yt.id, &format!("id{idx}"), Some("X"));
    let v1 = v(1).await.unwrap().id;
    let v2 = v(2).await.unwrap().id;
    let v3 = v(3).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, v1).await.unwrap();
    append_playlist_item(&pool, ytlive_id, v2).await.unwrap();
    append_playlist_item(&pool, ytlive_id, v3).await.unwrap();

    remove_playlist_item(&pool, ytlive_id, v2).await.unwrap();

    let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
    // After compaction, positions must be 0,1 (no gap), pointing at v1 then v3.
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].position, 0);
    assert_eq!(items[0].video_id, v1);
    assert_eq!(items[1].position, 1);
    assert_eq!(items[1].video_id, v3);
}

#[tokio::test]
async fn list_playlist_items_returns_rows_in_position_order() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    append_playlist_item(&pool, ytlive_id, a).await.unwrap();
    append_playlist_item(&pool, ytlive_id, b).await.unwrap();

    let items = list_playlist_items(&pool, ytlive_id).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].video_id, a);
    assert_eq!(items[1].video_id, b);
}

#[tokio::test]
async fn position_for_video_lookup() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let yt = insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let a = upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let b = upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();
    append_playlist_item(&pool, ytlive_id, a).await.unwrap();
    append_playlist_item(&pool, ytlive_id, b).await.unwrap();

    let pos = position_for_playlist_item(&pool, ytlive_id, b).await.unwrap();
    assert_eq!(pos, Some(1));

    let missing = position_for_playlist_item(&pool, ytlive_id, 999)
        .await
        .unwrap();
    assert_eq!(missing, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server db::models::tests::append_item db::models::tests::remove_item db::models::tests::list_playlist_items db::models::tests::position_for_video_lookup
```

Expected: FAIL — functions don't exist yet.

- [ ] **Step 3: Add the model helpers**

At the end of `crates/sp-server/src/db/models.rs` (outside the test module), add:

```rust
// ---------------------------------------------------------------------------
// Custom playlist items
// ---------------------------------------------------------------------------

/// A single item in a custom playlist's set list.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlaylistItem {
    pub position: i64,
    pub video_id: i64,
}

/// Append a video to a custom playlist's set list. Returns the assigned
/// position. Errors if `(playlist_id, video_id)` already exists.
pub async fn append_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let next_pos: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_items WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO playlist_items (playlist_id, video_id, position) VALUES (?, ?, ?)",
    )
    .bind(playlist_id)
    .bind(video_id)
    .bind(next_pos)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(next_pos)
}

/// Remove a video from a custom playlist's set list and compact positions
/// so there are no gaps afterwards.
pub async fn remove_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM playlist_items WHERE playlist_id = ? AND video_id = ?")
        .bind(playlist_id)
        .bind(video_id)
        .execute(&mut *tx)
        .await?;

    // Compact: rewrite positions 0..N-1 preserving order. Two-step to avoid
    // PRIMARY KEY collisions: first negate all positions, then assign
    // sequential non-negative positions based on the negated ordering.
    sqlx::query(
        "UPDATE playlist_items SET position = -position - 1
         WHERE playlist_id = ?",
    )
    .bind(playlist_id)
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query(
        "SELECT video_id FROM playlist_items
         WHERE playlist_id = ? ORDER BY position DESC",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;
    for (new_pos, r) in rows.iter().enumerate() {
        let vid: i64 = r.get("video_id");
        sqlx::query(
            "UPDATE playlist_items SET position = ?
             WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(new_pos as i64)
        .bind(playlist_id)
        .bind(vid)
        .execute(&mut *tx)
        .await?;
    }

    // Clamp current_position to the new valid range.
    sqlx::query(
        "UPDATE playlists
         SET current_position = MIN(current_position,
             COALESCE((SELECT MAX(position) FROM playlist_items WHERE playlist_id = ?), 0))
         WHERE id = ?",
    )
    .bind(playlist_id)
    .bind(playlist_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// List all items of a custom playlist in position order.
pub async fn list_playlist_items(
    pool: &SqlitePool,
    playlist_id: i64,
) -> Result<Vec<PlaylistItem>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT position, video_id FROM playlist_items
         WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| PlaylistItem {
            position: r.get("position"),
            video_id: r.get("video_id"),
        })
        .collect())
}

/// Look up the position of a video within a custom playlist.
pub async fn position_for_playlist_item(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT position FROM playlist_items
         WHERE playlist_id = ? AND video_id = ?",
    )
    .bind(playlist_id)
    .bind(video_id)
    .fetch_optional(pool)
    .await
}
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server db::models::tests
```

Expected: PASS for all five new tests and no regressions on existing ones.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/db/models.rs
git commit -m "feat(db): playlist_items CRUD helpers (append/remove-with-compact/list/lookup)"
```

---

## Task 7: `EngineCommand::PlayVideo` + `PlaybackEngine::handle_play_video`

**Files:**
- Modify: `crates/sp-server/src/lib.rs`
- Modify: `crates/sp-server/src/playback/mod.rs`

- [ ] **Step 1: Write the failing tests**

Append to the existing playback tests file `crates/sp-server/src/playback/tests.rs` (same harness used by `handle_previous_pops_history_and_plays` at line 617):

```rust
#[tokio::test]
async fn handle_play_video_updates_current_position_on_custom_playlist() {
    use crate::db;

    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    // Seed videos under a youtube playlist so paths and FKs resolve.
    let yt = db::models::insert_playlist(&pool, "src", "https://yt.com/src")
        .await
        .unwrap();
    let v_a = db::models::upsert_video(&pool, yt.id, "a", Some("A")).await.unwrap().id;
    let v_b = db::models::upsert_video(&pool, yt.id, "b", Some("B")).await.unwrap().id;
    for (id, path_video, path_audio) in [
        (v_a, "/cache/a_video.mp4", "/cache/a_audio.flac"),
        (v_b, "/cache/b_video.mp4", "/cache/b_audio.flac"),
    ] {
        sqlx::query(
            "UPDATE videos SET normalized = 1, file_path = ?, audio_file_path = ? WHERE id = ?",
        )
        .bind(path_video)
        .bind(path_audio)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Seed the custom playlist (ytlive is already present) and add items.
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();
    db::models::append_playlist_item(&pool, ytlive_id, v_a)
        .await
        .unwrap();
    db::models::append_playlist_item(&pool, ytlive_id, v_b)
        .await
        .unwrap();

    // Boot an engine pointed at the seeded pool. Helper mirrors the one
    // used by `handle_previous_pops_history_and_plays`.
    let mut engine = test_engine_with_pool(pool.clone()).await;
    engine.handle_scene_change(ytlive_id, true).await;

    // Ask the engine to jump to v_b (position 1).
    engine.handle_play_video(ytlive_id, v_b).await;

    // Expect playlists.current_position == 1 after the jump.
    let pos: i64 = sqlx::query_scalar(
        "SELECT current_position FROM playlists WHERE id = ?",
    )
    .bind(ytlive_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos, 1);
}

#[tokio::test]
async fn handle_play_video_with_unknown_video_is_noop() {
    use crate::db;

    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let mut engine = test_engine_with_pool(pool.clone()).await;
    engine.handle_scene_change(ytlive_id, true).await;

    // 999 is not a known video id — engine must not panic or commit changes.
    engine.handle_play_video(ytlive_id, 999).await;

    let pos: i64 = sqlx::query_scalar(
        "SELECT current_position FROM playlists WHERE id = ?",
    )
    .bind(ytlive_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pos, 0, "unknown video must not touch current_position");
}
```

If a `test_engine_with_pool` helper does not already exist, extract one from whatever `handle_previous_pops_history_and_plays` uses (look at lines ~617-690 of `playback/tests.rs` for the pattern — copy that setup into a private helper in the same file and call it from the existing tests as well).

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server playback::tests::handle_play_video
```

Expected: FAIL — method `handle_play_video` does not exist.

- [ ] **Step 3: Add the engine command variant and handler**

In `crates/sp-server/src/lib.rs`, extend the enum:

```rust
#[derive(Debug, Clone)]
pub enum EngineCommand {
    SceneChanged { playlist_id: i64, on_program: bool },
    Play { playlist_id: i64 },
    Pause { playlist_id: i64 },
    Skip { playlist_id: i64 },
    Previous { playlist_id: i64 },
    SetMode { playlist_id: i64, mode: PlaybackMode },
    /// Jump to a specific video within a playlist and start playing it
    /// immediately. For custom playlists, also updates
    /// `playlists.current_position` so subsequent Skip advances from the
    /// new position. For youtube playlists it behaves like Previous
    /// (plays the given video but does not affect the random-unplayed
    /// selector; the next Skip will pick a fresh random video).
    PlayVideo { playlist_id: i64, video_id: i64 },
}
```

And extend the engine loop match arm (same `match cmd` block that currently handles `Play`, `Pause`, etc.):

```rust
EngineCommand::PlayVideo { playlist_id, video_id } => {
    engine.handle_play_video(playlist_id, video_id).await;
}
```

In `crates/sp-server/src/playback/mod.rs` add the handler immediately after `handle_previous`:

```rust
/// Jump to a specific video within a playlist and start playing it.
///
/// For custom playlists this also updates `playlists.current_position` so
/// the next `Skip` advances to position+1. For youtube playlists the
/// column is ignored by the selector — only the pipeline command is
/// relevant. The previously-playing video (if any) is pushed onto the
/// history stack so `Previous` still walks the history.
#[cfg_attr(test, mutants::skip)]
pub async fn handle_play_video(&mut self, playlist_id: i64, video_id: i64) {
    // Resolve paths first — if the video row is unknown, no side-effects.
    let paths = match crate::db::models::get_song_paths(&self.pool, video_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            warn!(playlist_id, video_id, "PlayVideo: no paths for video; ignoring");
            return;
        }
        Err(e) => {
            warn!(playlist_id, video_id, %e, "PlayVideo: DB lookup failed; ignoring");
            return;
        }
    };

    // For custom playlists, bump current_position to the clicked item's
    // position so Skip continues from the right place.
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or_default();
    if kind.as_deref() == Some("custom") {
        if let Ok(Some(pos)) =
            crate::db::models::position_for_playlist_item(&self.pool, playlist_id, video_id).await
        {
            let _ = sqlx::query(
                "UPDATE playlists SET current_position = ? WHERE id = ?",
            )
            .bind(pos)
            .bind(playlist_id)
            .execute(&self.pool)
            .await;
        }
    }

    // Send the pipeline command and update engine bookkeeping.
    let (video_path, audio_path) = paths;
    if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
        if let Some(prev) = pp.current_video_id {
            // Keep Previous working by pushing the prior video onto history.
            if prev != video_id {
                pp.history.push_back(prev);
            }
        }
        pp.current_video_id = Some(video_id);
        pp.state = PlayState::Playing { video_id };
        info!(
            playlist_id,
            video_id, %video_path, %audio_path,
            "PlayVideo → jumping to clicked song"
        );
        pp.pipeline.send(PipelineCommand::Play {
            video: video_path.into(),
            audio: audio_path.into(),
        });

        let _ = self.ws_event_tx.send(ServerMsg::PlaybackStateChanged {
            playlist_id,
            state: WsPlaybackState::Playing,
            mode: pp.mode,
        });
    } else {
        warn!(playlist_id, video_id, "PlayVideo: no pipeline for playlist");
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server playback::tests::handle_play_video
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/lib.rs crates/sp-server/src/playback/mod.rs crates/sp-server/src/playback/tests.rs
git commit -m "feat(engine): PlayVideo command jumps to specific video and updates current_position"
```

---

## Task 8: HTTP handlers for `playlist_items` + play-video

**Files:**
- Create: `crates/sp-server/src/api/live.rs`
- Modify: `crates/sp-server/src/api/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/sp-server/src/api/live.rs` with the handler skeletons and tests (skeletons will be empty/missing fns at this step; the file below is the final form — write ONLY the `#[cfg(test)] mod tests` block first and see it fail, then add the impl. For brevity the full file is shown once and both steps reference it).

For the TDD step: append this test block to a new file `crates/sp-server/src/api/live.rs`:

```rust
//! HTTP handlers for custom playlist set-list management + click-to-play.

#[cfg(test)]
mod tests {
    use crate::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sqlx::SqlitePool;
    use tokio::sync::{broadcast, mpsc};
    use tower::ServiceExt;

    async fn setup() -> (SqlitePool, i64, i64, i64) {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        // Seed videos under a youtube playlist.
        let yt = crate::db::models::insert_playlist(&pool, "src", "https://yt.com/src")
            .await
            .unwrap();
        let v1 = crate::db::models::upsert_video(&pool, yt.id, "a", Some("A"))
            .await
            .unwrap()
            .id;
        let v2 = crate::db::models::upsert_video(&pool, yt.id, "b", Some("B"))
            .await
            .unwrap()
            .id;
        sqlx::query("UPDATE videos SET normalized = 1 WHERE id IN (?, ?)")
            .bind(v1)
            .bind(v2)
            .execute(&pool)
            .await
            .unwrap();

        let ytlive_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='ytlive'")
            .fetch_one(&pool)
            .await
            .unwrap();
        (pool, ytlive_id, v1, v2)
    }

    fn build_state(pool: SqlitePool, engine_tx: mpsc::Sender<crate::EngineCommand>) -> AppState {
        // Mirrors the `test_state` helper in
        // crates/sp-server/src/api/routes_tests.rs — construct AppState
        // with the real field set, no-op channels where we don't care.
        use std::sync::Arc;
        use tokio::sync::RwLock;
        let (event_tx, _) = broadcast::channel(16);
        let (sync_tx, _) = mpsc::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (obs_rebuild_tx, _) = broadcast::channel(4);
        AppState {
            pool,
            event_tx,
            engine_tx,
            obs_state: Arc::new(RwLock::new(crate::obs::ObsState::default())),
            tools_status: Arc::new(RwLock::new(crate::ToolsStatus::default())),
            tool_paths: Arc::new(RwLock::new(None)),
            sync_tx,
            resolume_tx,
            obs_rebuild_tx,
            cache_dir: std::path::PathBuf::from("/tmp/cache"),
            ai_proxy: Arc::new(crate::ai::proxy::ProxyManager::new(
                std::path::PathBuf::from("/tmp/cache"),
                crate::ai::proxy::ProxyManager::default_port(),
            )),
            ai_client: Arc::new(crate::ai::client::AiClient::new(
                crate::ai::AiSettings::default(),
            )),
        }
    }

    #[tokio::test]
    async fn post_item_appends_and_returns_position() {
        let (pool, ytlive_id, v1, _) = setup().await;
        let (engine_tx, _engine_rx) = mpsc::channel(8);
        let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

        let body = format!(r#"{{"video_id": {v1}}}"#);
        let resp = app
            .oneshot(
                Request::post(format!("/api/v1/playlists/{ytlive_id}/items"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["position"], 0);
    }

    #[tokio::test]
    async fn post_item_on_youtube_playlist_returns_409() {
        let (pool, _, v1, _) = setup().await;
        let yt_id: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name='src'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (engine_tx, _) = mpsc::channel(8);
        let app = crate::api::router(build_state(pool, engine_tx), None);

        let body = format!(r#"{{"video_id": {v1}}}"#);
        let resp = app
            .oneshot(
                Request::post(format!("/api/v1/playlists/{yt_id}/items"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_item_returns_ok_and_removes_row() {
        let (pool, ytlive_id, v1, _) = setup().await;
        crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
            .await
            .unwrap();

        let (engine_tx, _) = mpsc::channel(8);
        let app = crate::api::router(build_state(pool.clone(), engine_tx), None);

        let resp = app
            .oneshot(
                Request::delete(format!("/api/v1/playlists/{ytlive_id}/items/{v1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let items = crate::db::models::list_playlist_items(&pool, ytlive_id)
            .await
            .unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn get_items_returns_list_in_order() {
        let (pool, ytlive_id, v1, v2) = setup().await;
        crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
            .await
            .unwrap();
        crate::db::models::append_playlist_item(&pool, ytlive_id, v2)
            .await
            .unwrap();

        let (engine_tx, _) = mpsc::channel(8);
        let app = crate::api::router(build_state(pool, engine_tx), None);

        let resp = app
            .oneshot(
                Request::get(format!("/api/v1/playlists/{ytlive_id}/items"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let arr: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["position"], 0);
        assert_eq!(arr[0]["video_id"], v1);
        assert_eq!(arr[1]["position"], 1);
        assert_eq!(arr[1]["video_id"], v2);
    }

    #[tokio::test]
    async fn play_video_sends_engine_command() {
        let (pool, ytlive_id, v1, _) = setup().await;
        crate::db::models::append_playlist_item(&pool, ytlive_id, v1)
            .await
            .unwrap();

        let (engine_tx, mut engine_rx) = mpsc::channel(8);
        let app = crate::api::router(build_state(pool, engine_tx), None);

        let body = format!(r#"{{"video_id": {v1}}}"#);
        let resp = app
            .oneshot(
                Request::post(format!("/api/v1/playlists/{ytlive_id}/play-video"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let cmd = engine_rx.recv().await.expect("engine command");
        match cmd {
            crate::EngineCommand::PlayVideo {
                playlist_id,
                video_id,
            } => {
                assert_eq!(playlist_id, ytlive_id);
                assert_eq!(video_id, v1);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
```

Check how the existing `routes_tests.rs` builds `AppState` at line 48; the helper `AppState::test_default()` may need to be added to `AppState` if it does not exist — inspect `crates/sp-server/src/api/routes_tests.rs:48` and mirror the exact construction there for `build_state`.

- [ ] **Step 2: Run tests to verify they fail**

```
cargo test -p sp-server api::live
```

Expected: FAIL — file exists only with tests and no handlers; compilation error that `crate::api::router` does not register `/api/v1/playlists/{id}/items` etc.

- [ ] **Step 3: Implement the handlers**

Write the full contents of `crates/sp-server/src/api/live.rs`:

```rust
//! HTTP handlers for custom playlist set-list management + click-to-play.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AppState;
use crate::db::models;

#[derive(Debug, Deserialize)]
pub struct AddItemRequest {
    pub video_id: i64,
}

#[derive(Debug, Serialize)]
pub struct AddItemResponse {
    pub position: i64,
}

// HTTP handler: validates playlist kind is 'custom' then appends the video
// to playlist_items. Returns 409 for youtube playlists and for duplicate
// video_ids. Covered by api::live::tests::post_item_*.
#[cfg_attr(test, mutants::skip)]
pub async fn post_add_item(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<AddItemRequest>,
) -> impl IntoResponse {
    let kind: Option<String> =
        match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(k) => k,
            Err(e) => {
                warn!(playlist_id, %e, "post_add_item: kind lookup failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }
        };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    match models::append_playlist_item(&state.pool, playlist_id, req.video_id).await {
        Ok(position) => Json(AddItemResponse { position }).into_response(),
        Err(e) => {
            warn!(playlist_id, video_id = req.video_id, %e, "append_playlist_item failed");
            // UNIQUE constraint = duplicate add.
            let msg = e.to_string();
            if msg.contains("UNIQUE") {
                (StatusCode::CONFLICT, "video already in playlist").into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        }
    }
}

// HTTP handler: removes a video from the custom playlist and compacts
// positions. 409 for youtube playlists.
#[cfg_attr(test, mutants::skip)]
pub async fn delete_item(
    State(state): State<AppState>,
    Path((playlist_id, video_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let kind: Option<String> =
        match sqlx::query_scalar("SELECT kind FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(k) => k,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    match kind.as_deref() {
        Some("custom") => {}
        Some(_) => return (StatusCode::CONFLICT, "playlist is not custom").into_response(),
        None => return (StatusCode::NOT_FOUND, "playlist not found").into_response(),
    }

    match models::remove_playlist_item(&state.pool, playlist_id, video_id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            warn!(playlist_id, video_id, %e, "remove_playlist_item failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

// HTTP handler: returns the current set list in position order.
#[cfg_attr(test, mutants::skip)]
pub async fn get_items(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    match models::list_playlist_items(&state.pool, playlist_id).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => {
            warn!(playlist_id, %e, "list_playlist_items failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PlayVideoRequest {
    pub video_id: i64,
}

// HTTP handler: sends EngineCommand::PlayVideo to the engine. The engine
// is responsible for all side-effects (paths lookup, current_position
// update, pipeline play, WS broadcast).
#[cfg_attr(test, mutants::skip)]
pub async fn post_play_video(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Json(req): Json<PlayVideoRequest>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(crate::EngineCommand::PlayVideo {
            playlist_id,
            video_id: req.video_id,
        })
        .await;
    StatusCode::NO_CONTENT
}

#[path = "live_tests_included.rs"]
#[cfg(test)]
mod tests_included;
```

**Note:** keep the tests you wrote in Step 1 in a separate file to satisfy the 1000-line cap pattern the codebase uses. Move the `#[cfg(test)] mod tests { ... }` block into `crates/sp-server/src/api/live_tests_included.rs` and replace the inline `mod tests { ... }` in `live.rs` with the `#[path = "..."] #[cfg(test)] mod tests_included;` line shown above.

Register the routes in `crates/sp-server/src/api/mod.rs`. After the existing playback-control routes block (look for the `/api/v1/playback/{id}/play` registration), add:

```rust
// Custom playlist set list + click-to-play.
.route(
    "/api/v1/playlists/{id}/items",
    axum::routing::get(live::get_items).post(live::post_add_item),
)
.route(
    "/api/v1/playlists/{id}/items/{video_id}",
    axum::routing::delete(live::delete_item),
)
.route(
    "/api/v1/playlists/{id}/play-video",
    axum::routing::post(live::post_play_video),
)
```

And add at the top of `crates/sp-server/src/api/mod.rs`:

```rust
pub mod live;
```

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p sp-server api::live
```

Expected: PASS for all five handler tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/api/live.rs crates/sp-server/src/api/live_tests_included.rs crates/sp-server/src/api/mod.rs
git commit -m "feat(api): /playlists/{id}/items CRUD + /play-video click endpoint"
```

---

## Task 9: WASM frontend — `api.rs` helpers for the new endpoints

**Files:**
- Modify: `sp-ui/src/api.rs`

- [ ] **Step 1: Add the helpers**

Append to `sp-ui/src/api.rs`:

```rust
// ── Live playlist API helpers ─────────────────────────────────────────────────

/// GET all set-list items for a custom playlist.
pub async fn get_live_items(playlist_id: i64) -> Result<Vec<serde_json::Value>, String> {
    get(&format!("/api/v1/playlists/{playlist_id}/items")).await
}

/// POST to append a video to a custom playlist's set list.
pub async fn post_live_add_item(
    playlist_id: i64,
    video_id: i64,
) -> Result<serde_json::Value, String> {
    post_json(
        &format!("/api/v1/playlists/{playlist_id}/items"),
        &serde_json::json!({ "video_id": video_id }),
    )
    .await
}

/// DELETE a video from a custom playlist.
pub async fn delete_live_item(playlist_id: i64, video_id: i64) -> Result<(), String> {
    delete(&format!("/api/v1/playlists/{playlist_id}/items/{video_id}")).await
}

/// POST to jump-and-play a specific video on a custom playlist.
pub async fn post_live_play_video(playlist_id: i64, video_id: i64) -> Result<(), String> {
    post_json_empty(
        &format!("/api/v1/playlists/{playlist_id}/play-video"),
        &serde_json::json!({ "video_id": video_id }),
    )
    .await
}
```

If `post_json_empty` does not exist in `sp-ui/src/api.rs`, also add it alongside `put_json_empty`:

```rust
/// POST JSON to `path` and discard the response body.
pub async fn post_json_empty<T: Serialize>(path: &str, body: &T) -> Result<(), String> {
    let resp = Request::post(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    Ok(())
}
```

- [ ] **Step 2: Verify the WASM build compiles**

```
cd sp-ui && trunk build
```

Expected: build succeeds, no warnings about unused functions (they're `pub`, so unused-in-own-crate warnings are silenced anyway).

- [ ] **Step 3: Commit**

```bash
cd /home/newlevel/devel/songplayer
git add sp-ui/src/api.rs
git commit -m "feat(ui): API helpers for live-playlist items and click-to-play"
```

---

## Task 10: WASM frontend — `/live` page + components

**Files:**
- Create: `sp-ui/src/pages/live.rs`
- Create: `sp-ui/src/components/live_catalog.rs`
- Create: `sp-ui/src/components/live_setlist.rs`
- Modify: `sp-ui/src/pages/mod.rs`
- Modify: `sp-ui/src/components/mod.rs`
- Modify: `sp-ui/src/app.rs`

- [ ] **Step 1: Create the catalog component**

Create `sp-ui/src/components/live_catalog.rs`:

```rust
//! Left pane of /live: lists all songs from the catalog with an optional
//! "has lyrics only" filter and a "+ Add" button per row that appends the
//! song to the given custom playlist's set list.

use leptos::prelude::*;

use crate::api;

#[component]
pub fn LiveCatalog(
    /// The custom playlist that add-clicks target.
    target_playlist_id: i64,
    /// Bumped by the parent when a set-list change should refresh any
    /// "already-added" indicators inside this view (currently unused; kept
    /// for future per-row checkmarks).
    #[prop(into)] _set_list_version: Signal<u64>,
    /// Callback fired with the video_id after a successful add. Lets the
    /// parent refresh the set-list view.
    on_added: Callback<i64>,
) -> impl IntoView {
    let songs = RwSignal::new(Vec::<serde_json::Value>::new());
    let show_only_with_lyrics = RwSignal::new(true);
    let error_msg = RwSignal::new(String::new());

    // Load the full catalog on mount.
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match api::get_lyrics_songs(None).await {
                Ok(list) => songs.set(list),
                Err(e) => error_msg.set(format!("failed to load songs: {e}")),
            }
        });
    });

    let visible = move || {
        let all = songs.get();
        let filter = show_only_with_lyrics.get();
        all.into_iter()
            .filter(|s| {
                if filter {
                    s["has_lyrics"].as_bool().unwrap_or(false)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="live-catalog">
            <div class="live-catalog-header">
                <h2>"Catalog"</h2>
                <label>
                    <input
                        type="checkbox"
                        prop:checked=move || show_only_with_lyrics.get()
                        on:change=move |ev| {
                            let checked = event_target_checked(&ev);
                            show_only_with_lyrics.set(checked);
                        }
                    />
                    " Only songs with lyrics"
                </label>
            </div>
            <div class="live-catalog-error">{move || error_msg.get()}</div>
            <table class="live-catalog-table">
                <thead>
                    <tr>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th>"Lyrics"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=visible
                        key=|s| s["video_id"].as_i64().unwrap_or(0)
                        children=move |song| {
                            let video_id = song["video_id"].as_i64().unwrap_or(0);
                            let title = song["song"].as_str().unwrap_or("—").to_string();
                            let artist = song["artist"].as_str().unwrap_or("—").to_string();
                            let has_lyrics = song["has_lyrics"].as_bool().unwrap_or(false);
                            let badge = if has_lyrics { "✓" } else { "" };
                            view! {
                                <tr>
                                    <td>{title}</td>
                                    <td>{artist}</td>
                                    <td>{badge}</td>
                                    <td>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                match api::post_live_add_item(
                                                    target_playlist_id, video_id,
                                                ).await {
                                                    Ok(_) => on_added.run(video_id),
                                                    Err(e) => error_msg.set(e),
                                                }
                                            });
                                        }>"+ Add"</button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}
```

- [ ] **Step 2: Create the set-list component**

Create `sp-ui/src/components/live_setlist.rs`:

```rust
//! Right pane of /live: the current set list with ▶ / ✕ buttons per row
//! and the standard playback controls bound to the custom playlist id.

use leptos::prelude::*;

use crate::api;

#[component]
pub fn LiveSetList(
    playlist_id: i64,
    #[prop(into)] refresh: Signal<u64>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let items = RwSignal::new(Vec::<serde_json::Value>::new());
    let songs = RwSignal::new(Vec::<serde_json::Value>::new());
    let error_msg = RwSignal::new(String::new());

    // Reload whenever `refresh` bumps (add/remove/initial mount).
    let _load = Effect::new(move |_| {
        let _tick = refresh.get();
        leptos::task::spawn_local(async move {
            let items_res = api::get_live_items(playlist_id).await;
            let songs_res = api::get_lyrics_songs(None).await;
            match (items_res, songs_res) {
                (Ok(i), Ok(s)) => {
                    items.set(i);
                    songs.set(s);
                }
                (Err(e), _) | (_, Err(e)) => error_msg.set(e),
            }
        });
    });

    let enriched = move || {
        let idx: std::collections::HashMap<i64, serde_json::Value> = songs
            .get()
            .into_iter()
            .filter_map(|s| s["video_id"].as_i64().map(|id| (id, s)))
            .collect();
        items
            .get()
            .into_iter()
            .map(|it| {
                let video_id = it["video_id"].as_i64().unwrap_or(0);
                let meta = idx.get(&video_id).cloned().unwrap_or_default();
                (it, meta)
            })
            .collect::<Vec<_>>()
    };

    view! {
        <div class="live-setlist">
            <h2>"ytlive set list"</h2>
            <div class="live-setlist-error">{move || error_msg.get()}</div>
            <table class="live-setlist-table">
                <thead>
                    <tr>
                        <th>"#"</th>
                        <th>"Song"</th>
                        <th>"Artist"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=enriched
                        key=|(it, _)| it["video_id"].as_i64().unwrap_or(0)
                        children=move |(item, meta)| {
                            let position = item["position"].as_i64().unwrap_or(0);
                            let video_id = item["video_id"].as_i64().unwrap_or(0);
                            let song = meta["song"].as_str().unwrap_or("—").to_string();
                            let artist = meta["artist"].as_str().unwrap_or("—").to_string();
                            view! {
                                <tr>
                                    <td>{position + 1}</td>
                                    <td>{song}</td>
                                    <td>{artist}</td>
                                    <td>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                if let Err(e) = api::post_live_play_video(
                                                    playlist_id, video_id,
                                                ).await {
                                                    error_msg.set(e);
                                                }
                                            });
                                        }>"▶"</button>
                                        <button on:click=move |_| {
                                            leptos::task::spawn_local(async move {
                                                match api::delete_live_item(
                                                    playlist_id, video_id,
                                                ).await {
                                                    Ok(()) => on_changed.run(()),
                                                    Err(e) => error_msg.set(e),
                                                }
                                            });
                                        }>"✕"</button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
            <div class="live-setlist-controls">
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/play"),
                        ).await;
                    });
                }>"▶ Play"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/pause"),
                        ).await;
                    });
                }>"⏸"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/skip"),
                        ).await;
                    });
                }>"⏭"</button>
                <button on:click=move |_| {
                    leptos::task::spawn_local(async move {
                        let _ = api::post_empty(
                            &format!("/api/v1/playback/{playlist_id}/previous"),
                        ).await;
                    });
                }>"⏮"</button>
            </div>
        </div>
    }
}
```

If the playback endpoint paths in your tree differ from `/api/v1/playback/{id}/...`, open `crates/sp-server/src/api/mod.rs` and use whatever prefix the `routes::play` / `routes::pause` / `routes::skip` / `routes::previous` routes are currently registered under, then mirror that exact prefix here.

- [ ] **Step 3: Create the page**

Create `sp-ui/src/pages/live.rs`:

```rust
//! /live page: two-column catalog + set-list for the custom ytlive playlist.

use leptos::prelude::*;

use crate::api;
use crate::components::live_catalog::LiveCatalog;
use crate::components::live_setlist::LiveSetList;

#[component]
pub fn LivePage() -> impl IntoView {
    let ytlive_id = RwSignal::new(None::<i64>);
    let set_list_version = RwSignal::new(0u64);
    let error_msg = RwSignal::new(String::new());

    // Resolve the ytlive playlist id on mount.
    let _resolve = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match api::get::<Vec<serde_json::Value>>("/api/v1/playlists").await {
                Ok(all) => {
                    let yt = all.iter().find(|p| p["name"] == "ytlive").cloned();
                    if let Some(p) = yt {
                        if let Some(id) = p["id"].as_i64() {
                            ytlive_id.set(Some(id));
                        }
                    } else {
                        error_msg.set(
                            "ytlive playlist missing — migration V13 not applied?".to_string(),
                        );
                    }
                }
                Err(e) => error_msg.set(format!("failed to load playlists: {e}")),
            }
        });
    });

    let bump: Callback<()> = Callback::new(move |_| {
        set_list_version.update(|v| *v += 1);
    });
    let bump_after_add: Callback<i64> = Callback::new(move |_vid| {
        set_list_version.update(|v| *v += 1);
    });

    view! {
        <div class="live-page">
            <div class="live-page-error">{move || error_msg.get()}</div>
            {move || match ytlive_id.get() {
                None => view! { <div>"Loading ytlive playlist…"</div> }.into_any(),
                Some(id) => view! {
                    <div class="live-page-grid">
                        <LiveCatalog
                            target_playlist_id=id
                            _set_list_version=set_list_version.into()
                            on_added=bump_after_add
                        />
                        <LiveSetList
                            playlist_id=id
                            refresh=set_list_version.into()
                            on_changed=bump
                        />
                    </div>
                }.into_any(),
            }}
        </div>
    }
}
```

- [ ] **Step 4: Register module exports**

Append to `sp-ui/src/pages/mod.rs`:

```rust
pub mod live;
```

Append to `sp-ui/src/components/mod.rs`:

```rust
pub mod live_catalog;
pub mod live_setlist;
```

- [ ] **Step 5: Wire the Live page into the nav**

Replace the `enum Page` and `App` component in `sp-ui/src/app.rs`:

```rust
//! Root App component with tab-based navigation.

use leptos::prelude::*;

use crate::pages;
use crate::store::DashboardStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Live,
    Settings,
    Lyrics,
}

impl Default for Page {
    fn default() -> Self {
        Self::Dashboard
    }
}

#[component]
pub fn App() -> impl IntoView {
    let store = DashboardStore::new();
    provide_context(store);

    let page = RwSignal::new(Page::Dashboard);
    provide_context(page);

    crate::ws::connect(store);

    view! {
        <nav class="navbar">
            <span class="logo">"SongPlayer"</span>
            <button
                class:active=move || page.get() == Page::Dashboard
                on:click=move |_| page.set(Page::Dashboard)
            >"Dashboard"</button>
            <button
                class:active=move || page.get() == Page::Live
                on:click=move |_| page.set(Page::Live)
            >"Live"</button>
            <button
                class:active=move || page.get() == Page::Lyrics
                on:click=move |_| page.set(Page::Lyrics)
            >"Lyrics"</button>
            <button
                class:active=move || page.get() == Page::Settings
                on:click=move |_| page.set(Page::Settings)
            >"Settings"</button>
            <span class="ws-indicator">
                {move || if store.ws_connected.get() { "\u{1F7E2} WS" } else { "\u{1F534} WS" }}
            </span>
        </nav>
        <main class="content">
            {move || match page.get() {
                Page::Dashboard => pages::dashboard::DashboardPage().into_any(),
                Page::Live => pages::live::LivePage().into_any(),
                Page::Settings => pages::settings::SettingsPage().into_any(),
                Page::Lyrics => pages::lyrics::LyricsPage().into_any(),
            }}
        </main>
    }
}
```

- [ ] **Step 6: Build the frontend to confirm it compiles**

```
cd sp-ui && trunk build
```

Expected: success. If the playback endpoint paths do not match `/api/v1/playback/{id}/play` in the server registration, the set list's playback buttons will 404 at runtime but the build still succeeds — double-check against `crates/sp-server/src/api/mod.rs` routes before commit.

- [ ] **Step 7: Commit**

```bash
cd /home/newlevel/devel/songplayer
git add sp-ui/src/app.rs sp-ui/src/pages/mod.rs sp-ui/src/pages/live.rs \
        sp-ui/src/components/mod.rs sp-ui/src/components/live_catalog.rs \
        sp-ui/src/components/live_setlist.rs
git commit -m "feat(ui): /live page with catalog + set list + click-to-play controls"
```

---

## Task 11: Minimal CSS for the /live page

**Files:**
- Modify: `sp-ui/style.css`

- [ ] **Step 1: Append the styles**

Append to `sp-ui/style.css` (if the file has a different name, use `trunk build`'s output `dist/` to identify the actual style file, commonly `style.css` or `main.css`):

```css
.live-page-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 1rem;
    padding: 1rem;
}

.live-catalog-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 0.5rem;
}

.live-catalog-table, .live-setlist-table {
    width: 100%;
    border-collapse: collapse;
}

.live-catalog-table th,
.live-catalog-table td,
.live-setlist-table th,
.live-setlist-table td {
    border-bottom: 1px solid #333;
    padding: 0.25rem 0.5rem;
    text-align: left;
}

.live-setlist-controls {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.75rem;
}

.live-page-error,
.live-catalog-error,
.live-setlist-error {
    color: #f66;
    margin: 0.5rem 0;
    min-height: 1.2em;
}
```

- [ ] **Step 2: Rebuild and visually check**

```
cd sp-ui && trunk build
```

Expected: succeeds, `dist/` updated.

- [ ] **Step 3: Commit**

```bash
cd /home/newlevel/devel/songplayer
git add sp-ui/style.css
git commit -m "style(ui): minimal two-column layout for /live page"
```

---

## Task 12: Local sanity checks, format, push, and plan CI + manual smoke

**Files:**
- None (operational)

- [ ] **Step 1: Run workspace tests**

```
cargo test --workspace
```

Expected: all pass.

- [ ] **Step 2: Format check**

```
cargo fmt --all --check
```

Expected: no diff. If a diff appears, run `cargo fmt --all` and commit the formatting change:

```bash
git add -A
git commit -m "style: cargo fmt"
```

- [ ] **Step 3: Push to dev and monitor CI**

```
git push origin dev
```

Then monitor with the pattern from airuleset (one background sleep + `gh run view`, no `/loop` polling):

```bash
# Pick the latest run id
gh run list --branch dev --limit 1
# Watch it
Bash(command: "sleep 300 && gh run view <run-id> --json status,conclusion,jobs", run_in_background: true)
```

Expected: all jobs green (test, lint, mutation, build, deploy, post-deploy E2E).

- [ ] **Step 4: Verify migration V13 applied on win-resolume**

Via the `win-resolume` MCP:

```powershell
# On win-resolume, query the DB
$db = "C:\ProgramData\SongPlayer\songplayer.db"
# Use whichever SQLite binary the machine has (either sqlite3.exe or a Python one-liner)
python -c "import sqlite3; c = sqlite3.connect(r'$db'); print(c.execute('SELECT MAX(version) FROM schema_version').fetchone()); print(c.execute(\"SELECT name, kind, ndi_output_name FROM playlists WHERE name='ytlive'\").fetchone())"
```

Expected: `(13,)` and `('ytlive', 'custom', 'SP-live')`.

- [ ] **Step 5: Create the `ytlive` OBS scene + NDI input via MCP**

Use the `obs-resolume` MCP server (tools starting with `mcp__obs-resolume__`). The sequence:

1. `mcp__obs-resolume__obs-create-scene` with `{ "sceneName": "ytlive" }`.
2. Discover the SongPlayer machine's advertised NDI hostname. From the `win-resolume` MCP, run `hostname` via `mcp__win-resolume__Shell` — the NDI name will be `<HOSTNAME> (SP-live)`.
3. `mcp__obs-resolume__obs-create-input` with:
   - `sceneName: "ytlive"`
   - `inputName: "ytlive-ndi"`
   - `inputKind: "ndi_source"`
   - `inputSettings: { "ndi_source_name": "<HOSTNAME> (SP-live)" }`
4. Optionally, create a title text source in the same scene if the operator wants an OBS-side title overlay (same pattern as the existing `yt*` scenes).
5. Confirm by switching OBS to the `ytlive` scene — expect black (no video yet), and the SongPlayer dashboard's OBS status card should show `ytlive` as the program scene.

- [ ] **Step 6: Manual smoke test end-to-end**

On the operator's workstation with SongPlayer deployed:

1. Open the dashboard → click "Live" tab. Expect the two-pane layout, catalog populated.
2. Toggle "Only songs with lyrics" on; list shrinks to songs with `✓` in the Lyrics column.
3. Click `+ Add` on two different songs. Right pane updates to show both in order.
4. Make sure the `ytlive` scene is active in OBS.
5. Click `▶` on set-list row 1. Expect: NDI video appears on `SP-live`, title appears in Resolume (and optionally OBS text if configured), dashboard NowPlaying card for `ytlive` reflects the current song.
6. Click `▶` on row 2. Expect immediate jump — no auto-advance needed.
7. Click `⏭` Skip. Expect auto-advance to row 3 (if present) or "ended / stopped" (if the set list ends).
8. Click `✕` Remove on the currently-playing row. Expect the row disappears and the remaining rows compact. Playback of the current song continues; the next Skip should pick the item now at position `current_position + 1`.
9. Open the browser devtools console. Expect zero errors, zero warnings while the page loads and during any of the above interactions.

- [ ] **Step 7: Report results + open the PR**

If anything in Step 6 failed, file a follow-up fix commit and push; do not open the PR until the smoke test is clean. Once clean:

```bash
gh pr create --title "feat: custom ytlive playlist with click-to-play for live events" \
  --body "$(cat <<'EOF'
## Summary
- New custom playlist kind (`kind='custom'`) with `playlist_items` join table
- Pre-seeded `ytlive` playlist routing to NDI `SP-live` / OBS scene `ytlive`
- `EngineCommand::PlayVideo` jumps to a specific video and updates `current_position`
- New `/live` dashboard page with catalog + set list + click-to-play

## Test plan
- [x] Unit tests for migration V13, Playlist struct, selector branch, engine handler, HTTP handlers
- [x] Manual smoke on win-resolume: click-to-play, skip advances, remove compacts positions, zero console errors
- [ ] Playwright E2E — follow-up task AFTER tonight's event

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 8: Do NOT merge**

Per airuleset `pr-merge-policy`: merging requires an explicit user instruction. Post the PR URL and wait.

---

## Task 13 (post-event follow-up): Playwright E2E

**Files:**
- Create: `e2e/live-playlist.spec.ts`

Out of scope for tonight but MUST land before the PR is considered complete per airuleset `e2e-real-user-testing`. A new task file should cover: navigate to `/live`; filter by "has lyrics only"; add two songs; click play on the second one; assert the NowPlaying WS event fires with the correct `video_id`; remove a song; assert compaction; reload the page and assert the set list persisted. Zero console errors/warnings assertion mandatory. Create a GitHub issue `TODO: Playwright E2E for /live page` immediately after opening the PR so the gap is tracked.

---

## Verification

After Task 12 Step 6 completes cleanly:

1. `cargo test --workspace` passes on CI (Windows + Linux).
2. `cargo fmt --all --check` clean.
3. `cargo mutants --in-diff` on the PR shows zero surviving mutants (I/O-only handlers are annotated with `#[cfg_attr(test, mutants::skip)]` + a one-line reason).
4. `trunk build --release` produces a `dist/` under 3 MB.
5. CI deploy lands the new binary on win-resolume.
6. Migration V13 on-disk confirmed via the `win-resolume` MCP.
7. OBS `ytlive` scene created and confirmed as "program" by dashboard status card.
8. All manual smoke steps succeed with zero browser console errors/warnings.
9. PR is `mergeable: true` and `mergeable_state: "clean"`.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-17-live-playlist-click-to-play.md`.

**Scope tradeoff reminder:** PR #38 (lyrics ensemble work) is still open on `dev`. Two options for how this ships:

- **A (default):** commit these tasks directly on `dev` while #38 is still open. Both bodies of work ride into the same release when #38 merges (or when this PR merges, whichever is second). Only safe if both PRs are independently green.
- **B:** wait for #38 to merge first, then open a separate PR for this feature. Slower but isolates risk.

Choose A for speed, B for cleanliness. The operator can decide before execution starts.

**Execution modes:**

1. **Subagent-Driven (recommended — default per airuleset)** — I dispatch a fresh subagent per task, two-stage review between tasks, fast iteration. Per airuleset `ask-before-assuming` the fixed answer is "Subagent"; I will proceed with this mode unless told otherwise.

2. **Inline Execution** — execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints for review.

Which approach?
