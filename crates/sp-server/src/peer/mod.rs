//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod config;
pub mod kind;
pub mod lan;
pub mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

/// This node in the exchange: its database and its cache dir (later lanes add
/// its job board and its peer client). One per process, built by `lib.rs`.
pub struct Exchange {
    /// The node's database: the exchange settings are read from it live.
    pub pool: SqlitePool,
    /// The node's cache dir, where its processed files live (read from lane 3
    /// on, when the node serves and fetches them).
    pub cache_dir: PathBuf,
}

impl Exchange {
    pub fn new(pool: SqlitePool, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { pool, cache_dir })
    }
}

/// Every route of the exchange; `lib.rs` merges it into the app's router.
pub fn router(ex: Arc<Exchange>) -> axum::Router {
    lan::router(ex)
}
