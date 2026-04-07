//! Database layer — SQLite pool creation and manual migration system.

pub mod models;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

/// All migrations as (version, SQL) tuples.
/// Each SQL string may contain multiple statements separated by semicolons.
const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1)];

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

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqlitePool {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn pool_creation_and_migration() {
        let pool = setup().await;
        let ver = current_schema_version(&pool).await.unwrap();
        assert_eq!(ver, 1);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap(); // second run must not fail
        let ver = current_schema_version(&pool).await.unwrap();
        assert_eq!(ver, 1);
    }

    #[tokio::test]
    async fn migration_creates_all_tables() {
        let pool = setup().await;
        let tables: Vec<String> =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();

        for expected in &[
            "playlists",
            "videos",
            "play_history",
            "settings",
            "resolume_hosts",
            "resolume_clip_mappings",
        ] {
            assert!(
                tables.contains(&expected.to_string()),
                "missing table: {expected}"
            );
        }
    }

    #[tokio::test]
    async fn crud_playlist_and_videos() {
        let pool = setup().await;

        // Insert playlist
        let res =
            sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES (?, ?) RETURNING id")
                .bind("Test Playlist")
                .bind("https://youtube.com/playlist?list=PLtest")
                .fetch_one(&pool)
                .await
                .unwrap();
        let playlist_id: i64 = res.get("id");
        assert!(playlist_id > 0);

        // Insert video
        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title) VALUES (?, ?, ?)")
            .bind(playlist_id)
            .bind("dQw4w9WgXcQ")
            .bind("Never Gonna Give You Up")
            .execute(&pool)
            .await
            .unwrap();

        // Read back
        let row = sqlx::query("SELECT title FROM videos WHERE playlist_id = ?")
            .bind(playlist_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let title: String = row.get("title");
        assert_eq!(title, "Never Gonna Give You Up");
    }

    #[tokio::test]
    async fn unique_index_enforcement() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO videos (playlist_id, youtube_id) VALUES (1, 'abc')")
            .execute(&pool)
            .await
            .unwrap();

        // Duplicate should fail
        let res = sqlx::query("INSERT INTO videos (playlist_id, youtube_id) VALUES (1, 'abc')")
            .execute(&pool)
            .await;
        assert!(res.is_err(), "duplicate should violate unique index");
    }

    #[tokio::test]
    async fn foreign_key_cascade_delete() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO videos (playlist_id, youtube_id) VALUES (1, 'vid1')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO play_history (playlist_id, video_id) VALUES (1, 1)")
            .execute(&pool)
            .await
            .unwrap();

        // Delete playlist — should cascade to videos and play_history
        sqlx::query("DELETE FROM playlists WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        let video_count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM videos")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
        assert_eq!(video_count, 0);

        let history_count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM play_history")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
        assert_eq!(history_count, 0);
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let pool = setup().await;

        models::set_setting(&pool, "obs_url", "ws://127.0.0.1:4455")
            .await
            .unwrap();

        let val = models::get_setting(&pool, "obs_url").await.unwrap();
        assert_eq!(val, Some("ws://127.0.0.1:4455".to_string()));

        // Update
        models::set_setting(&pool, "obs_url", "ws://10.0.0.1:4455")
            .await
            .unwrap();
        let val = models::get_setting(&pool, "obs_url").await.unwrap();
        assert_eq!(val, Some("ws://10.0.0.1:4455".to_string()));

        // Non-existent key
        let missing = models::get_setting(&pool, "nope").await.unwrap();
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn model_query_roundtrips() {
        let pool = setup().await;

        // Insert a playlist via models
        let playlist = models::insert_playlist(&pool, "Worship", "https://yt.com/pl1")
            .await
            .unwrap();
        assert_eq!(playlist.name, "Worship");
        assert!(playlist.enabled);

        // Get active playlists
        let active = models::get_active_playlists(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, playlist.id);

        // Upsert video
        let video = models::upsert_video(&pool, playlist.id, "abc123", Some("My Song"))
            .await
            .unwrap();
        assert_eq!(video.youtube_id, "abc123");
        assert_eq!(video.title, "My Song");

        // Upsert same video with updated title
        let video2 = models::upsert_video(&pool, playlist.id, "abc123", Some("Updated"))
            .await
            .unwrap();
        assert_eq!(video2.id, video.id); // same row
        assert_eq!(video2.title, "Updated");

        // Get videos for playlist
        let videos = models::get_videos_for_playlist(&pool, playlist.id)
            .await
            .unwrap();
        assert_eq!(videos.len(), 1);

        // Record play
        models::record_play(&pool, playlist.id, video.id)
            .await
            .unwrap();

        // Get unplayed — should be empty now
        let unplayed = models::get_unplayed_video_ids(&pool, playlist.id)
            .await
            .unwrap();
        assert!(unplayed.is_empty());

        // Add second video, should be unplayed
        models::upsert_video(&pool, playlist.id, "def456", Some("Song 2"))
            .await
            .unwrap();
        let unplayed = models::get_unplayed_video_ids(&pool, playlist.id)
            .await
            .unwrap();
        assert_eq!(unplayed.len(), 1);
        assert_eq!(unplayed[0], "def456");
    }
}
