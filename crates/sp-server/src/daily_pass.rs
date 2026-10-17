//! The timer of a background pass that runs once a day, shared by the #260
//! playlist retention (`playlist::retention`) and the #256 usage-log prune
//! (`crate::ui_usage`): the first pass [`FIRST_PASS_AFTER`] the start (the
//! startup self-heal and the first syncs are done by then), then one a
//! day; a pass the #230 background hold held is tried again an hour later.
//! The pass itself is the caller's, and it logs its own outcome.

use std::future::Future;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::background_hold::{self, Job};

/// The first pass, after the start.
const FIRST_PASS_AFTER: Duration = Duration::from_secs(300);

/// A pass a day.
const PASS_EVERY: Duration = Duration::from_secs(86_400);

/// A pass the background hold found held is tried again this much later.
const HELD_RETRY: Duration = Duration::from_secs(3_600);

/// The wait before the next pass: a day, or an hour after a pass the
/// background hold kept from running.
pub fn next_wait(held: bool) -> Duration {
    if held { HELD_RETRY } else { PASS_EVERY }
}

/// Run `pass(pool, now_ms)` on the daily timer until shutdown, asking the
/// background hold for `job` before each one.
#[cfg_attr(test, mutants::skip)] // the timer loop; `next_wait` and each caller's pass are tested
pub async fn run<P, F>(
    pool: SqlitePool,
    mut shutdown: broadcast::Receiver<()>,
    job: Job,
    mut pass: P,
) where
    P: FnMut(SqlitePool, i64) -> F,
    F: Future<Output = ()>,
{
    let mut wait = FIRST_PASS_AFTER;
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        let held = background_hold::holds(&pool, job).await;
        if !held {
            pass(pool.clone(), chrono::Utc::now().timestamp_millis()).await;
        }
        wait = next_wait(held);
    }
}

#[cfg(test)]
#[path = "daily_pass_tests.rs"]
mod tests;
