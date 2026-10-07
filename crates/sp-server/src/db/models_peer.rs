//! #229: the node exchange's tables.
//!
//! - V29 `peer_hashes`: this node's sha256 cache, keyed by path; an entry
//!   holds while the file's size and mtime match (`peer::hasher` checks that
//!   before the catalog lists the file).
//! - V30 `peer_waits`: since when a job of a video waits for a peer (the
//!   first wait counts, `peer::ask`); `peer_fetches`: which node an artifact
//!   came from.
//!
//! Plus the job defers of the ask-first hooks (`defer_download`,
//! `defer_stems`; the lyrics job defers through `record_lyrics_wait`) and
//! the lyrics row a node takes from a peer (`adopt_lyrics`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sqlx::SqlitePool;

use crate::peer::wire::PeerLyrics;

/// One cached sha256: the file at `path` as it was when hashed.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct HashEntry {
    pub path: String,
    pub size: i64,
    pub mtime_ms: i64,
    /// 64 lowercase hex digits.
    pub sha256: String,
    /// When the hasher stored it: the catalog's `updated_at` and `?since=`.
    pub hashed_at_ms: i64,
}

/// Every cached hash, by path.
pub async fn all_hashes(pool: &SqlitePool) -> Result<HashMap<String, HashEntry>, sqlx::Error> {
    let rows: Vec<HashEntry> =
        sqlx::query_as("SELECT path, size, mtime_ms, sha256, hashed_at_ms FROM peer_hashes")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|h| (h.path.clone(), h)).collect())
}

/// Store `e`, replacing the entry of its path.
pub async fn put_hash(pool: &SqlitePool, e: &HashEntry) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(path) DO UPDATE SET size = excluded.size, \
             mtime_ms = excluded.mtime_ms, sha256 = excluded.sha256, \
             hashed_at_ms = excluded.hashed_at_ms",
    )
    .bind(&e.path)
    .bind(e.size)
    .bind(e.mtime_ms)
    .bind(&e.sha256)
    .bind(e.hashed_at_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop the entry of `path` (its file is gone). Returns the rows removed.
pub async fn remove_hash(pool: &SqlitePool, path: &str) -> Result<u64, sqlx::Error> {
    let done = sqlx::query("DELETE FROM peer_hashes WHERE path = ?")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

/// Drop every entry whose path is not in `keep` (a renamed or removed song).
/// Returns the entries dropped.
pub async fn prune_hashes(pool: &SqlitePool, keep: &HashSet<String>) -> Result<u64, sqlx::Error> {
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM peer_hashes")
        .fetch_all(pool)
        .await?;
    let mut pruned = 0;
    for path in paths.iter().filter(|p| !keep.contains(*p)) {
        pruned += remove_hash(pool, path).await?;
    }
    Ok(pruned)
}

/// How long `job` of `youtube_id` has waited for a peer at `now_ms`; `None` =
/// not waiting. A clock stepped back past the start reads as no wait yet.
pub async fn waited(
    pool: &SqlitePool,
    youtube_id: &str,
    job: &str,
    now_ms: i64,
) -> Result<Option<Duration>, sqlx::Error> {
    let since: Option<i64> =
        sqlx::query_scalar("SELECT since_ms FROM peer_waits WHERE youtube_id = ? AND job = ?")
            .bind(youtube_id)
            .bind(job)
            .fetch_optional(pool)
            .await?;
    Ok(since.map(|s| Duration::from_millis(u64::try_from(now_ms - s).unwrap_or(0))))
}

/// The job waits from `now_ms`, unless it already waits (the first start
/// counts).
pub async fn start_wait(
    pool: &SqlitePool,
    youtube_id: &str,
    job: &str,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO peer_waits (youtube_id, job, since_ms) VALUES (?, ?, ?)")
        .bind(youtube_id)
        .bind(job)
        .bind(now_ms)
        .execute(pool)
        .await?;
    Ok(())
}

/// The job no longer waits (it runs here, or a peer's copy was taken).
pub async fn end_wait(pool: &SqlitePool, youtube_id: &str, job: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM peer_waits WHERE youtube_id = ? AND job = ?")
        .bind(youtube_id)
        .bind(job)
        .execute(pool)
        .await?;
    Ok(())
}

/// `kind` of `youtube_id` came from peer `node` (the latest fetch wins).
pub async fn record_fetch(
    pool: &SqlitePool,
    youtube_id: &str,
    kind: &str,
    node: &str,
    version: u32,
    sha256: &str,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO peer_fetches \
             (youtube_id, kind, node, version, sha256, fetched_at_ms) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(youtube_id)
    .bind(kind)
    .bind(node)
    .bind(i64::from(version))
    .bind(sha256)
    .bind(at_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// `(node, version, sha256)` of the last fetch of `kind` of `youtube_id`.
pub async fn fetch_record(
    pool: &SqlitePool,
    youtube_id: &str,
    kind: &str,
) -> Result<Option<(String, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT node, version, sha256 FROM peer_fetches WHERE youtube_id = ? AND kind = ?",
    )
    .bind(youtube_id)
    .bind(kind)
    .fetch_optional(pool)
    .await
}

/// The download of row `video_id` is picked again after `wait`, with no
/// attempt counted (`fetch_next_unprocessed` compares the same RFC 3339 form
/// `record_download_failure` writes).
pub async fn defer_download(
    pool: &SqlitePool,
    video_id: i64,
    wait: Duration,
) -> Result<(), sqlx::Error> {
    let at = chrono::Utc::now()
        + chrono::Duration::from_std(wait).unwrap_or_else(|_| chrono::Duration::zero());
    sqlx::query("UPDATE videos SET next_attempt_at = ? WHERE id = ?")
        .bind(at.to_rfc3339())
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The stems of row `video_id` are picked again after `wait`, with the status
/// and the attempts untouched (the stem selector compares
/// `stem_next_attempt_at` in this form).
pub async fn defer_stems(
    pool: &SqlitePool,
    video_id: i64,
    wait: Duration,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET stem_next_attempt_at = \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) WHERE id = ?",
    )
    .bind(i64::try_from(wait.as_secs()).unwrap_or(i64::MAX))
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Row `video_id` takes a peer's lyrics row (its `{yt}_lyrics.json` is in
/// place already). The lyrics columns go through the lyrics row's one writer
/// (`models::mark_video_lyrics_complete`: the peer's source, version and
/// alignment model), then the ★ and the translation: the peer's translation
/// version unless this row asks another translation gender (then 0, so the
/// local retranslate pass redoes the SK lines). A row with no gender of its
/// own (auto) takes the peer's, the gender its SK lines were written in.
/// SQLite's SET reads the OLD row, so the CASE sees this row's own gender.
pub async fn adopt_lyrics(
    pool: &SqlitePool,
    video_id: i64,
    l: &PeerLyrics,
) -> Result<(), sqlx::Error> {
    crate::db::models::mark_video_lyrics_complete(
        pool,
        video_id,
        &l.source,
        l.pipeline_version,
        None,
        l.alignment_model.as_deref(),
    )
    .await?;
    sqlx::query(
        "UPDATE videos SET lyrics_reference = ?1, \
             lyrics_translation_version = CASE WHEN lyrics_translation_gender IS NULL \
                 OR lyrics_translation_gender IS ?3 THEN ?2 ELSE 0 END, \
             lyrics_translation_gender = COALESCE(lyrics_translation_gender, ?3) \
         WHERE id = ?4",
    )
    .bind(i64::from(l.reference))
    .bind(i64::from(l.translation_version))
    .bind(&l.translation_gender)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "models_peer_tests.rs"]
mod tests;
