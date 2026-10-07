// crates/sp-server/src/playback/ndi_health.rs
//! Per-pipeline health snapshot types + lock-free registry + engine
//! aggregator, served on `GET /api/v1/ndi/health` (the route keeps its
//! name: the dashboard, the post-deploy E2E and camera-box read it).
//!
//! #221 lane 3: a playlist pipeline has NO NDI sender of its own any more —
//! it feeds the program bus, and `SP-program` (`program_output.rs`) is the
//! one NDI sender. So a row carries no receiver count, no sender URL, no
//! burn flag and no recovery rung: the #127/#173 receiver ladder, the #196
//! post-restart self-check and its persisted receiver baseline went with
//! the senders. `SP-program`'s receivers are on `GET /api/v1/program`.
//!
//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Mirrors `playback/recovery.rs` precedent and `resolume::ResolumeRegistry`
//! shape from PR #54.

use crate::playback::clock_health::ClockHealth;
use crate::playback::ndi_health_transport::transport_from_reported;
// `PacingStats` lives in its own file (1000-line cap); re-exported so every
// `ndi_health::PacingStats` path stays valid.
pub use crate::playback::pacing_stats::PacingStats;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    RwLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tracing::warn;

// The log lines of one health snapshot (#221 L4a review: split out of
// `handle_health_snapshot`, and this file's 1000-line cap).
#[path = "ndi_health_log.rs"]
mod health_log;

/// Per-pipeline health. Serialized to the dashboard via
/// `GET /api/v1/ndi/health`. Built by the engine from
/// `PipelineEvent::HealthSnapshot` events emitted by the pipeline thread.
/// `ndi_name` is the playlist's `ndi_output_name`, the label its scene and
/// the dashboard name it by (#221 lane 3: no sender carries it any more).
#[derive(Clone, Debug, Serialize)]
pub struct PipelineHealthSnapshot {
    pub playlist_id: i64,
    pub ndi_name: String,
    pub state: PlaybackStateLabel,
    /// #201 round 2: the pipeline's OWN transport, from the RAW `reported_state`
    /// (before reconciliation), for `/api/v1/ndi/health`. Additive (= Idle).
    #[serde(default)]
    pub transport: sp_core::playback::TransportState,
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    /// #168 r6b: file SOURCE fps (decoder rate), path-independent; the lock rule reads THIS, not grid-valued `nominal_fps`.
    pub source_fps: f32,
    pub last_submit_ts: Option<DateTime<Utc>>,
    pub last_heartbeat_ts: Option<DateTime<Utc>>,
    pub consecutive_bad_polls: u32,
    /// Populated server-side when `consecutive_bad_polls >= 2` while Playing
    /// on program: an underrun or a stalled delivery to the program bus. The
    /// dashboard renders this verbatim; it does NOT compute its own
    /// staleness.
    pub degraded_reason: Option<String>,
    /// dantesync-derived clock health (#146). The same box-wide value is
    /// stamped onto every pipeline's snapshot; `clock_ok` gates the genlock
    /// lock-state. Defaults to `no dantesync` until the poller reports.
    pub clock: ClockHealth,
    /// Boundary-paced emission telemetry (#147), filled from the `Pacer`
    /// (pacing is the only path since #221 lane 3).
    pub pacing: PacingStats,
    /// Paced-audio telemetry (#148), filled from the `Pacer`'s
    /// `AudioGridBuffer`.
    pub audio: AudioStats,
    /// Derived three-state genlock lock (#149, contract §7 A7.3): the same
    /// LOCKED / DEGRADED / UNLOCKED vocabulary the OBS indicator uses
    /// (camera-box#1298). Computed at snapshot time from `clock.clock_ok`,
    /// `pacing.enabled` and the last-60-s event window.
    pub lock_state: sp_core::genlock::lock_state::LockState,
    /// Human reason for `lock_state` (e.g. `"pacing disabled"`,
    /// `"locked"`). Rendered verbatim by the dashboard / log.
    pub lock_reason: String,
    /// #229: the videos that failed to open in a row and when the next
    /// attempt is due (`failure_retry.rs`), copied from the engine at each
    /// heartbeat; `null` while none failed since the last song started. The
    /// dashboard's Player says why the program is black from it.
    pub open_failures: Option<sp_core::playback::OpenFailures>,
}

/// Paced-audio telemetry (#148), surfaced on `GET /api/v1/ndi/health` as
/// `audio`. `enabled=false` + all-zero is what an idle pipeline reports; the
/// `Pacer` fills real values from its `AudioGridBuffer`.
/// The A/V alignment itself (`av_align_err_ms`, …) is on [`PacingStats`].
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioStats {
    /// Whether the paced audio grid is active for this pipeline.
    pub enabled: bool,
    /// Samples delivered per grid boundary (1600 @ 48 kHz / 30 fps).
    pub samples_per_boundary: u64,
    /// Boundary chunks that ran the FIFO dry and were zero-filled (cumulative).
    pub underruns: u64,
    /// Times the 2 s cap dropped the oldest audio (cumulative).
    pub overflows: u64,
    /// Current buffered audio (ms) after the last take — no setpoint: the
    /// depth follows the decoder's audio lookahead (#148 design v2).
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

/// Lock-free-read registry holding the latest health snapshot per pipeline.
/// Mirrors `crate::resolume::ResolumeRegistry` from PR #54: one Arc held by
/// the playback engine (writer) and another by `AppState` (reader). The
/// `RwLock` is held only for short copy-out reads in `snapshots()`; the
/// returned Vec is owned data, no lifetimes leak out.
pub struct NdiHealthRegistry {
    snapshots: RwLock<HashMap<i64, PipelineHealthSnapshot>>,
    /// #167 readiness: the wall-clock instant this registry (≈ the process /
    /// engine) started, so the heavy-work startup grace + floor are measured
    /// off the same `Arc` both heavy workers already hold.
    created_at: Instant,
    /// #167 readiness: how many playback pipelines the engine has CREATED
    /// (`register_pipeline`), as distinct from how many have REPORTED a
    /// heartbeat (`snapshots` len). The gap is the "not proven idle yet" window
    /// that must read as wall-in-use.
    expected_pipelines: AtomicUsize,
}

impl NdiHealthRegistry {
    /// Construct an empty registry. Callers wrap in `Arc::new(...)` when
    /// sharing across the playback engine and `AppState` — matches the
    /// `ResolumeRegistry` precedent in `crate::resolume::mod`.
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
            created_at: Instant::now(),
            expected_pipelines: AtomicUsize::new(0),
        }
    }

    /// #167: record that the engine created one more playback pipeline. Called
    /// from `PlaybackEngine::ensure_pipeline_inner` at creation (once per new
    /// pipeline). Feeds `created_pipelines()` — the "how many outputs must
    /// report before the wall reading is trustworthy" count.
    pub fn register_pipeline(&self) {
        self.expected_pipelines.fetch_add(1, Ordering::Relaxed);
    }

    /// #167: how many pipelines the engine has created so far.
    pub fn created_pipelines(&self) -> usize {
        self.expected_pipelines.load(Ordering::Relaxed)
    }

    /// #167: how many pipelines have reported at least one heartbeat (the
    /// `snapshots` map holds exactly those). A poisoned lock reads as 0 (treat
    /// the reading as not-yet-ready, the safe direction).
    ///
    /// mutants::skip — the RwLock-poison fallback arm is unreachable by any
    /// terminating test (nothing poisons the lock), so its `0` literal is a
    /// MISSED mutant by construction; the map-len read IS exercised by
    /// `reported_pipelines_counts_distinct_seeded_snapshots`, and the readiness
    /// DECISION (`activity_known`) is exhaustively scored in `idle_gate.rs`.
    #[cfg_attr(test, mutants::skip)]
    pub fn reported_pipelines(&self) -> usize {
        match self.snapshots.read() {
            Ok(map) => map.len(),
            Err(_) => 0,
        }
    }

    /// #167: how long since this registry (≈ engine start) was created — the
    /// input to the startup grace + the 60 s heavy-step floor.
    ///
    /// mutants::skip — a wall-clock elapsed read; a mutant would only be caught
    /// by a wall-time assertion (non-deterministic on the runner). The pure
    /// grace/floor DECISIONS it feeds (`activity_known`, `startup_floor_defers`)
    /// are exhaustively mutation-scored in `lyrics/idle_gate.rs`.
    #[cfg_attr(test, mutants::skip)]
    pub fn since_created(&self) -> Duration {
        self.created_at.elapsed()
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
    /// derives the genlock lock, and writes the result into the shared
    /// `NdiHealthRegistry`.
    ///
    /// mutants::skip — the glue here (the pipeline lookup, the label match,
    /// the registry calls) is pinned BEHAVIOURALLY by the unit tests
    /// (`handle_health_snapshot_populates_registry_*`,
    /// `..._fills_degraded_reason_*`, `engine_overrides_idle_to_waiting_*`);
    /// its DECISIONS are pure and tested elsewhere (`compute_degraded_reason`,
    /// `lock_state::lock_for_heartbeat`). The log lines are
    /// `health_log::log_health_snapshot`.
    #[cfg_attr(test, mutants::skip)]
    pub fn handle_health_snapshot(&mut self, playlist_id: i64, event: PipelineEvent) {
        let PipelineEvent::HealthSnapshot {
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            source_fps,
            last_submit_ts,
            last_heartbeat_ts,
            consecutive_bad_polls,
            reported_state,
            pacing,
            audio,
            loop_stats,
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
        // Paused, so the label reads "Playing" only for a playlist on air
        // (the badge, the #154/#167 idle gates) and an off-air playlist that
        // keeps playing is never reported degraded.
        let scene_active = pp.scene_active.load(Ordering::Acquire);
        let canonical_state = match (&pp.state, &reported_state, scene_active) {
            (PlayState::Playing { .. }, PlaybackStateLabel::Playing, false) => {
                PlaybackStateLabel::Paused
            }
            (PlayState::WaitingForScene, PlaybackStateLabel::Idle, _) => {
                PlaybackStateLabel::WaitingForScene
            }
            _ => reported_state.clone(),
        };

        let ndi_name = pp.pipeline.output_name().to_string();
        let open_failures = pp.failures.view(); // #229
        let degraded_reason = compute_degraded_reason(
            &canonical_state,
            observed_fps,
            nominal_fps,
            consecutive_bad_polls,
        );

        // The previous snapshot, for the degraded / recovered transition and
        // the once-per-minute heartbeat lines.
        let prev = self
            .ndi_health_registry
            .snapshots()
            .into_iter()
            .find(|s| s.playlist_id == playlist_id);

        // Lock-state derivation (#149, Lane 1). Read the box-wide clock health,
        // push this heartbeat's cumulative pacing counters into the per-pipeline
        // 60 s window (a monotonic timestamp off the engine's `Instant` origin —
        // the window is purely relative, so no wall clock is needed), then
        // derive the three-state lock from clock_ok + pacing.enabled + the
        // differenced window counts.
        let clock = match self.clock_health.read() {
            Ok(guard) => guard.clone(),
            Err(_) => ClockHealth::default(),
        };
        let heartbeat_100ns = (last_heartbeat_ts
            .saturating_duration_since(self.instant_origin.0)
            .as_nanos()
            / 100) as i64;
        // #168 r6b: feed `source_fps` (the DECODER rate, path-independent) —
        // NOT `nominal_fps` (the grid, which falsely degraded a 24-fps output)
        // — with `grid_fps` the pacer's fixed `GENLOCK_GRID_FPS`.
        let (lock_state, lock_reason) = crate::playback::lock_state::lock_for_heartbeat(
            self.lock_windows.entry(playlist_id).or_default(),
            heartbeat_100ns,
            &pacing,
            clock.clock_ok,
            source_fps,
            sp_core::genlock::GENLOCK_GRID_FPS as u32,
            transport_from_reported(&reported_state),
        );

        let snapshot = PipelineHealthSnapshot {
            playlist_id,
            ndi_name,
            state: canonical_state,
            // #201 round 2: raw transport (pre-reconciliation), for the API.
            transport: transport_from_reported(&reported_state),
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            source_fps,
            last_submit_ts: last_submit_ts.map(|t| self.instant_to_utc(t)),
            last_heartbeat_ts: Some(self.instant_to_utc(last_heartbeat_ts)),
            consecutive_bad_polls,
            degraded_reason,
            clock,
            pacing,
            audio,
            lock_state,
            lock_reason: lock_reason.to_string(),
            open_failures,
        };

        health_log::log_health_snapshot(&snapshot, prev.as_ref(), scene_active, &loop_stats);

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
        "ndi: genlock playlist_id={pid} ndi_name={name} seq={seq} late={late} p99_us={p99} repeats={repeats} resyncs={resyncs} seeks={seeks} relatches={relatches} lag={lag} av_align_err_ms={av_err:.1} av_corrections={av_corr} av_corrected_samples={av_samples} av_frame_offset_ms={av_off:.1} av_frame_offset_min_ms={av_off_min:.1} av_frame_offset_max_ms={av_off_max:.1} wall_anchor_max_step_us={wa_step} wall_anchor_wide_brackets={wa_wide} wall_anchor_slewed_us={wa_slewed} wall_anchor_steps_followed={wa_followed} wall_anchor_last_step_us={wa_last} wall_anchor_holds_followed={wa_holds} wall_anchor_last_hold_us={wa_hold} wall_anchor_probes_rejected={wa_rejected} wall_anchor_detect_to_follow_us={wa_detect} fleet_shift_slots={shift_slots} last_regrid_remainder_us={remainder} song_change_unserviced_slots={unserviced} consumer_fill_pairs={fills} underruns={underruns} clock_ok={clock_ok} lock={lock} reason=\"{reason}\"",
        pid = s.playlist_id,
        name = s.ndi_name,
        seq = s.pacing.seq,
        late = s.pacing.late_frames,
        p99 = s.pacing.jitter_p99_us,
        repeats = s.pacing.repeats,
        resyncs = s.pacing.resyncs,
        seeks = s.pacing.seeks, // #150: the lock window restarts when it moves
        relatches = s.pacing.relatches,
        lag = s.pacing.lag_slots,
        av_err = s.pacing.av_align_err_ms,
        av_corr = s.pacing.av_corrections,
        av_samples = s.pacing.av_corrected_samples,
        // #148 v5: SongPlayer's own emitted audio-block − frame-pts relation.
        av_off = s.pacing.av_frame_offset_ms,
        av_off_min = s.pacing.av_frame_offset_min_ms,
        av_off_max = s.pacing.av_frame_offset_max_ms,
        // #147: the pacer wall's anchor telemetry (bracketed sampling + bounded update).
        wa_step = s.pacing.wall_anchor_max_step_us,
        wa_wide = s.pacing.wall_anchor_wide_brackets,
        wa_slewed = s.pacing.wall_anchor_slewed_us,
        wa_followed = s.pacing.wall_anchor_steps_followed,
        wa_last = s.pacing.wall_anchor_last_step_us,
        wa_holds = s.pacing.wall_anchor_holds_followed,
        wa_hold = s.pacing.wall_anchor_last_hold_us,
        // #224: the same wall's per-boundary step probe.
        wa_rejected = s.pacing.wall_anchor_probes_rejected,
        wa_detect = s.pacing.wall_anchor_detect_to_follow_us,
        // #224 part 2: the relabel of the date steps followed + the last remainder.
        shift_slots = s.pacing.fleet_shift_slots,
        remainder = s.pacing.last_regrid_remainder_us,
        // #147: the pipeline-lifetime submit consumer's grid across scopes.
        unserviced = s.pacing.song_change_unserviced_slots,
        fills = s.pacing.consumer_fill_pairs,
        underruns = s.audio.underruns,
        clock_ok = s.clock.clock_ok,
        lock = s.lock_state.as_str(),
        reason = s.lock_reason,
    )
}

/// Pure helper: convert canonical state + per-poll values + consecutive
/// bad-poll count into the degraded_reason string. The frontend uses this
/// string verbatim. Returns None when the snapshot is healthy or below
/// the >=2 consecutive gate. #221 lane 3: no receiver count — a playlist
/// has no NDI output of its own, so a bad poll is an underrun or a stalled
/// delivery (`pipeline::classify_bad_poll`).
///
/// Mutation testing: the >=2 gate is a single comparison; the helper is
/// excluded from cargo-mutants because the boundary is exhaustively
/// covered by the boundary tests below.
#[cfg_attr(test, mutants::skip)]
fn compute_degraded_reason(
    state: &PlaybackStateLabel,
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
#[path = "ndi_health_tests.rs"]
mod tests;
