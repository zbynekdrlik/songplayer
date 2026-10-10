//! #229: the server's first database open at startup waits out a briefly
//! locked database instead of failing.
//!
//! A restart can start the new process while the old one still holds
//! `songplayer.db` (its shutdown checkpoint). The first connection's
//! `PRAGMA journal_mode = WAL` then waits, the pool's 2 s acquire timeout
//! (`pool_tuning`, kept for normal requests) runs out, and before this module
//! `start()` failed and left the Tauri shell up with no server (PP,
//! 9.10.2026 18:57Z). A retryable error — the pool timing out, or SQLite
//! answering BUSY / LOCKED — is tried again after each of
//! [`STARTUP_OPEN_DELAYS`]; any other error, or the last wait spent, is
//! returned.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::warn;

/// The waits between startup open tries: 60 s in all.
pub const STARTUP_OPEN_DELAYS: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(15),
    Duration::from_secs(15),
];

/// Whether an SQLite result code (extended codes included) is SQLITE_BUSY
/// (5) or SQLITE_LOCKED (6): the primary code is the low byte.
pub fn sqlite_busy_code(code: &str) -> bool {
    code.parse::<i32>().is_ok_and(|c| matches!(c & 0xff, 5 | 6))
}

/// Whether a failed open may succeed once another process lets go of the
/// database.
pub fn is_retryable(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Database(db) => db.code().is_some_and(|c| sqlite_busy_code(&c)),
        _ => false,
    }
}

/// Run `opener` until it succeeds; a retryable error waits the next of
/// `delays` and tries again, any other error (or no wait left) is returned.
pub async fn open_with_retry<T, F, Fut>(
    mut opener: F,
    delays: &[Duration],
) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut waits = delays.iter();
    loop {
        match opener().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let Some(&wait) = waits.next().filter(|_| is_retryable(&e)) else {
                    return Err(e);
                };
                warn!(
                    %e,
                    wait_s = wait.as_secs(),
                    "database: opening it at startup failed on a lock (the previous process may still hold it) — trying again"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Startup's database open: [`super::create_pool`] with
/// [`open_with_retry`] over [`STARTUP_OPEN_DELAYS`].
pub async fn open(db_path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let url = format!("sqlite:{}", db_path.display());
    open_with_retry(|| super::create_pool(&url), &STARTUP_OPEN_DELAYS).await
}

#[cfg(test)]
#[path = "startup_open_tests.rs"]
mod tests;
