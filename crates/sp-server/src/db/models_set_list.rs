//! #261: a custom playlist's set list (`playlist_items`) stays numbered
//! `0..N-1` whatever takes an item out of it: the set-list removal
//! (`models::remove_playlist_item`) and a playlist's DELETE, whose cascade
//! takes that playlist's songs out of every set list. The selector asks the
//! position after the current one and wraps to 0 when it finds none
//! (`playlist::selector`), so a hole made it skip every item after it.
//!
//! The current position follows the item it pointed at
//! ([`shifted_current`]), so an item taken out before it never makes the
//! next pick skip one.

use sqlx::{SqliteConnection, SqlitePool};

/// Where a set list's `current` position points once the items at the old
/// positions `kept` (ascending) are numbered `0..N-1`: the same item when it
/// is kept; else the item before the first kept one that followed it, so
/// the next pick (current + 1) is the item that came after the one taken
/// out (with none kept before it, the last item: the next pick wraps to the
/// first). 0 for an empty set list. Kept as it is, accepted: a restart
/// right after the CURRENT item was taken out resumes at the current
/// position (`current_video_id` is gone then), so it plays that item before
/// once more; the live pick, the common case, is exact.
pub fn shifted_current(current: i64, kept: &[i64]) -> i64 {
    let before = kept.iter().filter(|&&p| p < current).count() as i64;
    if kept.contains(&current) || kept.is_empty() {
        return before;
    }
    (before - 1).rem_euclid(kept.len() as i64)
}

/// Number `playlist_id`'s set list `0..N-1` in its order and move its
/// current position with it ([`shifted_current`]), on `conn` (the caller's
/// transaction). Two steps for the primary key `(playlist_id, position)`:
/// every position is first moved below 0, then each item takes its new one.
pub(crate) async fn compact(
    conn: &mut SqliteConnection,
    playlist_id: i64,
) -> Result<(), sqlx::Error> {
    let items: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT position, video_id FROM playlist_items WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(&mut *conn)
    .await?;
    let current: i64 = sqlx::query_scalar("SELECT current_position FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query("UPDATE playlist_items SET position = -position - 1 WHERE playlist_id = ?")
        .bind(playlist_id)
        .execute(&mut *conn)
        .await?;
    for (new_pos, (_, video_id)) in items.iter().enumerate() {
        sqlx::query(
            "UPDATE playlist_items SET position = ? WHERE playlist_id = ? AND video_id = ?",
        )
        .bind(new_pos as i64)
        .bind(playlist_id)
        .bind(*video_id)
        .execute(&mut *conn)
        .await?;
    }
    let kept: Vec<i64> = items.iter().map(|(pos, _)| *pos).collect();
    sqlx::query("UPDATE playlists SET current_position = ? WHERE id = ?")
        .bind(shifted_current(current, &kept))
        .bind(playlist_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Delete playlist `playlist_id` (its songs, their plays and set-list
/// items go by the foreign keys' cascades) and number every OTHER custom
/// playlist that held one of its songs again ([`compact`]), in one
/// transaction. `false` (nothing written) when there is no such playlist.
/// `BEGIN IMMEDIATE`: the transaction reads before it writes, and in WAL a
/// deferred one whose snapshot another connection's commit made stale fails
/// its first write at once (no busy wait); an immediate one waits the busy
/// timeout for the write lock up front.
pub async fn delete_playlist(pool: &SqlitePool, playlist_id: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let holders: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT i.playlist_id FROM playlist_items i JOIN videos v ON v.id = i.video_id \
         WHERE v.playlist_id = ?1 AND i.playlist_id != ?1 ORDER BY i.playlist_id",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;
    let deleted = sqlx::query("DELETE FROM playlists WHERE id = ?")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    for holder in holders {
        compact(&mut tx, holder).await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
#[path = "models_set_list_tests.rs"]
mod tests;
