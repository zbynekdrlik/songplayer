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

const MIGRATION_V6: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
";

const MIGRATION_V7: &str = "
UPDATE videos SET has_lyrics = 0, lyrics_source = NULL;
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
        assert_eq!(ver, 7);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap(); // second run must not fail
        let ver = current_schema_version(&pool).await.unwrap();
        assert_eq!(ver, 7);
    }

    #[tokio::test]
    async fn migration_v4_adds_audio_file_path_column() {
        let pool = setup().await;
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(videos)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(
            cols.contains(&"audio_file_path".to_string()),
            "audio_file_path column should exist, columns: {cols:?}"
        );
    }

    #[tokio::test]
    async fn migration_v4_resets_all_normalized_rows() {
        let pool = create_memory_pool().await.unwrap();
        // Apply V1 + V2 + V3 manually so we can seed data before V4.
        for &(version, sql) in &MIGRATIONS[..3] {
            // Ensure schema_version table exists so the INSERT below works.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
                )",
            )
            .execute(&pool)
            .await
            .unwrap();

            let mut tx = pool.begin().await.unwrap();
            for stmt in sql.split(';') {
                let s = stmt.trim();
                if !s.is_empty() {
                    sqlx::query(s).execute(&mut *tx).await.unwrap();
                }
            }
            sqlx::query("INSERT INTO schema_version (version) VALUES (?)")
                .bind(version)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        // Seed a playlist and a normalized video.
        sqlx::query(
            "INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('p', 'u', 'n')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'abc', 1, '/tmp/foo.mp4')")
            .execute(&pool)
            .await
            .unwrap();

        // Now apply V4.
        run_migrations(&pool).await.unwrap();

        // Row's normalized must have been reset to 0.
        let n: i64 = sqlx::query("SELECT normalized FROM videos WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("normalized");
        assert_eq!(n, 0, "V4 must reset normalized=0 for every existing row");
    }

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
        assert!(
            !cols.contains(&"obs_text_source".to_string()),
            "obs_text_source column should be dropped, columns: {cols:?}"
        );
        assert!(
            !cols.contains(&"resolume_title_token".to_string()),
            "resolume_title_token column should be dropped, columns: {cols:?}"
        );
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
        assert_eq!(playlist.youtube_url, "https://yt.com/pl1");
        assert!(playlist.is_active);

        // Get active playlists — verify all fields are populated
        let active = models::get_active_playlists(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, playlist.id);
        assert_eq!(active[0].name, "Worship");
        assert_eq!(active[0].youtube_url, "https://yt.com/pl1");
        assert!(active[0].is_active);

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

    #[tokio::test]
    async fn get_video_file_path_returns_none_for_unnormalized() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO videos (playlist_id, youtube_id) VALUES (1, 'vid1')")
            .execute(&pool)
            .await
            .unwrap();

        // Not normalized → should return None.
        let path = models::get_video_file_path(&pool, 1).await.unwrap();
        assert!(path.is_none());
    }

    #[tokio::test]
    async fn get_video_file_path_returns_path_for_normalized() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, normalized, file_path) VALUES (1, 'vid1', 1, '/cache/song.mp4')")
            .execute(&pool)
            .await
            .unwrap();

        let path = models::get_video_file_path(&pool, 1).await.unwrap();
        assert_eq!(path, Some("/cache/song.mp4".to_string()));
    }

    #[tokio::test]
    async fn get_video_metadata_returns_song_and_artist() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url) VALUES ('P', 'url')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, song, artist) VALUES (1, 'vid1', 'My Song', 'Artist Name')")
            .execute(&pool)
            .await
            .unwrap();

        let meta = models::get_video_metadata(&pool, 1).await.unwrap();
        assert_eq!(
            meta,
            Some(("My Song".to_string(), "Artist Name".to_string()))
        );
    }

    #[tokio::test]
    async fn migration_v5_adds_lyrics_columns() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        // Verify videos.has_lyrics column exists
        let row = sqlx::query_scalar::<_, i64>("SELECT has_lyrics FROM videos LIMIT 0")
            .fetch_optional(&pool)
            .await;
        assert!(row.is_ok());
        // Verify playlists.karaoke_enabled column exists
        let row = sqlx::query_scalar::<_, i64>("SELECT karaoke_enabled FROM playlists LIMIT 0")
            .fetch_optional(&pool)
            .await;
        assert!(row.is_ok());
    }

    #[tokio::test]
    async fn get_next_video_without_lyrics_returns_unprocessed() {
        let pool = create_memory_pool().await.unwrap();
        run_migrations(&pool).await.unwrap();
        // Insert a playlist
        sqlx::query("INSERT INTO playlists (name, youtube_url, is_active) VALUES ('test', 'https://youtube.com/playlist?list=test', 1)")
            .execute(&pool).await.unwrap();
        // Insert a normalized video without lyrics
        sqlx::query("INSERT INTO videos (playlist_id, youtube_id, title, normalized, has_lyrics) VALUES (1, 'abc12345678', 'Test Song', 1, 0)")
            .execute(&pool).await.unwrap();
        let row = models::get_next_video_without_lyrics(&pool).await.unwrap();
        assert!(row.is_some());
        let row = row.unwrap();
        assert_eq!(row.youtube_id, "abc12345678");
        // Mark as having lyrics
        models::mark_video_lyrics(&pool, row.id, true, Some("lrclib"))
            .await
            .unwrap();
        let row2 = models::get_next_video_without_lyrics(&pool).await.unwrap();
        assert!(row2.is_none());
    }

    #[tokio::test]
    async fn get_active_playlists_includes_ndi_name() {
        let pool = setup().await;

        sqlx::query("INSERT INTO playlists (name, youtube_url, ndi_output_name) VALUES ('P', 'url', 'SP-test')")
            .execute(&pool)
            .await
            .unwrap();

        let playlists = models::get_active_playlists(&pool).await.unwrap();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].ndi_output_name, "SP-test");
    }
}
