//! #256: the dashboard's usage log keeps [`RETENTION_DAYS`] days (a yearly
//! action still shows), then its rows go. The loop has the #260 retention
//! worker's shape (`playlist::retention::run`): a pass 5 min after the
//! start, then one a day, an hour later when the #230 background hold held
//! it. A pass deletes in bounded statements ([`PRUNE_BATCH`] rows each),
//! so it never holds the database's write lock for long.
//!
//! The rest of the log: the table `db::models_ui_usage` (V42), the HTTP
//! side `api::ui_usage`, the shared rules `sp_core::ui_usage`, the browser
//! side sp-ui `usage.rs`.

use std::time::Duration;

use sp_core::ui_usage::{RETENTION_DAYS, window_start};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::background_hold::{self, Job};
use crate::db::models_ui_usage;

/// At most this many rows per DELETE statement.
pub const PRUNE_BATCH: i64 = 5_000;

/// At most this many statements per pass (5 million rows of
/// [`PRUNE_BATCH`]: far more than a node records in a day); what is left
/// goes the next day. A structural bound: no statement count can spin.
const MAX_STATEMENTS: u32 = 1_000;

/// The first pass, after the start.
const FIRST_PASS_AFTER: Duration = Duration::from_secs(300);

/// A pass a day.
const PASS_EVERY: Duration = Duration::from_secs(86_400);

/// A pass the background hold found held is tried again this much later.
const HELD_RETRY: Duration = Duration::from_secs(3_600);

/// The oldest instant the log keeps at `now_ms`: a row before it goes (the
/// same window the summary reads at `days=400`).
pub fn cutoff_ms(now_ms: i64) -> i64 {
    window_start(now_ms, RETENTION_DAYS)
}

/// The wait before the next pass: a day, or an hour after a pass the
/// background hold kept from running.
pub fn next_wait(held: bool) -> Duration {
    if held { HELD_RETRY } else { PASS_EVERY }
}

/// One pass at `now_ms`: every row older than [`cutoff_ms`] deleted,
/// `batch` rows per statement, until a statement finds none (at most
/// [`MAX_STATEMENTS`]). The number deleted.
pub async fn prune_with(pool: &SqlitePool, now_ms: i64, batch: i64) -> Result<u64, sqlx::Error> {
    let cutoff = cutoff_ms(now_ms);
    let mut deleted = 0;
    for _ in 0..MAX_STATEMENTS {
        let done = models_ui_usage::prune_older(pool, cutoff, batch).await?;
        if done == 0 {
            break;
        }
        deleted += done;
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}

/// One pass at `now_ms` in statements of [`PRUNE_BATCH`] rows.
pub async fn prune(pool: &SqlitePool, now_ms: i64) -> Result<u64, sqlx::Error> {
    prune_with(pool, now_ms, PRUNE_BATCH).await
}

/// The prune loop: a pass [`FIRST_PASS_AFTER`] the start, then
/// [`next_wait`] after each, until shutdown. A pass asks the #230
/// background hold first.
#[cfg_attr(test, mutants::skip)] // the timer loop; `prune` and `next_wait` are tested
pub async fn run(pool: SqlitePool, mut shutdown: broadcast::Receiver<()>) {
    let mut wait = FIRST_PASS_AFTER;
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        let held = background_hold::holds(&pool, Job::UiUsagePrune).await;
        if !held {
            let now_ms = chrono::Utc::now().timestamp_millis();
            match prune(&pool, now_ms).await {
                Ok(0) => debug!("ui usage: nothing older than {RETENTION_DAYS} days"),
                Ok(deleted) => info!(
                    deleted,
                    "ui usage: deleted the rows older than {RETENTION_DAYS} days"
                ),
                Err(e) => warn!(%e, "ui usage: the prune failed - trying again tomorrow"),
            }
        }
        wait = next_wait(held);
    }
}

#[cfg(test)]
#[path = "ui_usage_tests.rs"]
mod tests;
