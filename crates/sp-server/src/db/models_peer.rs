//! #229: the node exchange's tables. V29 `peer_hashes`: this node's sha256
//! cache, keyed by path; an entry holds while the file's size and mtime match
//! (`peer::hasher` checks that before the catalog lists the file).

use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;

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

#[cfg(test)]
#[path = "models_peer_tests.rs"]
mod tests;
