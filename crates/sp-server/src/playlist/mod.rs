//! Playlist sync — fetch video IDs from YouTube via yt-dlp and upsert to DB.

pub mod selector;

use sqlx::SqlitePool;
use std::path::Path;

/// Sync a YouTube playlist: fetch video IDs via yt-dlp, upsert to DB.
/// Returns count of new videos found.
pub async fn sync_playlist(
    pool: &SqlitePool,
    playlist_id: i64,
    youtube_url: &str,
    ytdlp_path: &Path,
) -> Result<usize, anyhow::Error> {
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    cmd.args([
        "--flat-playlist",
        "--dump-json",
        "--no-warnings",
        "--js-runtimes",
        "node",
        youtube_url,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    crate::downloader::hide_console_window(&mut cmd);
    let output = cmd.output().await?;

    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp exited with status {} for playlist {}",
            output.status,
            youtube_url
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries = parse_ndjson(&stdout)?;

    let mut new_count = 0;
    for entry in &entries {
        let was_new = upsert_entry(pool, playlist_id, entry).await?;
        if was_new {
            new_count += 1;
        }
    }

    Ok(new_count)
}

/// A single entry parsed from yt-dlp's NDJSON output.
#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub id: String,
    pub title: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Parse NDJSON (one JSON object per line) into playlist entries.
pub fn parse_ndjson(input: &str) -> Result<Vec<PlaylistEntry>, anyhow::Error> {
    let mut entries = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = serde_json::from_str(line)?;
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'id' field in yt-dlp JSON line"))?
            .to_string();
        let title = obj.get("title").and_then(|v| v.as_str()).map(String::from);
        let duration_ms = obj
            .get("duration")
            .and_then(|v| v.as_f64())
            .map(|secs| (secs * 1000.0) as i64);
        entries.push(PlaylistEntry {
            id,
            title,
            duration_ms,
        });
    }
    Ok(entries)
}

/// Upsert a single entry, returning true if it was newly inserted.
async fn upsert_entry(
    pool: &SqlitePool,
    playlist_id: i64,
    entry: &PlaylistEntry,
) -> Result<bool, sqlx::Error> {
    // Check if the video already exists.
    let existing = sqlx::query("SELECT id FROM videos WHERE playlist_id = ? AND youtube_id = ?")
        .bind(playlist_id)
        .bind(&entry.id)
        .fetch_optional(pool)
        .await?;

    if existing.is_some() {
        // Update title and duration if provided.
        sqlx::query(
            "UPDATE videos SET title = COALESCE(?, title), duration_ms = COALESCE(?, duration_ms)
             WHERE playlist_id = ? AND youtube_id = ?",
        )
        .bind(&entry.title)
        .bind(entry.duration_ms)
        .bind(playlist_id)
        .bind(&entry.id)
        .execute(pool)
        .await?;
        Ok(false)
    } else {
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, duration_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(playlist_id)
        .bind(&entry.id)
        .bind(&entry.title)
        .bind(entry.duration_ms)
        .fetch_optional(pool)
        .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ndjson_valid() {
        let input = r#"{"id": "abc123", "title": "Cool Song", "duration": 180.5}
{"id": "def456", "title": "Another One", "duration": 240}
{"id": "ghi789", "title": null, "duration": null}"#;

        let entries = parse_ndjson(input).unwrap();
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].id, "abc123");
        assert_eq!(entries[0].title.as_deref(), Some("Cool Song"));
        assert_eq!(entries[0].duration_ms, Some(180_500));

        assert_eq!(entries[1].id, "def456");
        assert_eq!(entries[1].title.as_deref(), Some("Another One"));
        assert_eq!(entries[1].duration_ms, Some(240_000));

        assert_eq!(entries[2].id, "ghi789");
        assert_eq!(entries[2].title, None);
        assert_eq!(entries[2].duration_ms, None);
    }

    #[test]
    fn parse_ndjson_empty_lines_ignored() {
        let input = "\n{\"id\": \"a\", \"title\": \"T\"}\n\n";
        let entries = parse_ndjson(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "a");
    }

    #[test]
    fn parse_ndjson_missing_id_fails() {
        let input = r#"{"title": "No ID"}"#;
        let result = parse_ndjson(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ndjson_empty_input() {
        let entries = parse_ndjson("").unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn sync_upserts_entries() {
        use crate::db;

        let pool = db::create_memory_pool().await.unwrap();
        db::run_migrations(&pool).await.unwrap();

        // Create a playlist.
        let playlist = db::models::insert_playlist(&pool, "Test", "https://yt.com/pl")
            .await
            .unwrap();

        // Simulate parsed entries by calling upsert_entry directly.
        let entry1 = PlaylistEntry {
            id: "vid1".to_string(),
            title: Some("Song 1".to_string()),
            duration_ms: Some(120_000),
        };
        let entry2 = PlaylistEntry {
            id: "vid2".to_string(),
            title: Some("Song 2".to_string()),
            duration_ms: None,
        };

        let was_new = upsert_entry(&pool, playlist.id, &entry1).await.unwrap();
        assert!(was_new);

        let was_new = upsert_entry(&pool, playlist.id, &entry2).await.unwrap();
        assert!(was_new);

        // Upsert same entry again — should not be new.
        let was_new = upsert_entry(&pool, playlist.id, &entry1).await.unwrap();
        assert!(!was_new);

        // Verify DB state.
        let videos = db::models::get_videos_for_playlist(&pool, playlist.id)
            .await
            .unwrap();
        assert_eq!(videos.len(), 2);
    }
}
