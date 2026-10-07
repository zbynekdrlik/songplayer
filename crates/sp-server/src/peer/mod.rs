//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod api;
pub mod ask;
pub mod board;
pub mod catalog;
pub mod client;
pub mod config;
pub mod decide;
pub mod download;
pub mod fetch;
pub mod hash;
pub mod hasher;
pub mod kind;
pub mod lan;
pub mod queued;
pub mod throttle;
pub mod wire;

pub use ask::{Ask, FetchPlan, PeerStep};

use std::path::PathBuf;
use std::sync::Arc;

use sp_core::config::SETTING_PEER_TRANSFERS_PAUSED;
use sqlx::SqlitePool;
use tracing::warn;

/// This node in the exchange: its database, its cache dir, its job board and
/// its peer client. One per process, built by `lib.rs`.
pub struct Exchange {
    /// The node's database: the exchange settings are read from it live.
    pub pool: SqlitePool,
    /// The node's cache dir, where its processed files live: the catalog
    /// names `{yt}_lyrics.json` in it.
    pub cache_dir: PathBuf,
    /// The jobs this node announces as running (empty until the worker hooks
    /// of lanes 8-9 announce theirs); the catalog lists them.
    pub board: Arc<board::JobBoard>,
    /// Reads the peers' catalogs and fetches their artifacts.
    pub(crate) client: client::PeerClient,
}

impl Exchange {
    pub fn new(pool: SqlitePool, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            pool,
            cache_dir,
            board: Arc::new(board::JobBoard::default()),
            client: client::PeerClient::new(),
        })
    }

    /// Announce `job` for `youtube_id` on this node's job board while the
    /// guard lives.
    pub fn announce(&self, youtube_id: &str, job: kind::Job) -> board::JobGuard {
        self.board.announce(youtube_id, job)
    }

    /// `peer_transfers_paused` = "true": no new transfer either way, no
    /// hashing. A failed read is WARNed and reads as not paused.
    pub(crate) async fn transfers_paused(&self) -> bool {
        let raw = crate::db::models::get_setting(&self.pool, SETTING_PEER_TRANSFERS_PAUSED)
            .await
            .inspect_err(|e| warn!("exchange: reading {SETTING_PEER_TRANSFERS_PAUSED} failed: {e}"))
            .ok()
            .flatten();
        sp_core::config::peer_transfers_paused(raw.as_deref())
    }
}

/// Every route of the exchange: the LAN status (no key) and the peer API
/// (`X-SP-Peer-Key`); `lib.rs` merges it into the app's router.
pub fn router(ex: Arc<Exchange>) -> axum::Router {
    lan::router(ex.clone()).merge(api::router(ex))
}

#[cfg(test)]
pub(crate) mod rig;
