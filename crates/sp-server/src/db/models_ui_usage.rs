//! #256: the dashboard's usage log (V42 `ui_usage`). Written by
//! `POST /api/v1/ui-usage` (`api::ui_usage`), read by its summary, pruned
//! after 400 days (`crate::ui_usage`). Every query reads by `ts_ms`, the
//! table's one index.

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID};
use sp_core::ui_usage::{UsageEvent, UsageRow};
use sqlx::SqlitePool;

/// Store `events` in one transaction, each with `on_air_source` (the
/// program's source when the batch arrived; `None` before the first
/// selection). All or nothing.
pub async fn insert_batch(
    pool: &SqlitePool,
    events: &[UsageEvent],
    on_air_source: Option<i64>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for event in events {
        sqlx::query(
            "INSERT INTO ui_usage (ts_ms, page, control, kind, on_air_source) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(event.ts_ms)
        .bind(&event.page)
        .bind(&event.control)
        .bind(event.kind.as_str())
        .bind(on_air_source)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// One summary row as SQLite answers it.
type RowTuple = (String, String, String, i64, i64, i64, i64, i64);

/// The window `from_ms..=to_ms`, one GROUP BY: per (page, control, kind)
/// the count, the distinct days with a use by the node's local calendar
/// (SQLite's `localtime`, the box's own time zone), the first and the last
/// use, and the uses while a playlist was on air (a source that is neither
/// none, the NDI input nor Blank: `program_on_air::wall_owner`'s rule).
/// Most used first.
pub async fn summary_rows(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<UsageRow>, sqlx::Error> {
    let rows: Vec<RowTuple> = sqlx::query_as(
        "SELECT page, control, kind, COUNT(*), \
                COUNT(DISTINCT date(ts_ms / 1000, 'unixepoch', 'localtime')), \
                MIN(ts_ms), MAX(ts_ms), \
                SUM(CASE WHEN on_air_source IS NOT NULL AND on_air_source NOT IN (?, ?) \
                    THEN 1 ELSE 0 END) \
         FROM ui_usage WHERE ts_ms >= ? AND ts_ms <= ? \
         GROUP BY page, control, kind \
         ORDER BY COUNT(*) DESC, page, control, kind",
    )
    .bind(PROGRAM_INPUT_ID)
    .bind(PROGRAM_BLANK_ID)
    .bind(from_ms)
    .bind(to_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(page, control, kind, count, days_used, first_ms, last_ms, on_air)| UsageRow {
                page,
                control,
                kind,
                count,
                days_used,
                first_ms,
                last_ms,
                on_air,
            },
        )
        .collect())
}

/// Delete at most `limit` rows older than `cutoff_ms` (oldest first); the
/// number deleted.
pub async fn prune_older(
    pool: &SqlitePool,
    cutoff_ms: i64,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "DELETE FROM ui_usage WHERE rowid IN \
         (SELECT rowid FROM ui_usage WHERE ts_ms < ? ORDER BY ts_ms LIMIT ?)",
    )
    .bind(cutoff_ms)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}
