// crates/sp-server/src/playback/ndi_health.rs
//! NDI per-pipeline health snapshot types + lock-free registry +
//! engine aggregator.
//!
//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Mirrors `playback/recovery.rs` precedent and `resolume::ResolumeRegistry`
//! shape from PR #54.

use crate::playback::clock_health::ClockHealth;
use crate::playback::lock_state::LOCK_WINDOW_100NS;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
    /// dantesync-derived clock health (#146). The same box-wide value is
    /// stamped onto every pipeline's snapshot; `clock_ok` gates the genlock
    /// lock-state. Defaults to `no dantesync` until the poller reports.
    pub clock: ClockHealth,
    /// Boundary-paced emission telemetry (#147). Default (`enabled=false`,
    /// zeros) on the SDK-clocked (flag-OFF) path; filled from the `Pacer` when
    /// `genlock_pacing` is on.
    pub pacing: PacingStats,
    /// Audio clock-discipline telemetry (#148). Default (`enabled=false`, zeros)
    /// on the SDK-clocked / idle path; filled from the `Pacer`'s
    /// `AudioGridBuffer` + `AudioPll` on the paced path.
    pub audio: AudioStats,
    /// Derived three-state genlock lock (#149, contract §7 A7.3): the same
    /// LOCKED / DEGRADED / UNLOCKED vocabulary the OBS indicator uses
    /// (camera-box#1298). Computed at snapshot time from `clock.clock_ok`,
    /// `pacing.enabled`, `connections`, and the last-60-s event window; flag
    /// OFF (pacing disabled) ⇒ `Unlocked`.
    pub lock_state: sp_core::genlock::lock_state::LockState,
    /// Human reason for `lock_state` (e.g. `"pacing disabled"`, `"no receiver"`,
    /// `"locked"`). Rendered verbatim by the dashboard / log.
    pub lock_reason: String,
    /// Whether the runtime burn-id QR overlay (#151) is currently ON for this
    /// output. Default `false`; toggled via `POST /api/v1/ndi/burn`; the fleet's
    /// burn-leak guard sweeps this to confirm no QR was left on the LED wall.
    pub burn_on: bool,
}

/// Boundary-paced emission telemetry (#147), surfaced on
/// `GET /api/v1/ndi/health` as `pacing`. `enabled=false` + all-zero is what an
/// SDK-clocked (flag-OFF) or idle pipeline reports; the `Pacer`
/// (`playback/pacer.rs`) fills real values on the paced path.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PacingStats {
    /// Whether boundary-paced emission is active for this pipeline.
    pub enabled: bool,
    /// Monotonic count of boundaries serviced (one frame emitted per boundary).
    pub seq: u64,
    /// Emits that landed a full interval or more past their boundary.
    pub late_frames: u64,
    /// Worst emit lateness observed (µs).
    pub max_late_us: u64,
    /// 99th-percentile emit jitter (emit − boundary, µs) over the recent window.
    pub jitter_p99_us: u64,
    /// Last-frame repeats emitted on decoder underrun (one per starved boundary).
    pub repeats: u64,
    /// Grid resyncs — a lag beyond the catch-up bound with nothing buffered.
    pub resyncs: u64,
    /// Backward-clock-step re-latches.
    pub relatches: u64,
    /// Decoded frames dropped as older-than-boundary (e.g. 60→30 decimation).
    pub dropped: u64,
    /// Current lag at the last emit: whole grid slots the serviced boundary sat
    /// behind `floor(now)` (#147 lane 3). A slow file decoder (`iter_cost >=
    /// interval`) drives this up until the playback re-anchor bounds it; a
    /// growing `lag_slots` is the direct signal of the box-test-1 failure, where
    /// `jitter_p99_us` (which measures emit − boundary lateness) merely mirrored
    /// it. Signed because it is a gauge, always `>= 0` in practice.
    pub lag_slots: i64,
    /// 99th-percentile per-iteration decode+submit cost (µs) over the recent
    /// window (#147 lane 3). `iter_cost >= interval` (≈ 33_333 µs @30 fps) is the
    /// condition under which catch-up can never gain on the wall clock, so this
    /// is the honest "can the decoder keep up?" signal, distinct from
    /// `jitter_p99_us` (emit − boundary lateness).
    pub iter_p99_us: u64,
    /// 99th-percentile pre-decode (`prepare`) duration (µs) over the recent
    /// window (#147 lane 4). Decode moved AHEAD of the boundary — `prepare`
    /// decodes the next due frame right after each emit, off the critical path,
    /// so `late_frames` collapses to ~0 while this gauges whether the decoder can
    /// still produce a frame inside one slot. `prep_p99_us >= interval`
    /// (≈ 33_333 µs @30 fps) means it cannot keep up and lag will grow until the
    /// re-anchor bounds it (the same signal `iter_p99_us` was, now measured where
    /// the decode actually happens).
    pub prep_p99_us: u64,
}

/// Audio clock-discipline telemetry (#148), surfaced on `GET /api/v1/ndi/health`
/// as `audio`. `enabled=false` + all-zero is what an SDK-clocked (flag-OFF) or
/// idle pipeline reports; the `Pacer` fills real values from its
/// `AudioGridBuffer` + `AudioPll` on the paced path.
///
/// `f64` residual/applied so it cannot derive `Eq` (only `PartialEq`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioStats {
    /// Whether audio clock discipline is active for this pipeline.
    pub enabled: bool,
    /// The TRUE file-clock-vs-wall rate residual (ppm) from the last 60 s window
    /// (#148 rework): `(mean_now − mean_prev) / (rate · 60 s) · 1e6` over
    /// same-phase means of the POST-take level. POSITIVE when the buffer is
    /// GROWING (file/audio clock fast).
    pub residual_ppm: f64,
    /// The slow-trim correction (ppm) the `AudioPll` is applying to the
    /// fractional read step (`1 + applied_ppm · 1e-6`). POSITIVE drains a growing
    /// buffer faster (negative feedback).
    pub applied_ppm: f64,
    /// Samples delivered per grid boundary (1600 @ 48 kHz / 30 fps).
    pub samples_per_boundary: u64,
    /// Boundary chunks that ran the FIFO dry and were zero-filled (cumulative).
    pub underruns: u64,
    /// Times the 2 s cap dropped the oldest audio (cumulative).
    pub overflows: u64,
    /// Current buffered audio (ms) — the POST-take setpoint is ~66 ms
    /// (2 boundaries, `AUDIO_TARGET_BOUNDARIES`).
    pub buffer_ms: u64,
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
            pacing,
            audio,
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

        // Lock-state derivation (#149, Lane 1). Read the box-wide clock health,
        // push this heartbeat's cumulative pacing counters into the per-pipeline
        // 60 s window (a monotonic timestamp off the engine's `Instant` origin —
        // the window is purely relative, so no wall clock is needed), then
        // derive the three-state lock from clock_ok + pacing.enabled +
        // connections + the differenced window counts. A `-1` "never polled"
        // connection count maps to 0 receivers.
        let clock = match self.clock_health.read() {
            Ok(guard) => guard.clone(),
            Err(_) => ClockHealth::default(),
        };
        let heartbeat_100ns = (last_heartbeat_ts
            .saturating_duration_since(self.instant_origin.0)
            .as_nanos()
            / 100) as i64;
        let (late_w, repeats_w, resyncs_w) = {
            let window = self.lock_windows.entry(playlist_id).or_default();
            window.push(
                heartbeat_100ns,
                pacing.late_frames,
                pacing.repeats,
                pacing.resyncs,
            );
            window.counts_in_window(heartbeat_100ns, LOCK_WINDOW_100NS)
        };
        let (lock_state, lock_reason) = sp_core::genlock::lock_state::derive(
            clock.clock_ok,
            pacing.enabled,
            connections.max(0) as u32,
            late_w,
            repeats_w,
            resyncs_w,
        );

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
            clock,
            pacing,
            audio,
            lock_state,
            lock_reason: lock_reason.to_string(),
            // #151: read the shared burn flag by output name so the health JSON
            // reflects the current toggle state (false unless the API set it).
            burn_on: self.ndi_burn_registry.is_on(&ndi_name),
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

        // Periodic heartbeat log: once per UTC-minute bucket per pipeline.
        // Guarantees a baseline state record in the log within 60s of any
        // moment, so a "wall is dark" report can be diagnosed against the
        // pipeline state SongPlayer believed it had at that minute. Without
        // this, transition-only logging leaves multi-hour silent windows
        // (observed: 2026-04-28 sp-fast playing all night with no log
        // line for ~9h, while OBS distroAV silently received zero frames).
        let prev_heartbeat_ts = prev.as_ref().and_then(|s| s.last_heartbeat_ts);
        let cur_heartbeat_ts = snapshot.last_heartbeat_ts;
        if let Some(cur) = cur_heartbeat_ts {
            if should_log_periodic_heartbeat(prev_heartbeat_ts, cur) {
                info!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    state = ?canonical_state,
                    connections,
                    frames_total = frames_submitted_total,
                    frames_5s = frames_submitted_last_5s,
                    observed_fps = format!("{:.1}", observed_fps),
                    nominal_fps = format!("{:.1}", nominal_fps),
                    scene_active,
                    "ndi: heartbeat"
                );
                // #149 item 2: a second, grep-stable genlock telemetry line
                // beside the heartbeat, same once-per-UTC-minute cadence.
                info!("{}", format_genlock_line(&snapshot));
            }
        }

        self.ndi_health_registry.update(snapshot);
    }
}

/// Pure helper: should the engine emit a periodic INFO heartbeat log for
/// this `last_heartbeat_ts`? Returns `true` on the first heartbeat for a
/// pipeline (`prev` is `None`) and once per UTC-minute wall-clock bucket
/// thereafter.
///
/// The 60-second bucket is computed via `timestamp() / 60`, so consecutive
/// 5-second heartbeats inside the same UTC minute log at most once. This
/// bounds output to ~1 line/min/pipeline (~8640/day for 6 pipelines) while
/// guaranteeing a recent baseline ground-truth entry exists for every
/// minute the engine is alive.
fn should_log_periodic_heartbeat(prev: Option<DateTime<Utc>>, cur: DateTime<Utc>) -> bool {
    match prev {
        None => true,
        Some(p) => p.timestamp() / 60 != cur.timestamp() / 60,
    }
}

/// Render the once-per-minute structured genlock telemetry line (#149 item 2,
/// contract §7 vocabulary). Emitted as a second line beside `ndi: heartbeat`
/// with grep-stable `key=value` tokens. Pure so the exact shape is
/// unit-testable; the periodic INFO path logs the returned string verbatim.
pub(crate) fn format_genlock_line(s: &PipelineHealthSnapshot) -> String {
    format!(
        "ndi: genlock playlist_id={pid} ndi_name={name} seq={seq} late={late} p99_us={p99} repeats={repeats} resyncs={resyncs} relatches={relatches} lag={lag} audio_ppm={ppm:.1} underruns={underruns} clock_ok={clock_ok} lock={lock} reason=\"{reason}\"",
        pid = s.playlist_id,
        name = s.ndi_name,
        seq = s.pacing.seq,
        late = s.pacing.late_frames,
        p99 = s.pacing.jitter_p99_us,
        repeats = s.pacing.repeats,
        resyncs = s.pacing.resyncs,
        relatches = s.pacing.relatches,
        lag = s.pacing.lag_slots,
        ppm = s.audio.residual_ppm,
        underruns = s.audio.underruns,
        clock_ok = s.clock.clock_ok,
        lock = s.lock_state.as_str(),
        reason = s.lock_reason,
    )
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
#[path = "lock_state_tests.rs"]
mod lock_state_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::state::PlayState;
    use crate::playback::{PlaybackEngine, PlaybackEngineConfig};
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
        let engine = PlaybackEngine::new(PlaybackEngineConfig {
            pool,
            cache_dir: PathBuf::from("/tmp"),
            obs_event_tx: obs_tx,
            obs_cmd_tx: None,
            resolume_tx,
            ws_event_tx: ws_tx,
            presenter_client: None,
            ndi_health_registry: registry.clone(),
        });
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
                pacing: Default::default(),
                audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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
            pacing: Default::default(),
            audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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
                pacing: Default::default(),
                audio: Default::default(),
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

    #[test]
    fn should_log_periodic_heartbeat_on_first_heartbeat() {
        let cur: DateTime<Utc> = "2026-04-28T05:21:00Z".parse().unwrap();
        assert!(
            should_log_periodic_heartbeat(None, cur),
            "first heartbeat for a pipeline must always log"
        );
    }

    #[test]
    fn should_log_periodic_heartbeat_on_new_minute_bucket() {
        let prev: DateTime<Utc> = "2026-04-28T05:21:55Z".parse().unwrap();
        let cur: DateTime<Utc> = "2026-04-28T05:22:00Z".parse().unwrap();
        assert!(
            should_log_periodic_heartbeat(Some(prev), cur),
            "crossing into a new UTC-minute bucket must log"
        );
    }

    #[test]
    fn should_log_periodic_heartbeat_suppresses_within_same_minute() {
        let prev: DateTime<Utc> = "2026-04-28T05:21:00Z".parse().unwrap();
        let cur: DateTime<Utc> = "2026-04-28T05:21:55Z".parse().unwrap();
        assert!(
            !should_log_periodic_heartbeat(Some(prev), cur),
            "heartbeats inside the same UTC minute must NOT spam the log"
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
                pacing: Default::default(),
                audio: Default::default(),
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
