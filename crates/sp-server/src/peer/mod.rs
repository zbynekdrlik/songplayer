//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod board;
pub mod config;
pub mod kind;
pub mod lan;
pub mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

/// This node in the exchange: its database, its cache dir and its job board
/// (a later lane adds its peer client). One per process, built by `lib.rs`.
pub struct Exchange {
    /// The node's database: the exchange settings are read from it live.
    pub pool: SqlitePool,
    /// The node's cache dir, where its processed files live (read from lane 3
    /// on, when the node serves and fetches them).
    pub cache_dir: PathBuf,
    /// The jobs this node runs now (its catalog lists them, lane 3).
    pub board: Arc<board::JobBoard>,
}

impl Exchange {
    pub fn new(pool: SqlitePool, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            pool,
            cache_dir,
            board: Arc::new(board::JobBoard::default()),
        })
    }

    /// Announce `job` for `youtube_id` in this node's catalog while the guard lives.
    pub fn announce(&self, youtube_id: &str, job: kind::Job) -> board::JobGuard {
        self.board.announce(youtube_id, job)
    }
}

/// Every route of the exchange; `lib.rs` merges it into the app's router.
pub fn router(ex: Arc<Exchange>) -> axum::Router {
    lan::router(ex)
}
