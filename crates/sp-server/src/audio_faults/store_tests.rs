//! #257: the `audio_faults` table through the store, on an in-memory
//! database with every migration.

use sqlx::SqlitePool;

use super::*;
use crate::audio_faults::answer::{CleanMark, KindTotal, StoredEvent};
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
/// #257 review: the prune keeps each path's EARLIEST start, so a path's
/// first start bounds its clean time for good (a pruned deploy never
/// resets it); every other row before its time goes.
async fn a_prune_deletes_the_rows_before_its_time_but_each_paths_first_start() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(50, "out:a", kinds::START, Severity::Info, 1),
            row(70, "loop:a", kinds::CHECKED, Severity::Meter, 60_000),
            row(80, "out:a", kinds::START, Severity::Info, 1),
            row(90, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(60, "out:b", kinds::START, Severity::Info, 1),
            row(100, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(120, "out:a", kinds::START, Severity::Info, 1),
        ],
    )
    .await
    .unwrap();
    assert_eq!(prune(&pool, 100).await.unwrap(), 3);
    let left: Vec<(i64, String)> =
        sqlx::query_as("SELECT ts_ms, path FROM audio_faults ORDER BY ts_ms")
            .fetch_all(&pool)
            .await
            .unwrap();
    let left: Vec<(i64, &str)> = left.iter().map(|(t, p)| (*t, p.as_str())).collect();
    assert_eq!(
        left,
        [(50, "out:a"), (60, "out:b"), (100, "out:a"), (120, "out:a")]
    );
    assert_eq!(
        prune(&pool, 100).await.unwrap(),
        0,
        "a second prune keeps them"
    );
}

/// #257 lane 2: every path's newest loss from `since` on (the 90 days
/// kept) and its first start ever (#257 review: the prune keeps it) —
/// beyond any window; risks and meters never count.
#[tokio::test]
async fn clean_marks_take_each_paths_newest_loss_and_first_start() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(40, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(60, "out:a", kinds::START, Severity::Info, 1),
            row(70, "out:a", kinds::RESYNC, Severity::Loss, 1),
            row(80, "out:a", kinds::START, Severity::Info, 1),
            row(90, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(95, "out:a", kinds::LATE_CALLBACK, Severity::Risk, 1),
            row(65, "out:b", kinds::START, Severity::Info, 1),
            row(85, "out:b", kinds::START, Severity::Info, 1),
            row(99, "out:b", kinds::LATE_CALLBACK, Severity::Risk, 1),
            row(75, "loop:a", kinds::CHECKED, Severity::Meter, 60_000),
            row(76, "loop:a", kinds::BLIND, Severity::Meter, 1_000),
        ],
    )
    .await
    .unwrap();
    let mark = |path: &str, loss: Option<i64>, start: Option<i64>| CleanMark {
        path: path.to_string(),
        last_loss_ms: loss,
        first_start_ms: start,
    };
    assert_eq!(
        clean_marks(&pool, 50).await.unwrap(),
        vec![
            mark("out:a", Some(90), Some(60)),
            mark("out:b", None, Some(65)),
        ],
        "a risk is no loss, a meter no mark; the loss before 50 is not kept"
    );
    assert_eq!(
        clean_marks(&pool, 0).await.unwrap()[0],
        mark("out:a", Some(90), Some(60))
    );
    assert_eq!(
        clean_marks(&pool, 61).await.unwrap()[0],
        mark("out:a", Some(90), Some(60)),
        "#257 review: a loss counts from `since` on, the first start ever"
    );
    assert_eq!(
        clean_marks(&pool, 95).await.unwrap(),
        vec![mark("out:a", None, Some(60)), mark("out:b", None, Some(65))],
        "no loss since 95: the first starts still"
    );
}

#[test]
fn the_bucket_case_numbers_a_row_by_its_inner_edges() {
    assert_eq!(
        bucket_case(2),
        "CASE WHEN ts_ms < ? THEN 0 WHEN ts_ms < ? THEN 1 ELSE 2 END"
    );
    assert_eq!(
        bucket_case(59).matches("WHEN ts_ms < ? THEN").count(),
        59,
        "one edge per inner boundary of the timeline's 60 buckets"
    );
    assert!(bucket_case(59).ends_with(" ELSE 59 END"));
}

/// #257 review: the timeline counts EVERY row of the window (here 1001
/// losses, far over the answer's 500 events), each in the bucket sp-core's
/// `bucket_of` names — the query numbers them by sp-core's own edges.
#[tokio::test]
async fn bucket_counts_number_every_row_by_the_timelines_edges() {
    use sp_core::audio_faults_timeline::{BUCKETS, bucket_of};
    let pool = pool().await;
    let mut rows: Vec<FaultRow> = (1_000..=2_000)
        .map(|t| row(t, "out:a", kinds::UNDERRUN, Severity::Loss, 1))
        .collect();
    rows.push(row(999, "out:a", kinds::UNDERRUN, Severity::Loss, 1));
    rows.push(row(2_001, "out:a", kinds::UNDERRUN, Severity::Loss, 1));
    rows.push(row(1_500, "out:a", kinds::START, Severity::Info, 1));
    rows.push(row(
        1_500,
        "loop:a",
        kinds::CHECKED,
        Severity::Meter,
        60_000,
    ));
    rows.push(row(1_016, "out:a", kinds::LATE_CALLBACK, Severity::Risk, 1));
    rows.push(row(1_017, "out:a", kinds::LATE_CALLBACK, Severity::Risk, 1));
    rows.push(row(2_000, "out:b", kinds::LATE_CALLBACK, Severity::Risk, 1));
    insert(&pool, &rows).await.unwrap();
    let counts = bucket_counts(&pool, 1_000, 2_000).await.unwrap();
    let mut expected = vec![0i64; BUCKETS];
    for t in 1_000..=2_000 {
        expected[bucket_of(t, 1_000, 2_000, BUCKETS).unwrap()] += 1;
    }
    let losses: Vec<(i64, i64)> = counts
        .iter()
        .filter(|c| c.path == "out:a" && c.severity == "loss")
        .map(|c| (c.bucket, c.rows))
        .collect();
    assert_eq!(
        losses,
        expected
            .iter()
            .enumerate()
            .map(|(i, n)| (i as i64, *n))
            .collect::<Vec<_>>(),
        "every bucket, its rows by sp-core's edges; nothing outside the window"
    );
    assert_eq!((losses[0].1, losses[1].1, losses[59].1), (17, 17, 17));
    let risks: Vec<(String, i64, i64)> = counts
        .iter()
        .filter(|c| c.severity == "risk")
        .map(|c| (c.path.clone(), c.bucket, c.rows))
        .collect();
    assert_eq!(
        risks,
        [
            ("out:a".to_string(), 0, 1),
            ("out:a".to_string(), 1, 1),
            ("out:b".to_string(), 59, 1)
        ],
        "1016 and 1017 split at the edge; a start and a meter are no count"
    );
    assert_eq!(counts.len(), BUCKETS + 3);
}

#[tokio::test]
async fn window_starts_list_each_paths_starts_in_the_window() {
    let pool = pool().await;
    insert(
        &pool,
        &[
            row(900, "out:a", kinds::START, Severity::Info, 1),
            row(1_200, "out:b", kinds::START, Severity::Info, 1),
            row(1_100, "out:a", kinds::START, Severity::Info, 1),
            row(1_300, "out:a", kinds::UNDERRUN, Severity::Loss, 1),
            row(2_000, "out:a", kinds::START, Severity::Info, 1),
            row(2_100, "out:a", kinds::START, Severity::Info, 1),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        window_starts(&pool, 1_000, 2_000).await.unwrap(),
        [
            ("out:a".to_string(), 1_100),
            ("out:a".to_string(), 2_000),
            ("out:b".to_string(), 1_200)
        ]
    );
}
