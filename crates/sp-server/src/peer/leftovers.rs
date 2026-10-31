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
//!   `youtube_id` ([`names_a_video`]). Every file it hashes is named after
//!   its video (`{song}_{artist}_{id}_normalized…`, `{id}_lyrics.json`); a
//!   path that happens to contain another row's id only KEEPS its entry, it
//!   never drops one a row needs. The hasher prunes the paths its catalog no
//!   longer names on a serving node (SNV); this pass also covers a node
//!   that serves nothing (PP), whose `peer::audio` stores an entry per
//!   audio it hashes on demand.
//!
//! No statement scans one table per row of another (review round 1: the
//! write lock a DELETE holds grew with hashes × rows, measured 10 s at
//! 4 000 rows / 20 000 hashes, past the pool's 5 s busy timeout): the
//! records' `NOT EXISTS` looks a video up by V43's `idx_videos_youtube`,
//! and the hashes are decided here from the ids and the paths read once,
//! each orphan deleted by its own short statement. An entry is only a
//! cache: a row added between the reads and its delete costs a hash again,
//! never a wrong answer.

use std::collections::{BTreeSet, HashSet};

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::background_hold::Job;
use crate::daily_pass;
use crate::db::models_membership::PEER_TABLES;
use crate::db::models_peer;

/// What one pass deleted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// Waits, fetch records and stand-ins of videos no row has.
    pub records: u64,
    /// sha256 cache entries whose path names no row's video.
    pub hashes: u64,
}

/// Whether `path` names one of the YouTube ids `ids` (`lengths` = their
/// distinct lengths): any window of an id's length that IS one. An empty id
/// would name every path, so the caller leaves it out.
pub fn names_a_video(path: &str, ids: &HashSet<&[u8]>, lengths: &BTreeSet<usize>) -> bool {
    lengths
        .iter()
        .any(|&len| path.as_bytes().windows(len).any(|w| ids.contains(w)))
}

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
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT youtube_id FROM videos WHERE youtube_id != ''")
            .fetch_all(pool)
            .await?;
    let known: HashSet<&[u8]> = ids.iter().map(|id| id.as_bytes()).collect();
    let lengths: BTreeSet<usize> = ids.iter().map(String::len).collect();
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM peer_hashes")
        .fetch_all(pool)
        .await?;
    let mut hashes = 0;
    for path in paths.iter().filter(|p| !names_a_video(p, &known, &lengths)) {
        hashes += models_peer::remove_hash(pool, path).await?;
    }
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
