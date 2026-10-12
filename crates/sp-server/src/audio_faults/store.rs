//! #257: the `audio_faults` table (V38): the ledger's rows written, pruned
//! and read for `GET /api/v1/audio/faults`.

use sqlx::SqlitePool;

use super::answer::{CleanMark, KindTotal, StoredEvent};
use super::model::{FaultRow, Severity, kinds};

/// Write `rows`, all or none (one transaction).
pub async fn insert(pool: &SqlitePool, rows: &[FaultRow]) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO audio_faults (ts_ms, path, kind, severity, amount, detail) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(r.ts_ms)
        .bind(&r.path)
        .bind(r.kind)
        .bind(r.severity.as_str())
        .bind(r.amount)
        .bind(&r.detail)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Delete the rows older than `before_ms`; how many.
pub async fn prune(pool: &SqlitePool, before_ms: i64) -> Result<u64, sqlx::Error> {
    let done = sqlx::query("DELETE FROM audio_faults WHERE ts_ms < ?")
        .bind(before_ms)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

/// Every `(path, kind, severity)` from `since_ms` on: its amount summed, its
/// rows and its newest row's time.
pub async fn totals(pool: &SqlitePool, since_ms: i64) -> Result<Vec<KindTotal>, sqlx::Error> {
    let rows: Vec<(String, String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT path, kind, severity, SUM(amount), COUNT(*), MAX(ts_ms) FROM audio_faults \
         WHERE ts_ms >= ? GROUP BY path, kind, severity ORDER BY path, kind, severity",
    )
    .bind(since_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, kind, severity, amount, rows, last_ms)| KindTotal {
            path,
            kind,
            severity,
            amount,
            rows,
            last_ms,
        })
        .collect())
}

/// The newest `limit` rows from `since_ms` on that are no meter, newest
/// first.
pub async fn newest_events(
    pool: &SqlitePool,
    since_ms: i64,
    limit: i64,
) -> Result<Vec<StoredEvent>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, i64)> = sqlx::query_as(
        "SELECT ts_ms, path, kind, severity, amount FROM audio_faults \
         WHERE ts_ms >= ? AND severity != ? ORDER BY ts_ms DESC, rowid DESC LIMIT ?",
    )
    .bind(since_ms)
    .bind(Severity::Meter.as_str())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(ts_ms, path, kind, severity, amount)| StoredEvent {
            ts_ms,
            path,
            kind,
            severity,
            amount,
        })
        .collect())
}

/// Every path's newest loss and first start over every row kept from
/// `since_ms` on (the ledger's 90 days): `clean_since_ms` looks beyond the
/// asked window, so a planned restart never resets it. It reads the loss
/// and start rows only (V39's index by severity), never the meters.
pub async fn clean_marks(pool: &SqlitePool, since_ms: i64) -> Result<Vec<CleanMark>, sqlx::Error> {
    let rows: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT path, MAX(CASE WHEN severity = ? THEN ts_ms END), \
         MIN(CASE WHEN kind = ? THEN ts_ms END) FROM audio_faults \
         WHERE severity IN (?, ?) AND ts_ms >= ? GROUP BY path ORDER BY path",
    )
    .bind(Severity::Loss.as_str())
    .bind(kinds::START)
    .bind(Severity::Loss.as_str())
    .bind(Severity::Info.as_str())
    .bind(since_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, last_loss_ms, first_start_ms)| CleanMark {
            path,
            last_loss_ms,
            first_start_ms,
        })
        .collect())
}

/// Every path with a row from `since_ms` on, and the name its newest row
/// carries.
pub async fn details(
    pool: &SqlitePool,
    since_ms: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    // SQLite's documented bare columns: with ONE max() aggregate, `detail`
    // comes from the row that holds the max.
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT path, detail, MAX(rowid) FROM audio_faults WHERE ts_ms >= ? \
         GROUP BY path ORDER BY path",
    )
    .bind(since_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, detail, _)| (path, detail))
        .collect())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
