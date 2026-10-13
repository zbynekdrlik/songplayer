//! #260: a row's membership in its YouTube playlist (V40 `removed_at`).
//!
//! The playlist sync marks every row whose song left the YouTube playlist
//! (`removed_at` = when, UTC ms) and clears the mark of every song it lists
//! again (`playlist::membership`). A marked row is never picked to play,
//! never on the dashboard's list and never taken by a worker queue
//! (`crate::db::queue_row!`); its files, stems, lyrics and dub stay, so a
//! song put back plays again at once. A year after its mark the retention
//! deletes it with its own data and files (`playlist::retention`).

use sqlx::{Row, SqlitePool};

/// A row of a playlist as the sync compares it with the fetched list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub youtube_id: String,
    pub title: Option<String>,
    /// Marked: its song is not in the YouTube playlist (`removed_at` set).
    pub removed: bool,
}

/// Every row of `playlist_id`, marked or not, in id order.
pub async fn members(pool: &SqlitePool, playlist_id: i64) -> Result<Vec<Member>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT youtube_id, title, removed_at FROM videos WHERE playlist_id = ? ORDER BY id",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Member {
            youtube_id: r.get("youtube_id"),
            title: r.get("title"),
            removed: r.get::<Option<i64>, _>("removed_at").is_some(),
        })
        .collect())
}

/// Mark `youtube_id`'s row of `playlist_id` as gone from the YouTube
/// playlist at `now_ms`. A row already marked keeps its first mark: the
/// retention's year counts from the day the song left.
pub async fn mark_removed(
    pool: &SqlitePool,
    playlist_id: i64,
    youtube_id: &str,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET removed_at = ? \
         WHERE playlist_id = ? AND youtube_id = ? AND removed_at IS NULL",
    )
    .bind(now_ms)
    .bind(playlist_id)
    .bind(youtube_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Membership + readiness check for the `videos.playlist_id` model used by
/// youtube-kind playlists (issue #134) — the direct-link counterpart of
/// `position_for_playlist_item`'s `playlist_items` set-list model that only
/// custom-kind playlists use. Returns `Some(normalized)` when `video_id`
/// belongs to `playlist_id`, `None` when it doesn't exist or belongs to a
/// different playlist (prevents a client from triggering playback of an
/// arbitrary video via another playlist's URL — same protection
/// `position_for_playlist_item` gives the custom-playlist path). #260: a row
/// whose song left the YouTube playlist is not in it any more (`None`).
pub async fn video_playlist_membership(
    pool: &SqlitePool,
    playlist_id: i64,
    video_id: i64,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT normalized FROM videos WHERE id = ? AND playlist_id = ? AND removed_at IS NULL",
    )
    .bind(video_id)
    .bind(playlist_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
#[path = "models_membership_tests.rs"]
mod tests;
