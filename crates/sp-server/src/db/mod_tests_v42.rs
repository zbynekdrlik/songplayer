//! V42 (#256): the dashboard's usage log table, read and pruned by time.

use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

#[tokio::test]
async fn v42_creates_the_ui_usage_table_with_its_time_index() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 41).await;
    apply_upto(&pool, 42).await;
    assert_eq!(
        column_names(&pool, "ui_usage").await,
        ["ts_ms", "page", "control", "kind", "on_air_source"]
    );
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'ui_usage' \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, ["idx_ui_usage_ts"]);
    sqlx::query(
        "INSERT INTO ui_usage (ts_ms, page, control, kind) \
         VALUES (1760000000000, '/', 'nav-settings', 'click')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let source: Option<i64> = sqlx::query_scalar("SELECT on_air_source FROM ui_usage")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source, None, "a row with nothing on air keeps NULL");
}

#[tokio::test]
async fn migration_v42_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 42, "{latest}");
}
