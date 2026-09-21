//! Exact-value tests for the file-pool tuning (#184 round A). `pool_tuning()` is
//! the single source of the WAL/synchronous/busy/acquire values `create_pool`
//! applies; the acquire timeout in particular MUST be bounded (2 s), never
//! sqlx's 30 s default that caused the dub-mix stall.

use super::*;

#[test]
fn pool_tuning_has_the_exact_hardened_values() {
    let t = pool_tuning();
    assert_eq!(t.journal_mode, SqliteJournalMode::Wal);
    assert_eq!(t.synchronous, SqliteSynchronous::Normal);
    assert_eq!(t.busy_timeout, Duration::from_secs(5));
    // The bounded acquire timeout is the whole fix — never the 30 s default.
    assert_eq!(t.acquire_timeout, Duration::from_secs(2));
}

#[tokio::test]
async fn create_memory_pool_still_works_without_wal() {
    // WAL is not applicable to `:memory:`, so the in-memory pool must keep
    // building (tests rely on it) — `pool_tuning` is file-pool only.
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    let v = current_schema_version(&pool).await.unwrap();
    assert!(v >= 26, "migrations run on the in-memory pool");
}
