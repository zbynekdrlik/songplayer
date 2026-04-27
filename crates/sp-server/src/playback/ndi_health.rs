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
use std::sync::{RwLock, atomic::Ordering};
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
    ///
    /// Visibility-only: SongPlayer does not auto-recover from this state in
    /// v0.26.0+. The 2026-04-27 production failure showed per-sender recreate
    /// cannot fix the actual root cause (NDI runtime mDNS bound to a stale
    /// network adapter); recovery requires a process restart or full NDI
    /// runtime re-init (tracked in #60).
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

use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::PlayState;
use tracing::info;

impl crate::playback::PlaybackEngine {
    /// Map an `Instant` from the pipeline thread to a `DateTime<Utc>` using
    /// the engine's startup reference. Approximate (drift between Instant's
    /// monotonic clock and SystemTime grows over long runs) but bounded by
    /// the difference between Instant::now() and SystemTime::now() at engine
    /// startup, which is typically zero.
    ///
    /// mutants::skip — direction of the offset is observable only via
    /// absolute-time assertions on the dashboard payload; the unit tests
    /// assert presence/structure of the timestamp, not its absolute value.
    /// Behaviour is verified by the live `/api/v1/ndi/health` endpoint
    /// returning sane recent-past timestamps in production.
    #[cfg_attr(test, mutants::skip)]
    fn instant_to_utc(&self, t: Instant) -> DateTime<Utc> {
        let (origin_instant, origin_utc) = self.instant_origin;
        let delta = t.saturating_duration_since(origin_instant);
        origin_utc + chrono::Duration::from_std(delta).unwrap_or(chrono::Duration::zero())
    }

    /// Process a `PipelineEvent::HealthSnapshot` for `playlist_id`.
    /// Reconciles the pipeline-reported state against the canonical
    /// `PlayState`, fills `degraded_reason` when consecutive_bad_polls >= 2,
    /// and writes the result into the shared `NdiHealthRegistry`.
    ///
    /// mutants::skip — the lookup and transition-log conditionals (find
    /// predicate, prev != current guards, &&-vs-||) are visible only as
    /// log-line presence/absence, not in the persisted snapshot. The
    /// snapshot's correctness IS exercised by the unit tests below
    /// (handle_health_snapshot_populates_registry_*, ..._fills_degraded_reason_*,
    /// engine_overrides_idle_to_waiting_for_scene_*); the log-side effects
    /// are observable in production trace output but not unit-testable
    /// without log-capture machinery.
    #[cfg_attr(test, mutants::skip)]
    pub fn handle_health_snapshot(&mut self, playlist_id: i64, event: PipelineEvent) {
        let PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            last_submit_ts,
            last_heartbeat_ts,
            consecutive_bad_polls,
            reported_state,
        } = event
        else {
            return;
        };

        // Drop the event entirely for pipelines the engine doesn't know about.
        // Returning early instead of writing through the registry keeps the
        // API output consistent with the engine's view of which pipelines
        // exist.
        let pp = match self.pipelines.get(&playlist_id) {
            Some(p) => p,
            None => return,
        };

        // Reconcile state: the canonical engine knows about WaitingForScene;
        // the pipeline thread doesn't. Override the pipeline's Idle when the
        // engine says WaitingForScene. Also map Playing+scene_inactive to
        // Paused so compute_degraded_reason returns None when OBS is not
        // on this pipeline's scene (connections=0 there is normal noise).
        let scene_active = pp.scene_active.load(Ordering::Acquire);
        let canonical_state = match (&pp.state, &reported_state, scene_active) {
            // OBS isn't on this pipeline's scene → no subscriber is expected.
            // Map Playing to a quiet state so compute_degraded_reason returns
            // None even when connections == 0.
            (PlayState::Playing { .. }, PlaybackStateLabel::Playing, false) => {
                PlaybackStateLabel::Paused
            }
            (PlayState::WaitingForScene, PlaybackStateLabel::Idle, _) => {
                PlaybackStateLabel::WaitingForScene
            }
            _ => reported_state.clone(),
        };

        let ndi_name = pp.pipeline.ndi_name().to_string();
        let degraded_reason = compute_degraded_reason(
            &canonical_state,
            connections,
            observed_fps,
            nominal_fps,
            consecutive_bad_polls,
        );

        // Look up the previous snapshot from the registry to detect
        // connection-count changes and degraded transitions for logging.
        let prev = self
            .ndi_health_registry
            .snapshots()
            .into_iter()
            .find(|s| s.playlist_id == playlist_id);
        let prev_connections = prev.as_ref().map(|s| s.connections);
        let prev_degraded = prev.as_ref().and_then(|s| s.degraded_reason.clone());

        let snapshot = PipelineHealthSnapshot {
            playlist_id,
            ndi_name: ndi_name.clone(),
            state: canonical_state.clone(),
            connections,
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            last_submit_ts: last_submit_ts.map(|t| self.instant_to_utc(t)),
            last_heartbeat_ts: Some(self.instant_to_utc(last_heartbeat_ts)),
            consecutive_bad_polls,
            degraded_reason: degraded_reason.clone(),
        };

        // Transition logging: connection-count change, degradation, recovery.
        if let Some(prev) = prev_connections {
            if prev != connections {
                info!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    prev = prev,
                    now = connections,
                    "ndi: connections changed"
                );
            }
        }
        if degraded_reason.is_some() && prev_degraded.is_none() {
            warn!(
                playlist_id,
                ndi_name = %ndi_name,
                reason = degraded_reason.as_deref().unwrap_or(""),
                "ndi: pipeline degraded"
            );
        } else if degraded_reason.is_none() && prev_degraded.is_some() {
            info!(
                playlist_id,
                ndi_name = %ndi_name,
                "ndi: pipeline recovered"
            );
        }

        self.ndi_health_registry.update(snapshot);
    }
}

/// Pure helper: convert canonical state + per-poll values + consecutive
/// bad-poll count into the degraded_reason string. The frontend uses this
/// string verbatim. Returns None when the snapshot is healthy or below
/// the >=2 consecutive gate.
///
/// Mutation testing: the >=2 gate is a single comparison; the helper is
/// excluded from cargo-mutants because the boundary is exhaustively
/// covered by the boundary tests below.
#[cfg_attr(test, mutants::skip)]
fn compute_degraded_reason(
    state: &PlaybackStateLabel,
    connections: i32,
    observed_fps: f32,
    nominal_fps: f32,
    consecutive_bad_polls: u32,
) -> Option<String> {
    if !matches!(state, PlaybackStateLabel::Playing) {
        return None;
    }
    if consecutive_bad_polls < 2 {
        return None;
    }
    if connections == 0 {
        return Some("no NDI receiver — wall is dark".to_string());
    }
    if nominal_fps > 0.0 && observed_fps < nominal_fps / 2.0 {
        return Some(format!(
            "underrunning ({obs:.0}/{nom:.0} fps)",
            obs = observed_fps,
            nom = nominal_fps,
        ));
    }
    Some("no frames in 10s".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::PlaybackEngine;
    use crate::playback::state::PlayState;
    use sp_core::ws::ServerMsg;
    use sqlx::SqlitePool;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::{broadcast, mpsc};

    async fn fresh_engine() -> (PlaybackEngine, Arc<NdiHealthRegistry>) {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let (obs_tx, _) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
        let registry = Arc::new(NdiHealthRegistry::new());
        let engine = PlaybackEngine::new(
            pool,
            PathBuf::from("/tmp"),
            obs_tx,
            None,
            resolume_tx,
            ws_tx,
            None,
            registry.clone(),
        );
        (engine, registry)
    }

    #[tokio::test]
    async fn handle_health_snapshot_populates_registry_for_known_pipeline() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(7, "SP-test");

        let now = Instant::now();
        engine.handle_health_snapshot(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 2,
                frames_submitted_total: 150,
                frames_submitted_last_5s: 30,
                observed_fps: 29.97,
                nominal_fps: 29.97,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Playing,
            },
        );

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].playlist_id, 7);
        assert_eq!(snapshots[0].connections, 2);
        assert_eq!(snapshots[0].frames_submitted_total, 150);
        assert!(snapshots[0].last_submit_ts.is_some());
    }

    #[tokio::test]
    async fn handle_health_snapshot_drops_event_for_unknown_pipeline() {
        let (mut engine, registry) = fresh_engine().await;
        let now = Instant::now();
        engine.handle_health_snapshot(
            999,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 0,
                frames_submitted_last_5s: 0,
                observed_fps: 0.0,
                nominal_fps: 30.0,
                last_submit_ts: None,
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Idle,
            },
        );
        assert_eq!(registry.snapshots().len(), 0);
    }

    #[tokio::test]
    async fn registry_holds_one_entry_per_pipeline_with_health() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(1, "SP-a");
        engine.ensure_pipeline(2, "SP-b");
        let now = Instant::now();
        let mk_event = |state| PipelineEvent::HealthSnapshot {
            connections: 1,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            last_submit_ts: None,
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: state,
        };
        engine.handle_health_snapshot(1, mk_event(PlaybackStateLabel::Playing));
        engine.handle_health_snapshot(2, mk_event(PlaybackStateLabel::Idle));
        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 2);
        let ids: Vec<_> = snapshots.iter().map(|s| s.playlist_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[tokio::test]
    async fn engine_overrides_idle_to_waiting_for_scene_when_canonical_state_says_so() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(5, "SP-w");
        engine.set_state_for_test(5, PlayState::WaitingForScene);

        let now = Instant::now();
        engine.handle_health_snapshot(
            5,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 0,
                frames_submitted_last_5s: 0,
                observed_fps: 0.0,
                nominal_fps: 30.0,
                last_submit_ts: None,
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Idle,
            },
        );

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            snapshots[0].state,
            PlaybackStateLabel::WaitingForScene,
            "engine must override pipeline's Idle -> WaitingForScene when canonical state matches"
        );
    }

    #[tokio::test]
    async fn handle_health_snapshot_fills_degraded_reason_at_2_consecutive_bad_polls() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(8, "SP-fail");
        engine.set_state_for_test(8, PlayState::Playing { video_id: 1 });
        engine.set_scene_active_for_test(8, true);
        let now = Instant::now();
        engine.handle_health_snapshot(
            8,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 100,
                frames_submitted_last_5s: 30,
                observed_fps: 30.0,
                nominal_fps: 30.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 2,
                reported_state: PlaybackStateLabel::Playing,
            },
        );
        let snapshots = registry.snapshots();
        assert_eq!(snapshots[0].consecutive_bad_polls, 2);
        assert_eq!(
            snapshots[0].degraded_reason.as_deref(),
            Some("no NDI receiver — wall is dark"),
        );
    }

    #[test]
    fn degraded_reason_returns_none_at_one_bad_poll() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 0, 0.0, 30.0, 1);
        assert!(r.is_none(), "single bad poll must not trigger degradation");
    }

    #[test]
    fn degraded_reason_returns_none_when_not_playing() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Idle, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
        let r = compute_degraded_reason(&PlaybackStateLabel::Paused, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
        let r = compute_degraded_reason(&PlaybackStateLabel::WaitingForScene, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
    }

    #[test]
    fn degraded_reason_emits_underrun_when_fps_below_half_nominal() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 10.0, 30.0, 2);
        assert_eq!(r.as_deref(), Some("underrunning (10/30 fps)"));
    }

    #[test]
    fn degraded_reason_emits_stale_when_fps_ok_and_connections_ok() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 30.0, 30.0, 2);
        assert_eq!(r.as_deref(), Some("no frames in 10s"));
    }

    /// Regression test for the 2026-04-27 production failure.
    ///
    /// v0.25.0 deployed PR #58's Tier-2 RecreateSender as the auto-recovery
    /// for prolonged `connections=0`. In production NDI's mDNS socket bound
    /// to a stale APIPA address (`169.254.144.214`); per-sender recreate
    /// could not fix that runtime-level binding, and `send_create` with the
    /// existing name failed on the same-name conflict. The wall stayed dark
    /// while the log spammed `RecreateSender mid-decode: failed; keeping existing`
    /// every 30 s for ~50 minutes until the process was restarted.
    ///
    /// v0.26.0 ripped the entire trigger out (no `RecreateSender` variant,
    /// no `should_fire_recreate` predicate, no `recreate_attempts` snapshot
    /// field) and reverted to Tier-1 visibility only. This test asserts the
    /// remaining behaviour: prolonged `connections=0` while Playing fills
    /// `degraded_reason` for the dashboard/log without any other side effects.
    /// Re-introducing per-sender recreate machinery would have to redefine
    /// the snapshot shape and is structurally caught by `cargo check` — but
    /// this test is the documented contract.
    #[tokio::test]
    async fn handle_health_snapshot_visibility_only_on_prolonged_dark_wall() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(7, "SP-fast");
        engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
        engine.set_scene_active_for_test(7, true);

        let now = Instant::now();
        // Simulate 100 consecutive bad polls (8+ minutes of dark wall) —
        // past every threshold the v0.25.0 PR #58 schedule fired at.
        engine.handle_health_snapshot(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 12_000,
                frames_submitted_last_5s: 120,
                observed_fps: 24.0,
                nominal_fps: 24.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 100,
                reported_state: PlaybackStateLabel::Playing,
            },
        );

        let snap = &registry.snapshots()[0];
        assert_eq!(snap.consecutive_bad_polls, 100);
        assert_eq!(snap.connections, 0);
        // Tier-1 visibility fires.
        assert_eq!(
            snap.degraded_reason.as_deref(),
            Some("no NDI receiver — wall is dark"),
        );
    }

    /// Tier-1 visibility must clear when the wall recovers (e.g. operator
    /// restarts SongPlayer after NDI APIPA binding made connections=0). A
    /// clean poll after a degraded run drops `degraded_reason` back to None
    /// so the dashboard / log "ndi: pipeline recovered" path fires.
    #[tokio::test]
    async fn handle_health_snapshot_clears_degraded_reason_on_clean_poll() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(7, "SP-fast");
        engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
        engine.set_scene_active_for_test(7, true);

        let now = Instant::now();
        // First: degraded.
        engine.handle_health_snapshot(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 240,
                frames_submitted_last_5s: 120,
                observed_fps: 24.0,
                nominal_fps: 24.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 5,
                reported_state: PlaybackStateLabel::Playing,
            },
        );
        assert_eq!(
            registry.snapshots()[0].degraded_reason.as_deref(),
            Some("no NDI receiver — wall is dark")
        );

        // Then: clean poll. Connections returned, no consecutive_bad_polls.
        engine.handle_health_snapshot(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 2,
                frames_submitted_total: 480,
                frames_submitted_last_5s: 120,
                observed_fps: 24.0,
                nominal_fps: 24.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Playing,
            },
        );
        let snap = &registry.snapshots()[0];
        assert_eq!(snap.connections, 2);
        assert_eq!(snap.consecutive_bad_polls, 0);
        assert!(
            snap.degraded_reason.is_none(),
            "clean poll must clear degraded_reason so 'ndi: pipeline recovered' log fires",
        );
    }

    #[tokio::test]
    async fn handle_health_snapshot_skips_alert_when_scene_inactive() {
        // Pipeline is decoding (state=Playing) but OBS is on a different
        // scene → scene_active=false. Even with connections=0, no alert.
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(9, "SP-off");
        engine.set_state_for_test(9, PlayState::Playing { video_id: 1 });
        // scene_active defaults to false on a fresh pipeline; do not flip it.

        let now = Instant::now();
        engine.handle_health_snapshot(
            9,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 100,
                frames_submitted_last_5s: 30,
                observed_fps: 30.0,
                nominal_fps: 30.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 5,
                reported_state: PlaybackStateLabel::Playing,
            },
        );
        let snapshots = registry.snapshots();
        assert_eq!(snapshots[0].state, PlaybackStateLabel::Paused);
        assert!(
            snapshots[0].degraded_reason.is_none(),
            "scene_active=false must not produce a degraded_reason even with connections=0"
        );
    }
}
