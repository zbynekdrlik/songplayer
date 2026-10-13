//! #257: the audio fault ledger — every audio path's faults, persisted
//! across restarts and deploys (the owner, 12.10.2026: "akékoľvek puknutie,
//! strata samplu je neakceptovateľná"; design on #257).
//!
//! Every audio stage already counts its faults in memory (the program bus,
//! the outputs, their loop checks, the playlist pipelines), from 0 at every
//! process start. One task ([`run`], every [`TICK`]) reads those counters
//! IN PROCESS (`probe.rs`, the structs the API serves, never HTTP), turns
//! their growth into rows (`ledger.rs`, pure) and writes them to the
//! `audio_faults` table (V38, `store.rs`). Nothing of it runs on a real-time
//! thread. `GET /api/v1/audio/faults` answers from the rows
//! (`answer.rs`, `api/audio_faults.rs`); the post-deploy audio gates fail
//! on any `loss` row inside their own window (`e2e/audio-faults-gate.ts`).
//! Rows older than [`RETENTION_MS`] (90 days) are pruned once a day.

pub mod answer;
pub mod ledger;
pub mod model;
pub mod probe;
pub mod store;

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::ndi_health::NdiHealthRegistry;
use crate::playback::program_bus::ProgramBus;
use ledger::Ledger;
use model::{FaultRow, Severity};
use probe::TraceTally;

/// How often the ledger reads every path.
pub const TICK: Duration = Duration::from_secs(5);
/// How long a row is kept: 90 days (ms).
pub const RETENTION_MS: i64 = 7_776_000_000;
/// How often the old rows are pruned: once a day (ms).
pub const PRUNE_EVERY_MS: i64 = 86_400_000;
/// Rows kept for the next tick at most when the store could not take them.
pub const MAX_UNWRITTEN: usize = 10_000;

/// UTC now, ms.
#[cfg_attr(test, mutants::skip)] // a clock read
pub fn utc_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Whether the old rows are due for a prune (never pruned yet, or a day
/// since the last prune).
pub fn prune_due(last_ms: Option<i64>, now_ms: i64) -> bool {
    last_ms.is_none_or(|last| now_ms - last >= PRUNE_EVERY_MS)
}

/// Keep at most the newest `cap` rows the store did not take; how many of
/// the oldest went.
pub fn keep_unwritten(rows: &mut Vec<FaultRow>, cap: usize) -> usize {
    let over = rows.len().saturating_sub(cap);
    rows.drain(..over);
    over
}

/// How many `loss` and `risk` rows, and path starts, `rows` hold: the
/// tick's INFO line, `None` when it holds none of them.
pub fn tally(rows: &[FaultRow]) -> Option<(usize, usize, usize)> {
    let count = |severity| rows.iter().filter(|r| r.severity == severity).count();
    let counts = (
        count(Severity::Loss),
        count(Severity::Risk),
        count(Severity::Info),
    );
    (counts != (0, 0, 0)).then_some(counts)
}

/// Start the ledger's task; it ends on `shutdown`.
pub fn start(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    registry: Arc<NdiHealthRegistry>,
    shutdown: &broadcast::Sender<()>,
) {
    tokio::spawn(run(pool, bus, registry, shutdown.subscribe(), TICK));
}

/// The ledger's task: a tick at once, then every `every`, until `shutdown`
/// (or its sender is gone). Rows the store refuses are kept for the next
/// tick (the newest [`MAX_UNWRITTEN`]).
pub async fn run(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    registry: Arc<NdiHealthRegistry>,
    mut shutdown: broadcast::Receiver<()>,
    every: Duration,
) {
    let mut ledger = Ledger::default();
    let mut trace = TraceTally::default();
    let mut unwritten: Vec<FaultRow> = Vec::new();
    let mut pruned_ms: Option<i64> = None;
    let mut ticks = tokio::time::interval(every);
    info!(
        every_ms = every.as_millis() as u64,
        "audio faults: the ledger runs"
    );
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            _ = ticks.tick() => {}
        }
        let now_ms = utc_now_ms();
        let probes = probe::read(&bus, &registry, &mut trace);
        let rows = ledger.tick(&probes, now_ms);
        if let Some((losses, risks, starts)) = tally(&rows) {
            info!(losses, risks, starts, "audio faults: rows recorded");
        }
        unwritten.extend(rows);
        match store::insert(&pool, &unwritten).await {
            Ok(()) => unwritten.clear(),
            Err(e) => {
                let dropped = keep_unwritten(&mut unwritten, MAX_UNWRITTEN);
                warn!(%e, kept = unwritten.len(), dropped, "audio faults: the rows could not be written — kept for the next tick");
            }
        }
        if prune_due(pruned_ms, now_ms) {
            match store::prune(&pool, now_ms - RETENTION_MS).await {
                Ok(deleted) => {
                    pruned_ms = Some(now_ms);
                    info!(deleted, "audio faults: rows older than 90 days pruned");
                }
                Err(e) => warn!(%e, "audio faults: the old rows could not be pruned"),
            }
        }
    }
    info!("audio faults: the ledger stopped");
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
