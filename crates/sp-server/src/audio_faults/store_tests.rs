//! #257: the `audio_faults` table through the store, on an in-memory
//! database with every migration.

use sqlx::SqlitePool;

use super::*;
use crate::audio_faults::answer::{KindTotal, StoredEvent};
use crate::audio_faults::model::{FaultRow, Severity, kinds};

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

fn row(ts_ms: i64, path: &str, kind: &'static str, severity: Severity, amount: i64) -> FaultRow {
    FaultRow {
        ts_ms,
        path: path.to_string(),
        kind,
        severity,
        amount,
        detail: format!("name of {path} at {ts_ms}"),
    }
}

#[tokio::test]
async fn rows_are_written_whole_and_read_back_as_written() {
    let pool = pool().await;
    insert(&pool, &[]).await.unwrap();
    let rows = [
        row(100, "out:a", kinds::UNDERRUN, Severity::Loss, 3),
        row(200, "out:a", kinds::UNDERRUN, Severity::Loss, 4),
        row(150, "out:a", kinds::LATE_CALLBACK, Severity::Risk, 1),
        row(120, "loop:a", kinds::CHECKED, Severity::Meter, 60_000),
        row(90, "out:a", kinds::START, Severity::Info, 1),
    ];
    insert(&pool, &rows).await.unwrap();
    let stored: Vec<(i64, String, String, String, i64, String)> = sqlx::query_as(
        "SELECT ts_ms, path, kind, severity, amount, detail FROM audio_faults ORDER BY rowid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 5);
    assert_eq!(
        stored[2],
        (
            150,
            "out:a".to_string(),
            "late_callback".to_string(),
            "risk".to_string(),
            1,
            "name of out:a at 150".to_string()
        )
    );
    let severities: Vec<&str> = stored.iter().map(|r| r.3.as_str()).collect();
    assert_eq!(severities, ["loss", "loss", "risk", "meter", "info"]);
}

#[tokio::test]
async fn totals_group_the_window_by_path_kind_and_severity() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(50, "out:a", kinds::UNDERRUN, Severity::Loss, 9),
            row(100, "out:a", kinds::UNDERRUN, Severity::Loss, 3),
            row(200, "out:a", kinds::UNDERRUN, Severity::Loss, 4),
            row(150, "loop:a", kinds::MISMATCH, Severity::Loss, 256),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        totals(&pool, 100).await.unwrap(),
        vec![
            KindTotal {
                path: "loop:a".to_string(),
                kind: "mismatch".to_string(),
                severity: "loss".to_string(),
                amount: 256,
                rows: 1,
                last_ms: 150,
            },
            KindTotal {
                path: "out:a".to_string(),
                kind: "underrun".to_string(),
                severity: "loss".to_string(),
                amount: 7,
                rows: 2,
                last_ms: 200,
            },
        ],
        "the row at 50 is before the window"
    );
}

#[tokio::test]
async fn the_newest_events_come_newest_first_with_no_meter() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(100, "out:a", kinds::START, Severity::Info, 1),
            row(300, "loop:a", kinds::CHECKED, Severity::Meter, 60_000),
            row(200, "out:a", kinds::UNDERRUN, Severity::Loss, 2),
            row(200, "out:a", kinds::RESYNC, Severity::Loss, 1),
            row(50, "out:a", kinds::RESET, Severity::Loss, 1),
        ],
    )
    .await
    .unwrap();
    let event = |ts_ms, kind: &str, severity: &str, amount| StoredEvent {
        ts_ms,
        path: "out:a".to_string(),
        kind: kind.to_string(),
        severity: severity.to_string(),
        amount,
    };
    assert_eq!(
        newest_events(&pool, 100, 10).await.unwrap(),
        vec![
            event(200, "resync", "loss", 1),
            event(200, "underrun", "loss", 2),
            event(100, "start", "info", 1),
        ],
        "a tie on time: the row written later first"
    );
    assert_eq!(newest_events(&pool, 100, 2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_paths_detail_is_its_newest_rows() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(100, "out:b", kinds::START, Severity::Info, 1),
            row(300, "out:a", kinds::START, Severity::Info, 1),
            row(200, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(10, "out:c", kinds::START, Severity::Info, 1),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        details(&pool, 50).await.unwrap(),
        vec![
            ("out:a".to_string(), "name of out:a at 200".to_string()),
            ("out:b".to_string(), "name of out:b at 100".to_string()),
        ],
        "the row written last names out:a; out:c has no row in the window"
    );
}

#[tokio::test]
async fn a_prune_deletes_only_the_rows_before_its_time() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(99, "out:a", kinds::START, Severity::Info, 1),
            row(100, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(101, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
        ],
    )
    .await
    .unwrap();
    assert_eq!(prune(&pool, 100).await.unwrap(), 1);
    let left: Vec<i64> = sqlx::query_scalar("SELECT ts_ms FROM audio_faults ORDER BY ts_ms")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, [100, 101]);
}
