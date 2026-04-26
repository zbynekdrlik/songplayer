// crates/sp-server/src/playback/ndi_health.rs
//! NDI per-pipeline health snapshot types + lock-free registry +
//! engine aggregator.
//!
//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Mirrors `playback/recovery.rs` precedent and `resolume::ResolumeRegistry`
//! shape from PR #54.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;
use tracing::warn;

/// Per-pipeline NDI health. Serialized to the dashboard via
/// `GET /api/v1/ndi/health`. Built by the engine from
/// `PipelineEvent::HealthSnapshot` events emitted by the pipeline thread.
#[derive(Clone, Debug, Serialize)]
pub struct PipelineHealthSnapshot {
    pub playlist_id: i64,
    pub ndi_name: String,
    pub state: PlaybackStateLabel,
    /// Connection count from `NDIlib_send_get_no_connections`. `-1` means
    /// the heartbeat has never run yet (e.g. pipeline just spawned).
    pub connections: i32,
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    pub last_submit_ts: Option<DateTime<Utc>>,
    pub last_heartbeat_ts: Option<DateTime<Utc>>,
    pub consecutive_bad_polls: u32,
    /// Populated server-side when `consecutive_bad_polls >= 2`. The dashboard
    /// renders this verbatim; it does NOT compute its own staleness.
    pub degraded_reason: Option<String>,
}

/// Wire-level playback state used by the NDI health snapshot. Distinct from
/// `sp_core::playback::PlaybackState` because the heartbeat needs to
/// distinguish Idle (no playlist active) from Paused (playlist active but
/// paused) from WaitingForScene (engine knows but pipeline doesn't).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum PlaybackStateLabel {
    Idle,
    WaitingForScene,
    Playing,
    Paused,
}

/// Snapshot of the per-pipeline frame counter window. Returned by
/// `FrameSubmitter::drain_window`; the heartbeat consumer divides
/// `frames_in_window` by `window_secs` to get observed fps.
#[derive(Clone, Debug)]
pub struct WindowStats {
    pub frames_in_window: u32,
    pub window_secs: f32,
    /// `Instant::now()` captured when `drain_window` ran.
    pub drained_at: Instant,
}

/// Lock-free-read registry holding the latest health snapshot per pipeline.
/// Mirrors `crate::resolume::ResolumeRegistry` from PR #54: one Arc held by
/// the playback engine (writer) and another by `AppState` (reader). The
/// `RwLock` is held only for short copy-out reads in `snapshots()`; the
/// returned Vec is owned data, no lifetimes leak out.
pub struct NdiHealthRegistry {
    snapshots: RwLock<HashMap<i64, PipelineHealthSnapshot>>,
}

impl NdiHealthRegistry {
    /// Construct an empty registry. Callers wrap in `Arc::new(...)` when
    /// sharing across the playback engine and `AppState` — matches the
    /// `ResolumeRegistry` precedent in `crate::resolume::mod`.
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
        }
    }

    /// Replace (or insert) the snapshot for `playlist_id`.
    /// Called from the playback-engine HealthSnapshot handler.
    pub fn update(&self, snapshot: PipelineHealthSnapshot) {
        match self.snapshots.write() {
            Ok(mut map) => {
                map.insert(snapshot.playlist_id, snapshot);
            }
            Err(_) => {
                warn!(
                    playlist_id = snapshot.playlist_id,
                    "NdiHealthRegistry: RwLock poisoned on write — snapshot dropped"
                );
            }
        }
    }

    /// Snapshot every pipeline's most recent NDI health for the
    /// `/api/v1/ndi/health` endpoint. Returns one entry per pipeline that
    /// has reported at least one heartbeat.
    pub fn snapshots(&self) -> Vec<PipelineHealthSnapshot> {
        match self.snapshots.read() {
            Ok(map) => map.values().cloned().collect(),
            Err(_) => {
                warn!("NdiHealthRegistry: RwLock poisoned on read — returning empty list");
                Vec::new()
            }
        }
    }
}

impl Default for NdiHealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}
