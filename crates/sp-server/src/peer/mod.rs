//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod config;
pub mod lan;
