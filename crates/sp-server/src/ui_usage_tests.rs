//! #256: the usage log keeps 400 days; a pass deletes everything older,
//! in bounded statements, oldest first.

use sp_core::ui_usage::{UsageEvent, UsageKind};
use sqlx::SqlitePool;

use super::{cutoff_ms, prune, prune_with};
use crate::db::models_ui_usage;

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// One click on `nav-live` at each of `stamps`.
async fn store(pool: &SqlitePool, stamps: &[i64]) {
    let events: Vec<UsageEvent> = stamps
        .iter()
        .map(|&ts| UsageEvent::new(ts, "/", "nav-live", UsageKind::Click))
        .collect();
    models_ui_usage::insert_batch(pool, &events, None)
        .await
        .unwrap();
}

/// Every row's stamp, oldest first.
async fn kept(pool: &SqlitePool) -> Vec<i64> {
    sqlx::query_scalar("SELECT ts_ms FROM ui_usage ORDER BY ts_ms")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[test]
fn the_log_keeps_400_days() {
    assert_eq!(cutoff_ms(NOW), NOW - 400 * DAY);
}

#[tokio::test]
async fn a_statement_deletes_the_oldest_rows_up_to_its_limit() {
    let pool = pool().await;
    let cutoff = cutoff_ms(NOW);
    store(
        &pool,
        &[cutoff - 3 * DAY, cutoff - 9 * DAY, cutoff - DAY, NOW],
    )
    .await;
    let done = models_ui_usage::prune_older(&pool, cutoff, 2)
        .await
        .unwrap();
    assert_eq!(done, 2);
    assert_eq!(kept(&pool).await, [cutoff - DAY, NOW]);
}

#[tokio::test]
async fn a_pass_deletes_every_row_older_than_400_days_statement_by_statement() {
    let pool = pool().await;
    let cutoff = cutoff_ms(NOW);
    let old = [
        cutoff - 5 * DAY,
        cutoff - 4 * DAY,
        cutoff - 3 * DAY,
        cutoff - 2 * DAY,
        cutoff - 1,
    ];
    let young = [cutoff, cutoff + 1, NOW - DAY, NOW];
    store(&pool, &old).await;
    store(&pool, &young).await;
    // Two rows a statement: 2 + 2 + 1, then a statement that finds none.
    assert_eq!(prune_with(&pool, NOW, 2).await.unwrap(), 5);
    assert_eq!(kept(&pool).await, young, "a row at the cutoff itself stays");
    assert_eq!(prune_with(&pool, NOW, 2).await.unwrap(), 0, "nothing left");
}

#[tokio::test]
async fn the_daily_pass_deletes_what_is_older() {
    let pool = pool().await;
    let cutoff = cutoff_ms(NOW);
    store(
        &pool,
        &[cutoff - 30 * DAY, cutoff - 2 * DAY, cutoff - 1, NOW],
    )
    .await;
    assert_eq!(prune(&pool, NOW).await.unwrap(), 3);
    assert_eq!(kept(&pool).await, [NOW]);
}
