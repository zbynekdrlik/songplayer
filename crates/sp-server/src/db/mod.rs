//! Database layer — SQLite pool creation and manual migration system.

pub mod models;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

/// All migrations as (version, SQL) tuples.
/// Each SQL string may contain multiple statements separated by semicolons.
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
    (14, MIGRATION_V14),
    (15, MIGRATION_V15),
    (16, MIGRATION_V16),
    (17, MIGRATION_V17),
    (18, MIGRATION_V18),
];

const MIGRATION_V1: &str = "
CREATE TABLE playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    youtube_url TEXT NOT NULL,
    ndi_output_name TEXT NOT NULL DEFAULT '',
    obs_text_source TEXT,
    playback_mode TEXT NOT NULL DEFAULT 'continuous',
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE videos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    youtube_id TEXT NOT NULL,
    title TEXT,
    song TEXT,
    artist TEXT,
    metadata_source TEXT,
    gemini_failed INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    file_path TEXT,
    normalized INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX idx_videos_playlist_youtube ON videos(playlist_id, youtube_id);

CREATE TABLE play_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    video_id INTEGER NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    played_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_play_history_playlist ON play_history(playlist_id, played_at);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE resolume_hosts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    host TEXT NOT NULL,
    port INTEGER NOT NULL DEFAULT 8090,
    is_enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE resolume_clip_mappings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    host_id INTEGER NOT NULL REFERENCES resolume_hosts(id) ON DELETE CASCADE,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    clip_token TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_clip_mappings_unique ON resolume_clip_mappings(host_id, playlist_id, clip_token);
";

const MIGRATION_V2: &str = "
ALTER TABLE playlists ADD COLUMN resolume_title_token TEXT NOT NULL DEFAULT '';
";

const MIGRATION_V3: &str = "
ALTER TABLE playlists DROP COLUMN obs_text_source;
ALTER TABLE playlists DROP COLUMN resolume_title_token;
";

const MIGRATION_V4: &str = "
ALTER TABLE videos ADD COLUMN audio_file_path TEXT;
UPDATE videos SET normalized = 0;
";

const MIGRATION_V5: &str = "
ALTER TABLE videos ADD COLUMN has_lyrics INTEGER NOT NULL DEFAULT 0;
ALTER TABLE videos ADD COLUMN lyrics_source TEXT;
ALTER TABLE playlists ADD COLUMN karaoke_enabled INTEGER NOT NULL DEFAULT 1;
";

// V6 resets all lyrics after disabling YouTube auto-subs source.
// Forces reprocessing with LRCLIB-only (clean lyrics).
const MIGRATION_V6: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
";

// V7 = re-run of V6's lyrics reset.
//
// V6 ran on machines before the startup.rs stale-file cleanup landed.
// On those machines, normalized rows were re-linked to stale lyrics files
// that still existed on disk, short-circuiting the reprocessing intent.
// V7 forces another reset now that startup actually deletes those files.
const MIGRATION_V7: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
";

// V8 (historical) downgraded rows whose lyrics_source combined lrclib with
// whole-song Qwen3 output back to plain 'lrclib' so the retroactive-alignment
// loop (since removed) could re-run them through the vocal-isolation path.
// V9 supersedes V8 by resetting every row unconditionally.
const MIGRATION_V8: &str = "
UPDATE videos SET lyrics_source = 'lrclib' WHERE lyrics_source LIKE 'lrclib+qwen3%';
";

// V9 = reset all lyrics rows to re-process them through the new
// YT-subs-first pipeline. Retires 'lrclib+qwen3' (whole-song alignment)
// in favour of 'yt_subs+qwen3' (chunked) or plain 'lrclib' (line-level
// fallback). Idempotent: a row already at (0, NULL) is a no-op.
const MIGRATION_V9: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
";

// V10 = re-reset rows that fell into the partial 'yt_subs' state during
// the first deploy of v0.16.x — bootstrap failed there, so the YT-subs
// fetch succeeded but chunked alignment never ran and the rows persisted
// as (has_lyrics=1, lyrics_source='yt_subs'). The worker uses
// `get_next_video_for_lyrics` (3-bucket priority queue) which filters on
// has_lyrics=0, so without V10 those rows would never be re-picked-up.
// Reset them so the now-fixed alignment pipeline gets a second shot.
// Scoped to 'yt_subs' rows only — LRCLIB-line-level rows are correct
// as-is and shouldn't pay another reprocessing cycle.
const MIGRATION_V10: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE lyrics_source = 'yt_subs';
";

// V11 = reset 'yt_subs+qwen3' rows so they re-run through the long-line-
// splitting chunking introduced to fix #119 Housefires (32-word SRT
// events collapsed 27 words onto the same start_ms). Scoped to that
// source only — LRCLIB-line-level rows are unaffected and don't pay
// another reprocessing cycle.
const MIGRATION_V11: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL WHERE lyrics_source = 'yt_subs+qwen3';
";

// V12 adds pipeline version tracking + quality score + manual reprocess priority.
// Defaults: pipeline_version=0 (routes every existing row into the stale bucket
// when LYRICS_PIPELINE_VERSION >= 1), quality_score=NULL (NULLS FIRST treats
// them as worst), manual_priority=0 (not user-triggered).
const MIGRATION_V12: &str = "
ALTER TABLE videos ADD COLUMN lyrics_pipeline_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE videos ADD COLUMN lyrics_quality_score REAL;
ALTER TABLE videos ADD COLUMN lyrics_manual_priority INTEGER NOT NULL DEFAULT 0;
";

// V13 introduces the "custom" playlist kind for the Live/DJ-style set list.
//
// - `kind` text defaults to 'youtube' so every existing playlist keeps its
//   behavior. `current_position` is only meaningful for kind='custom' and
//   tracks which item in the set list was last played (so Skip advances).
// - `playlist_items` stores ordered references to existing videos. Videos
//   themselves still live under their *home* youtube playlist; this table
//   just names positions.
// - The 'ytlive' seed row is intentionally NOT inserted here. Seeding it
//   in the migration caused pre-existing tests (which call run_migrations on
//   an empty pool and count rows or hard-code playlist_id=1) to regress.
//   The row is created by `startup::ensure_live_playlist_exists` instead,
//   which runs whenever the server actually starts.
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
";

// V14 adds per-song suppress_resolume_en flag. Songs whose YouTube video
// has lyrics baked in (visual subtitles inside the video frame) set this
// to 1 to tell Resolume to skip the #sp-subs EN push — otherwise the same
// line shows twice on the wall. SK subs + Presenter current_text remain
// unaffected.
const MIGRATION_V14: &str = "
ALTER TABLE videos ADD COLUMN suppress_resolume_en INTEGER NOT NULL DEFAULT 0;
";

// V15 — operator-provided lyrics override. For songs where YouTube has
// no manual subs, no lyrics in description, and no LRCLIB match, the
// Gemini alignment path has no reference text and the song ships as
// `source=no_source`. Giving operators a field to paste lyrics text
// unblocks Gemini alignment for those songs without cache-file hacks.
// The worker's `gather_sources` picks this up as the highest-priority
// candidate when non-empty.
const MIGRATION_V15: &str = "
ALTER TABLE videos ADD COLUMN lyrics_override_text TEXT;
";

// V16 — per-song lyrics time-axis shift. Applied at render time so the
// operator can correct systematic lead/lag on a single song without
// reprocessing (e.g. Gemini's uniform-duration hallucinations observed
// during the 2026-04-23 event). Signed: positive = delay display
// (effectively shorter lead), negative = advance display (longer lead).
// Defaults to 0 for existing rows so untouched songs behave identically.
const MIGRATION_V16: &str = "
ALTER TABLE videos ADD COLUMN lyrics_time_offset_ms INTEGER NOT NULL DEFAULT 0;
";

// V17 — Spotify track ID for Tier-1 SpotifyLyricsFetcher. Manually
// assigned per video for line-synced lyrics via public proxy. NULL when
// not set; fetcher silently skips when None.
// Note: migration runner enforces idempotency via schema_version;
// this raw ALTER would error if applied twice manually.
const MIGRATION_V17: &str = "
ALTER TABLE videos ADD COLUMN spotify_track_id TEXT;
";

const MIGRATION_V18: &str = "
ALTER TABLE videos ADD COLUMN spotify_resolved_at TEXT;
UPDATE videos SET spotify_resolved_at = datetime('now') WHERE spotify_track_id IS NOT NULL;
";

/// Create a connection pool backed by a file.
pub async fn create_pool(path: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(path)?
        .create_if_missing(true)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
}

/// Create an in-memory pool (for tests).
pub async fn create_memory_pool() -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    // Single connection so the in-memory DB persists for the pool's lifetime.
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
}

/// Run all pending migrations inside transactions.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Ensure the schema_version table exists.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    for &(version, sql) in MIGRATIONS {
        let row = sqlx::query("SELECT version FROM schema_version WHERE version = ?")
            .bind(version)
            .fetch_optional(pool)
            .await?;
        if row.is_some() {
            continue; // already applied
        }

        // Execute each statement in a transaction.
        let mut tx = pool.begin().await?;
        for stmt in sql.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            sqlx::query(stmt).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
            .bind(version)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }

    Ok(())
}

/// Helper: return current schema version (0 if no migrations applied).
pub async fn current_schema_version(pool: &SqlitePool) -> Result<i32, sqlx::Error> {
    let row = sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version")
        .fetch_one(pool)
        .await?;
    Ok(row.get("v"))
}

#[path = "mod_tests.rs"]
#[cfg(test)]
mod tests;

#[path = "mod_tests_v18.rs"]
#[cfg(test)]
mod tests_v18;
