//! Shared test helpers for db migration tests. Sibling file referenced by
//! `mod.rs` under `#[cfg(test)]` so production builds skip it.
//!
//! Each per-version test file (`mod_tests_vN.rs`) used to ship its own
//! `apply_through_vN-1` copy-paste. The bodies differ only in the slice
//! bound (`MIGRATIONS[..N-1]`). This helper takes the slice bound as a
//! parameter so every future migration test just calls
//! `apply_first_n(&pool, N-1).await`.

use super::MIGRATIONS;
use sqlx::{Row, SqlitePool};

/// Apply migrations V1..=`n` manually (first `n` entries of `MIGRATIONS`),
/// initializing the `schema_version` table first. Leaves any later
/// migrations (typically the one under test) unapplied so the caller
/// can fire `run_migrations` to trigger just that one.
///
/// Panics on any SQL error — these are test-only helpers and a failure
/// here means the test scaffold itself is broken.
pub(crate) async fn apply_first_n(pool: &SqlitePool, n: usize) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await
    .unwrap();

    for &(version, sql) in &MIGRATIONS[..n] {
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
}

/// Fetch the column names of `table` via `PRAGMA table_info(<table>)`.
/// Used by migration tests asserting a new column exists.
pub(crate) async fn column_names(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("name"))
        .collect()
}
