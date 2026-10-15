//! #275: the process's record of whether SongPlayer's clip runs in Arena,
//! read by `GET /api/v1/wall` (`api/routes_status.rs::wall`, its
//! `songplayer_clip`).
//!
//! Each Resolume host driver publishes its own fact (`sp_clips::Fact`,
//! `driver_sp_clips.rs`); the record keeps them by host and their
//! combination ([`combine`]: running when one host runs a SongPlayer clip),
//! with the instant that combination's state last changed (`since_ms`, on
//! the server's clock). The same pattern as `wall::WallStore`: a
//! `std::sync::RwLock` copied out, no lock held after a call.

use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use sp_core::wall_clip::{ClipState, SongplayerClip};

use crate::resolume::sp_clips::Fact;

/// Every host's fact, and their combination.
#[derive(Debug, Default)]
struct Record {
    hosts: BTreeMap<String, Fact>,
    clip: SongplayerClip,
}

/// The record (one per process: [`global`]; a test makes its own).
#[derive(Debug, Default)]
pub struct SpClipStore(RwLock<Record>);

impl SpClipStore {
    /// `host`'s fact now, at `now_ms` (UTC unix ms): the combination's
    /// `since_ms` moves when its state changes, and at the first fact.
    pub fn publish(&self, host: &str, fact: Fact, now_ms: i64) {
        let mut record = self.0.write().unwrap_or_else(|e| e.into_inner());
        record.hosts.insert(host.to_string(), fact);
        let combined = combine(record.hosts.values());
        let since_ms = if record.clip.since_ms.is_none() || combined.state != record.clip.state {
            Some(now_ms)
        } else {
            record.clip.since_ms
        };
        record.clip = SongplayerClip {
            since_ms,
            ..combined
        };
    }

    /// The combination now, stamped with the server's clock (`now_ms`): the
    /// dashboard reads how long the state holds against it.
    pub fn current(&self, now_ms: i64) -> SongplayerClip {
        let record = self.0.read().unwrap_or_else(|e| e.into_inner());
        SongplayerClip {
            now_ms: Some(now_ms),
            ..record.clip.clone()
        }
    }
}

/// Every host's fact as one: running when one runs a SongPlayer clip, else
/// unknown when one is unknown (or there is no host), else none. The clips
/// of every host are listed (in host order).
pub fn combine<'a>(facts: impl IntoIterator<Item = &'a Fact>) -> SongplayerClip {
    let facts: Vec<&Fact> = facts.into_iter().collect();
    let has = |state: ClipState| facts.iter().any(|fact| fact.state == state);
    let state = if has(ClipState::Running) {
        ClipState::Running
    } else if facts.is_empty() || has(ClipState::Unknown) {
        ClipState::Unknown
    } else {
        ClipState::NotRunning
    };
    SongplayerClip {
        state,
        clips: facts.iter().flat_map(|fact| fact.clips.clone()).collect(),
        ndi_clips: facts
            .iter()
            .flat_map(|fact| fact.ndi_clips.clone())
            .collect(),
        since_ms: None,
        now_ms: None,
    }
}

/// The process's record: `unknown` until a Resolume host driver publishes.
pub fn global() -> &'static SpClipStore {
    static STORE: LazyLock<SpClipStore> = LazyLock::new(SpClipStore::default);
    &STORE
}

#[cfg(test)]
#[path = "sp_clip_store_tests.rs"]
mod tests;
