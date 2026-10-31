//! #260: a row's membership in its YouTube playlist (V40 `removed_at`).
//!
//! The playlist sync marks every row whose song left the YouTube playlist
//! (`removed_at` = when, UTC ms) and clears the mark of every song it lists
//! again (`playlist::membership`). A marked row is never picked to play,
//! never on the dashboard's list and never taken by a worker queue
//! (`crate::db::queue_row!`); its files, stems, lyrics and dub stay, so a
//! song put back plays again at once. A year after its mark the retention
//! deletes it with its own data and files (`playlist::retention`).
//!
//! #261: a playlist's deactivation marks its rows the same way
//! ([`deactivate`]) and its reactivation clears exactly those marks
//! ([`reactivate`]), so an inactive playlist's songs go a year later too.

use std::path::{Path, PathBuf};

use sqlx::{Row, SqliteConnection, SqlitePool};

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

/// Whether `playlist_id` is active; `false` for no such playlist (#261: an
/// inactive playlist's sync records nothing, `playlist::membership`).
pub async fn playlist_active(pool: &SqlitePool, playlist_id: i64) -> Result<bool, sqlx::Error> {
    let active: Option<i64> = sqlx::query_scalar("SELECT is_active FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_optional(pool)
        .await?;
    Ok(active == Some(1))
}

/// #261: a playlist's deactivation starts #260's clock on its rows. The
/// playlist keeps its FIRST `deactivated_at` (V43; `now_ms` unless it is
/// inactive already), and every row not marked yet gets exactly that value
/// as its `removed_at`; a row the sync marked keeps its mark. Never the
/// test item's playlist (its pinned clips are no songs to delete). On
/// `conn`, the caller's transaction with the `is_active` write. The rows
/// it marked.
pub async fn deactivate(
    conn: &mut SqliteConnection,
    playlist_id: i64,
    now_ms: i64,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE playlists SET deactivated_at = COALESCE(deactivated_at, ?) \
         WHERE id = ? AND kind != 'test'",
    )
    .bind(now_ms)
    .bind(playlist_id)
    .execute(&mut *conn)
    .await?;
    let marked = sqlx::query(
        "UPDATE videos SET removed_at = (SELECT deactivated_at FROM playlists WHERE id = ?1) \
         WHERE playlist_id = ?1 AND removed_at IS NULL \
           AND (SELECT deactivated_at FROM playlists WHERE id = ?1) IS NOT NULL",
    )
    .bind(playlist_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(marked)
}

/// #261: a reactivation clears exactly the marks its deactivation set
/// (`removed_at` = the playlist's `deactivated_at`: [`deactivate`] gives
/// every mark that very value, so a sync's mark never equals it, even
/// after a clock step), then the column: a song the sync marked before it
/// stays marked, so it is never picked again before the next sync, and its
/// year keeps counting. On `conn`, the caller's transaction. The rows it
/// cleared.
pub async fn reactivate(conn: &mut SqliteConnection, playlist_id: i64) -> Result<u64, sqlx::Error> {
    let cleared = sqlx::query(
        "UPDATE videos SET removed_at = NULL WHERE playlist_id = ?1 \
         AND removed_at = (SELECT deactivated_at FROM playlists WHERE id = ?1)",
    )
    .bind(playlist_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query("UPDATE playlists SET deactivated_at = NULL WHERE id = ?")
        .bind(playlist_id)
        .execute(&mut *conn)
        .await?;
    Ok(cleared)
}

/// #261: what an `is_active` write does to the playlist's rows, on `conn`
/// (the PUT's transaction, `routes::update_playlist`): a deactivation marks
/// them at `now_ms` ([`deactivate`]), a reactivation clears those marks
/// ([`reactivate`]). The rows it marked or cleared.
pub async fn follow_activation(
    conn: &mut SqliteConnection,
    playlist_id: i64,
    active: bool,
    now_ms: i64,
) -> Result<u64, sqlx::Error> {
    if active {
        reactivate(conn, playlist_id).await
    } else {
        deactivate(conn, playlist_id, now_ms).await
    }
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

/// A row the retention deletes: marked at or before the cutoff (the one
/// `?`), and in no custom playlist's set list (an operator put it there).
const DUE_WHERE: &str = "removed_at <= ? \
     AND NOT EXISTS (SELECT 1 FROM playlist_items i WHERE i.video_id = videos.id)";

/// A row the retention deletes, with the files it records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueRow {
    pub id: i64,
    pub youtube_id: String,
    pub title: Option<String>,
    /// `file_path` as the row stores it (`""` when none).
    pub video: String,
    pub audio: Option<String>,
}

/// At most `limit` rows due at `cutoff_ms` (`DUE_WHERE`), the longest
/// marked first.
pub async fn due_ids(
    pool: &SqlitePool,
    cutoff_ms: i64,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT id FROM videos WHERE {DUE_WHERE} ORDER BY removed_at, id LIMIT ?"
    ))
    .bind(cutoff_ms)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Row `id` while it is still due at `cutoff_ms`, read right before its
/// deletion (a sync may have brought it back since the batch was read).
pub async fn due_row(
    pool: &SqlitePool,
    id: i64,
    cutoff_ms: i64,
) -> Result<Option<DueRow>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT id, youtube_id, title, COALESCE(file_path, '') AS video, audio_file_path \
         FROM videos WHERE id = ? AND {DUE_WHERE}"
    ))
    .bind(id)
    .bind(cutoff_ms)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| DueRow {
        id: r.get("id"),
        youtube_id: r.get("youtube_id"),
        title: r.get("title"),
        video: r.get("video"),
        audio: r.get("audio_file_path"),
    }))
}

/// Another row than `id` records `path` as one of its files (its video, its
/// audio, a stem or its dub): the same video in another playlist shares the
/// files by name (#136), so such a file is never deleted with `id`.
pub async fn recorded_elsewhere(
    pool: &SqlitePool,
    id: i64,
    path: &Path,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM videos WHERE id != ?1 \
         AND (file_path = ?2 OR audio_file_path = ?2 OR vocals_file_path = ?2 \
              OR instrumental_file_path = ?2 OR dub_file_path = ?2))",
    )
    .bind(id)
    .bind(path.to_string_lossy().into_owned())
    .fetch_one(pool)
    .await
}

/// The peer exchange's records of a video (`youtube_id` keyed) that go with
/// its last row (and #261's daily `peer::leftovers` pass).
pub(crate) const PEER_TABLES: [&str; 3] = ["peer_waits", "peer_fetches", "peer_standins"];

/// Delete `row` while it is still due at `cutoff_ms`, in one transaction
/// with its own data: its play history (the foreign key's
/// cascade), the exchange's records of its video once no row of the video
/// is left, and the sha256 cache entries of the files `removed`. `false`
/// (nothing written) when the row is not due any more.
pub async fn delete_row(
    pool: &SqlitePool,
    row: &DueRow,
    cutoff_ms: i64,
    removed: &[PathBuf],
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query(&format!("DELETE FROM videos WHERE id = ? AND {DUE_WHERE}"))
        .bind(row.id)
        .bind(cutoff_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    for table in PEER_TABLES {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE youtube_id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM videos WHERE youtube_id = ?1)"
        ))
        .bind(&row.youtube_id)
        .execute(&mut *tx)
        .await?;
    }
    for path in removed {
        sqlx::query("DELETE FROM peer_hashes WHERE path = ?")
            .bind(path.to_string_lossy().into_owned())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
#[path = "models_membership_tests.rs"]
mod tests;
