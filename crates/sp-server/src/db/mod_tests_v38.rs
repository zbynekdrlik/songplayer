//! V38 (#257): the audio fault ledger's table, read by time and by path.

use super::test_helpers::{apply_first_n, apply_upto, column_names};
use super::*;

#[tokio::test]
async fn v38_creates_the_audio_fault_table_with_its_indexes() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 37).await;
    apply_upto(&pool, 38).await;
    assert_eq!(
        column_names(&pool, "audio_faults").await,
        ["ts_ms", "path", "kind", "severity", "amount", "detail"]
    );
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'audio_faults' \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, ["idx_audio_faults_path_ts", "idx_audio_faults_ts"]);
    sqlx::query(
        "INSERT INTO audio_faults (ts_ms, path, kind, severity, amount) \
         VALUES (1760000000000, 'program', 'start', 'info', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let detail: String = sqlx::query_scalar("SELECT detail FROM audio_faults")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(detail, "", "a row with no name reads an empty detail");
}

#[tokio::test]
async fn migration_v38_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 38, "{latest}");
}
