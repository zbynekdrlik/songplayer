//! #257: the ledger task's own decisions, and the task itself over an
//! in-memory database and a real (idle) program bus.

use std::time::Instant;

use super::*;
use crate::audio_faults::model::kinds;

fn fault(ts_ms: i64, path: &str) -> FaultRow {
    FaultRow {
        ts_ms,
        path: path.to_string(),
        kind: kinds::UNDERRUN,
        severity: Severity::Loss,
        amount: 1,
        detail: String::new(),
    }
}

#[test]
fn the_ledger_reads_every_5_s_and_keeps_90_days_pruned_daily() {
    assert_eq!(TICK, Duration::from_secs(5));
    assert_eq!(PRUNE_EVERY_MS, 24 * 3_600_000);
    assert_eq!(RETENTION_MS, 90 * PRUNE_EVERY_MS);
}

#[test]
fn a_prune_is_due_at_first_then_once_a_day() {
    let now = 1_760_000_000_000;
    assert!(prune_due(None, now));
    assert!(prune_due(Some(now - PRUNE_EVERY_MS), now));
    assert!(!prune_due(Some(now - PRUNE_EVERY_MS + 1), now));
}

#[test]
fn rows_the_store_refused_keep_their_newest() {
    let mut rows: Vec<FaultRow> = (1..=5).map(|t| fault(t, "out:a")).collect();
    assert_eq!(keep_unwritten(&mut rows, 3), 2);
    assert_eq!(rows.iter().map(|r| r.ts_ms).collect::<Vec<_>>(), [3, 4, 5]);
    assert_eq!(keep_unwritten(&mut rows, 10), 0);
    assert_eq!(rows.len(), 3);
}

#[test]
fn the_ticks_line_counts_losses_risks_and_starts() {
    let mut rows = vec![fault(1, "a")];
    let with = |severity| FaultRow {
        severity,
        ..fault(2, "a")
    };
    rows.extend([
        with(Severity::Risk),
        with(Severity::Risk),
        with(Severity::Info),
        with(Severity::Meter),
    ]);
    assert_eq!(tally(&rows), Some((1, 2, 1)));
    assert_eq!(tally(&[]), None);
    assert_eq!(tally(&[with(Severity::Meter)]), None, "a meter alone");
    assert_eq!(tally(&[with(Severity::Info)]), Some((0, 0, 1)));
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// The task writes the program's start, prunes a row past 90 days (and only
/// that one) on its first tick, and ends on shutdown.
#[tokio::test]
async fn the_task_records_prunes_and_ends_on_shutdown() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let now = utc_now_ms();
    let rows = [
        fault(now - RETENTION_MS - 60_000, "out:old"),
        fault(now - RETENTION_MS + 60_000, "out:kept"),
    ];
    store::insert(&pool, &rows).await.unwrap();
    let bus = Arc::new(ProgramBus::new());
    let registry = Arc::new(NdiHealthRegistry::new());
    let (tx, rx) = broadcast::channel(1);
    let every = Duration::from_millis(10);
    let task = tokio::spawn(run(pool.clone(), bus, registry, rx, every));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let started = count(
            &pool,
            "SELECT COUNT(*) FROM audio_faults WHERE kind = 'start'",
        )
        .await;
        let old = count(
            &pool,
            "SELECT COUNT(*) FROM audio_faults WHERE path = 'out:old'",
        )
        .await;
        if started > 0 && old == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "started {started}, old {old}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the task ends on shutdown")
        .unwrap();
    let program = count(
        &pool,
        "SELECT COUNT(*) FROM audio_faults WHERE path = 'program' AND kind = 'start'",
    )
    .await;
    assert_eq!(program, 1, "the program started once, then ran steady");
    let kept = count(
        &pool,
        "SELECT COUNT(*) FROM audio_faults WHERE path = 'out:kept'",
    )
    .await;
    assert_eq!(kept, 1, "a row inside 90 days is kept");
}
