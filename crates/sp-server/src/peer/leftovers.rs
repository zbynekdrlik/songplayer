//! #261 (no infinite archive): the exchange's records of a video no row has
//! any more go, once a day on the shared daily timer (`crate::daily_pass`),
//! held by the #230 background hold as `Job::PeerLeftovers`.
//!
//! None of the exchange's tables has a foreign key: a playlist's DELETE
//! (whose cascade takes its rows) and #260's retention before its last row
//! of a video left them behind. A record whose video still has a row is
//! never touched, whatever playlist the row is in.
//!
//! - `peer_waits`, `peer_fetches`, `peer_standins` (keyed by `youtube_id`;
//!   the tables #260's retention clears with a video's last row,
//!   `db::models_membership::PEER_TABLES`): a record whose video no row has.
//! - `peer_hashes` (keyed by path): an entry whose path names no row's
//!   `youtube_id`. Every file it hashes is named after its video
//!   (`{song}_{artist}_{id}_normalized…`, `{id}_lyrics.json`); a path that
//!   happens to contain another row's id only KEEPS its entry, it never
//!   drops one a row needs. The hasher prunes the paths its catalog no
//!   longer names on a serving node (SNV); this pass also covers a node
//!   that serves nothing (PP), whose `peer::audio` stores an entry per
//!   audio it hashes on demand.

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::background_hold::Job;
use crate::daily_pass;
use crate::db::models_membership::PEER_TABLES;

/// What one pass deleted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// Waits, fetch records and stand-ins of videos no row has.
    pub records: u64,
    /// sha256 cache entries whose path names no row's video.
    pub hashes: u64,
}

/// A hash entry whose path names no row's YouTube id (never an empty id:
/// it would name every path).
const ORPHAN_HASHES: &str = "DELETE FROM peer_hashes WHERE NOT EXISTS ( \
     SELECT 1 FROM videos v WHERE v.youtube_id != '' AND instr(peer_hashes.path, v.youtube_id) > 0)";

/// One pass: every leftover record, then every orphan hash entry.
pub async fn pass(pool: &SqlitePool) -> Result<Dropped, sqlx::Error> {
    let mut records = 0;
    for table in PEER_TABLES {
        records += sqlx::query(&format!(
            "DELETE FROM {table} WHERE NOT EXISTS \
             (SELECT 1 FROM videos v WHERE v.youtube_id = {table}.youtube_id)"
        ))
        .execute(pool)
        .await?
        .rows_affected();
    }
    let hashes = sqlx::query(ORPHAN_HASHES)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(Dropped { records, hashes })
}

/// The pass's loop on the shared daily timer, until shutdown.
#[cfg_attr(test, mutants::skip)] // the timer is daily_pass's; `pass` is tested
pub async fn run(pool: SqlitePool, shutdown: broadcast::Receiver<()>) {
    daily_pass::run(pool, shutdown, Job::PeerLeftovers, |pool, _| async move {
        match pass(&pool).await {
            Ok(Dropped {
                records: 0,
                hashes: 0,
            }) => debug!("exchange leftovers: nothing to delete"),
            Ok(done) => info!(
                records = done.records,
                hashes = done.hashes,
                "exchange leftovers: deleted the records of videos no row has any more"
            ),
            Err(e) => warn!(%e, "exchange leftovers: the pass failed - trying again tomorrow"),
        }
    })
    .await;
}

#[cfg(test)]
#[path = "leftovers_tests.rs"]
mod tests;
