//! #229: the startup database open waits out a lock, on a paused clock.
//! Wired via `#[cfg(test)] #[path = "startup_open_tests.rs"] mod tests;`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::{STARTUP_OPEN_DELAYS, is_retryable, open_with_retry, sqlite_busy_code};

const DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

#[test]
fn the_startup_waits_are_one_minute_in_all() {
    let secs: Vec<u64> = STARTUP_OPEN_DELAYS.iter().map(Duration::as_secs).collect();
    assert_eq!(secs, [1, 2, 4, 8, 15, 15, 15]);
    assert_eq!(secs.iter().sum::<u64>(), 60);
}

#[test]
fn busy_and_locked_are_read_from_the_low_byte_of_the_code() {
    // SQLITE_BUSY, SQLITE_LOCKED, SQLITE_BUSY_SNAPSHOT (517), SQLITE_BUSY_RECOVERY (261).
    for code in ["5", "6", "517", "261", "262"] {
        assert!(sqlite_busy_code(code), "{code}");
    }
    // SQLITE_ERROR, SQLITE_CANTOPEN, SQLITE_IOERR_READ (266), not a number.
    for code in ["1", "14", "266", "", "busy"] {
        assert!(!sqlite_busy_code(code), "{code:?}");
    }
}

#[test]
fn a_pool_timeout_is_retryable_and_a_missing_row_is_not() {
    assert!(is_retryable(&sqlx::Error::PoolTimedOut));
    assert!(!is_retryable(&sqlx::Error::RowNotFound));
    assert!(!is_retryable(&sqlx::Error::PoolClosed));
}

/// The lock clears after two tries: the third succeeds after 1 + 2 s.
#[tokio::test(start_paused = true)]
async fn a_lock_that_clears_is_waited_out() {
    let tries = AtomicUsize::new(0);
    let counter = &tries;
    let start = tokio::time::Instant::now();
    let opened = open_with_retry(
        move || async move {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(sqlx::Error::PoolTimedOut)
            } else {
                Ok(42u32)
            }
        },
        &DELAYS,
    )
    .await;
    assert_eq!(opened.ok(), Some(42));
    assert_eq!(tries.load(Ordering::SeqCst), 3);
    assert_eq!(start.elapsed(), Duration::from_secs(3));
}

/// A lock that never clears: one try per wait plus the first, then the error.
#[tokio::test(start_paused = true)]
async fn a_lock_that_never_clears_gives_up_after_every_wait() {
    let tries = AtomicUsize::new(0);
    let counter = &tries;
    let start = tokio::time::Instant::now();
    let opened = open_with_retry(
        move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Err::<u32, _>(sqlx::Error::PoolTimedOut)
        },
        &DELAYS,
    )
    .await;
    assert!(matches!(opened, Err(sqlx::Error::PoolTimedOut)));
    assert_eq!(tries.load(Ordering::SeqCst), 4);
    assert_eq!(start.elapsed(), Duration::from_secs(7));
}

/// Any other error is returned at once.
#[tokio::test(start_paused = true)]
async fn another_error_is_returned_at_once() {
    let tries = AtomicUsize::new(0);
    let counter = &tries;
    let start = tokio::time::Instant::now();
    let opened = open_with_retry(
        move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Err::<u32, _>(sqlx::Error::RowNotFound)
        },
        &DELAYS,
    )
    .await;
    assert!(matches!(opened, Err(sqlx::Error::RowNotFound)));
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

/// A real database file opens on the first try.
#[tokio::test]
async fn a_free_database_opens_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let pool = super::open(&dir.path().join("songplayer.db"))
        .await
        .unwrap();
    let one: i64 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(one, 1);
}

/// A real SQLite BUSY is retryable, a real "no such table" is not: one
/// connection holds the database EXCLUSIVE and another, with no busy wait,
/// tries to write (CI run 37995844762 found the Database arm untested).
#[tokio::test]
async fn a_real_busy_database_is_retryable_and_another_database_error_is_not() {
    use sqlx::ConnectOptions;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("locked.db").display());
    let opts = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true)
        .busy_timeout(Duration::ZERO);
    // Both connections open first: a connection's own setup must not meet
    // the lock.
    let mut holder = opts.connect().await.unwrap();
    let mut other = opts.connect().await.unwrap();
    sqlx::query("CREATE TABLE t (x INTEGER)")
        .execute(&mut holder)
        .await
        .unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut holder)
        .await
        .unwrap();

    let busy = sqlx::query("INSERT INTO t (x) VALUES (1)")
        .execute(&mut other)
        .await
        .unwrap_err();
    assert!(is_retryable(&busy), "{busy}");

    // Lock released: the next error is the missing table, not a lock.
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
    let missing = sqlx::query("SELECT x FROM no_such_table")
        .execute(&mut other)
        .await
        .unwrap_err();
    assert!(!is_retryable(&missing), "{missing}");
}
