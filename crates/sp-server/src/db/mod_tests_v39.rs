//! V39 (#257 lane 2): the audio fault ledger read by severity, for each
//! path's newest loss and first start over the 90 days kept.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn v39_indexes_the_audio_faults_by_severity_path_and_time() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 38).await;
    apply_upto(&pool, 39).await;
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'audio_faults' \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        indexes,
        [
            "idx_audio_faults_path_ts",
            "idx_audio_faults_severity_path_ts",
            "idx_audio_faults_ts"
        ]
    );
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('idx_audio_faults_severity_path_ts') ORDER BY seqno",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, ["severity", "path", "ts_ms"]);
}

#[tokio::test]
async fn migration_v39_is_in_the_list() {
    let latest = MIGRATIONS.last().unwrap().0;
    assert!(latest >= 39, "{latest}");
}
