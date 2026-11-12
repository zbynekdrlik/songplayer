//! #265: the node's song block list (V44 `blocked_songs`).
//!
//! A song is blocked by its YouTube id, so every playlist's row of it at
//! once; the selection leaves it out through `db::pickable_row!`. The table
//! is this node's own: the exchange carries no blocks.

use sp_core::song_block::{BlockedSong, shown_title};
use sqlx::{Row, SqlitePool};

/// Every blocked song, the newest block first.
pub async fn list(pool: &SqlitePool) -> Result<Vec<BlockedSong>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT youtube_id, title, blocked_at_ms FROM blocked_songs \
         ORDER BY blocked_at_ms DESC, youtube_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(blocked_song).collect())
}

fn blocked_song(r: &sqlx::sqlite::SqliteRow) -> BlockedSong {
    BlockedSong {
        youtube_id: r.get("youtube_id"),
        title: r.get("title"),
        blocked_at_ms: r.get("blocked_at_ms"),
    }
}

/// The song as the wall shows it, from a row of `youtube_id` on this node
/// (a row with a song name first, then the oldest; never the #228 test
/// item). `None` = this node has no such song.
pub async fn song_title(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(concat!(
        "SELECT song, artist, title FROM videos WHERE youtube_id = ? AND ",
        crate::test_item::not_test_item!(),
        " ORDER BY (COALESCE(TRIM(song), '') = ''), id LIMIT 1"
    ))
    .bind(youtube_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        shown_title(
            r.get::<Option<String>, _>("song").as_deref(),
            r.get::<Option<String>, _>("artist").as_deref(),
            &r.get::<Option<String>, _>("title").unwrap_or_default(),
        )
    }))
}

/// Block `youtube_id` at `now_ms` with `title`. A song already blocked keeps
/// its first time and title. Returns the stored row and whether this call
/// added it.
pub async fn block(
    pool: &SqlitePool,
    youtube_id: &str,
    title: &str,
    now_ms: i64,
) -> Result<(BlockedSong, bool), sqlx::Error> {
    let added = sqlx::query(
        "INSERT INTO blocked_songs (youtube_id, blocked_at_ms, title) VALUES (?, ?, ?) \
         ON CONFLICT (youtube_id) DO NOTHING",
    )
    .bind(youtube_id)
    .bind(now_ms)
    .bind(title)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    let row = sqlx::query(
        "SELECT youtube_id, title, blocked_at_ms FROM blocked_songs WHERE youtube_id = ?",
    )
    .bind(youtube_id)
    .fetch_one(pool)
    .await?;
    Ok((blocked_song(&row), added))
}

/// Unblock `youtube_id`: its title when it was blocked, `None` when not.
pub async fn unblock(pool: &SqlitePool, youtube_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM blocked_songs WHERE youtube_id = ? RETURNING title")
        .bind(youtube_id)
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
#[path = "models_blocked_tests.rs"]
mod tests;
