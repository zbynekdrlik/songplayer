// crates/sp-server/src/playback/ndi_health.rs
//! NDI per-pipeline health snapshot types + lock-free registry +
//! engine aggregator.
//!
//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Mirrors `playback/recovery.rs` precedent and `resolume::ResolumeRegistry`
//! shape from PR #54.

use crate::obs::ndi_recovery::{NdiRecoveryTracker, RecoveryStep};
use crate::playback::clock_health::ClockHealth;
use crate::playback::lock_state::LOCK_WINDOW_100NS;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{
    RwLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tracing::warn;

/// The `degraded_reason` string a Playing-on-program pipeline gets when it has
/// zero NDI receivers — the "dark wall" state (#127). Single source of truth so
/// the receiver-recovery trigger and the dashboard read the same string.
pub(crate) const DARK_WALL_REASON: &str = "no NDI receiver — wall is dark";

/// #196: the `degraded_reason` a Playing-on-program output gets when it is
/// dark (`connections==0`) but NO OBS NDI input advertises its stream — the
/// receiver-side recovery ladder is not the tool for this class (there is no
/// input to nudge), so we set a distinct reason and skip the ladder entirely,
/// which also stops the every-10 s degraded/recovered flap the incident saw
/// for an output whose OBS scene did not exist yet (SP-dabing).
pub(crate) const NO_OBS_INPUT_REASON: &str = "no OBS scene for this output";

/// #196: choose the effective dark reason for a Playing-on-program output.
/// When the base reason is the dark-wall reason but no OBS input advertises
/// this output, return [`NO_OBS_INPUT_REASON`] instead — which is NOT the
/// dark-wall reason, so the caller's `is_dark` is false and the ladder never
/// runs. Any other base reason passes through unchanged. Pure so the branch
/// is mutation-scored.
pub(crate) fn effective_dark_reason(base: Option<String>, has_obs_input: bool) -> Option<String> {
    if base.as_deref() == Some(DARK_WALL_REASON) && !has_obs_input {
        Some(NO_OBS_INPUT_REASON.to_string())
    } else {
        base
    }
}

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
    /// #173 round 2: the dark-wall recovery ladder rung last fired in the current
    /// dark outage (`ClearRestore` / `ToggleSceneItem` / `RecreateInput`), or
    /// `None` when not recovering. Cleared to `None` the moment the receiver
    /// re-attaches, so the dashboard / E2E can see which rung recovered a wall.
    pub recovery_step: Option<RecoveryStep>,
    /// #196: the sender's advertised source URL (`host:port`) as the NDI runtime
    /// assigned it, e.g. `"10.77.9.201:5963"`. `None` until the SDK reports one.
    /// Makes a restart's name→port shuffle — the root cause of the
    /// dark-wall-after-restart incident — visible on `/api/v1/ndi/health`.
    pub sender_url: Option<String>,
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
    /// Wall-clock audio-emitter telemetry (#192) for the SDK-clocked path. The
    /// paced path leaves this at `enabled=false`; the SDK-clocked path fills it
    /// from the dedicated emitter thread (`playback/audio_emitter.rs`).
    #[serde(default)]
    pub emitter: EmitterStats,
}

/// Wall-clock NDI audio-emitter telemetry (#192), surfaced on
/// `GET /api/v1/ndi/health` as `audio.emitter`. On the SDK-clocked path the
/// emitter clocks the NDI audio stream off the wall clock (one 1600-sample
/// block per 33.333 ms grid slot, silence when the ring is short) so the stream
/// never starves at song transitions and the receiver's servo sees a clean
/// 48 kHz rate. `enabled=false` + all-zero is the default (paced / idle path).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmitterStats {
    /// Whether the wall-clock audio emitter is active for this pipeline.
    pub enabled: bool,
    /// `"sdk-video/wallclock-audio"` when enabled, empty otherwise.
    pub mode: String,
    /// Silence blocks emitted (cumulative) — grows ONLY at transitions / stalls.
    pub silence_blocks: u64,
    /// Current ring depth (ms of buffered decoded audio ahead of the grid).
    pub ring_depth_ms: u64,
    /// 99th-percentile emit jitter (µs) — the emit thread's grid accuracy.
    pub emit_jitter_p99_us: u64,
    /// Emits that woke a whole block or more past their grid boundary
    /// (cumulative) — should stay ≈ 0 with a TIME_CRITICAL emit thread.
    pub late_blocks: u64,
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
    /// #192 round 3: the worst per-call `send_video_async` duration (µs) over the
    /// drained window, from the submitter's `SubmitHist`. 0 with no submit.
    pub submit_call_us_max: u64,
    /// #192 round 3: the 99th-percentile per-call `send_video_async` duration
    /// (µs) over the drained window. 0 with no submit.
    pub submit_call_us_p99: u64,
}

/// Lock-free-read registry holding the latest health snapshot per pipeline.
/// Mirrors `crate::resolume::ResolumeRegistry` from PR #54: one Arc held by
/// the playback engine (writer) and another by `AppState` (reader). The
/// `RwLock` is held only for short copy-out reads in `snapshots()`; the
/// returned Vec is owned data, no lifetimes leak out.
pub struct NdiHealthRegistry {
    snapshots: RwLock<HashMap<i64, PipelineHealthSnapshot>>,
    /// #196: the advertised source URL (`host:port`) each output's sender was
    /// assigned, recorded once at sender creation and read back onto every
    /// health snapshot. Kept out of `snapshots` so it survives across heartbeat
    /// rebuilds and is independent of which heartbeat path (SDK-clocked/paced)
    /// runs.
    sender_urls: RwLock<HashMap<i64, String>>,
    /// #127 receiver-recovery state. Composed here (rather than as a new
    /// `PlaybackEngine` field) so the engine reaches it through the `Arc` it
    /// already holds; the single writer is `handle_health_snapshot`.
    recovery: NdiRecoveryTracker,
    /// #167 readiness: the wall-clock instant this registry (≈ the process /
    /// engine) started, so the heavy-work startup grace + floor are measured
    /// off the same `Arc` both heavy workers already hold.
    created_at: Instant,
    /// #167 readiness: how many playback pipelines the engine has CREATED
    /// (`register_pipeline`), as distinct from how many have REPORTED a
    /// heartbeat (`snapshots` len). The gap is the "not proven idle yet" window
    /// that must read as wall-in-use.
    expected_pipelines: AtomicUsize,
    /// #196: per-output receiver count read from the settings table at startup
    /// — the PRE-restart snapshot the post-restart self-check compares against.
    /// Seeded once (`seed_pre_restart_counts`) before senders are created; never
    /// overwritten in memory (the live counts are persisted to the DB instead).
    pre_restart_counts: RwLock<HashMap<i64, i32>>,
    /// #196: outputs that have reached `connections >= 1` at least once since
    /// this process started. Once an output reconnects, the restart-reconnect
    /// succeeded and the self-check never flags it again this process (a later
    /// legitimate off-program drop is not a restart failure).
    reconnected: RwLock<HashSet<i64>>,
    /// #196: outputs already WARN-logged for "no receiver after restart" — so
    /// the WARN fires once per output, not once per 5 s poll. Cleared when the
    /// output recovers, so a genuinely new failure re-warns.
    warned_no_receiver: RwLock<HashSet<i64>>,
    /// #196: when the startup senders became ready — the +30 s self-check clock.
    /// `None` until `mark_senders_ready`.
    senders_ready_at: RwLock<Option<Instant>>,
    /// #198 item 5: pending receiver-count DB writes, keyed by playlist so a
    /// flapping count debounces to its LATEST value (a `HashMap` insert dedups).
    /// The SYNC `handle_health_snapshot` only QUEUES here (no `tokio::spawn` — a
    /// spawn from a sync caller with no reactor panics, and one detached task per
    /// 5 s poll per output has no write ordering); the async pipeline-event
    /// handler drains and awaits them in order.
    pending_persist: RwLock<HashMap<i64, i32>>,
}

impl NdiHealthRegistry {
    /// Construct an empty registry. Callers wrap in `Arc::new(...)` when
    /// sharing across the playback engine and `AppState` — matches the
    /// `ResolumeRegistry` precedent in `crate::resolume::mod`.
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
            sender_urls: RwLock::new(HashMap::new()),
            recovery: NdiRecoveryTracker::new(),
            created_at: Instant::now(),
            expected_pipelines: AtomicUsize::new(0),
            pre_restart_counts: RwLock::new(HashMap::new()),
            reconnected: RwLock::new(HashSet::new()),
            warned_no_receiver: RwLock::new(HashSet::new()),
            senders_ready_at: RwLock::new(None),
            pending_persist: RwLock::new(HashMap::new()),
        }
    }

    /// #198 item 5: queue a receiver-count persist for `playlist_id` (the latest
    /// value wins — a flapping count debounces). Sync + non-panicking (no
    /// reactor needed); the async event handler drains it via
    /// [`drain_pending_persists`](Self::drain_pending_persists). A poisoned lock
    /// silently drops the write (the next change re-queues; a lost baseline only
    /// affects the NEXT restart's self-check, never live playback).
    pub fn queue_receiver_count_persist(&self, playlist_id: i64, connections: i32) {
        if let Ok(mut m) = self.pending_persist.write() {
            m.insert(playlist_id, connections);
        }
    }

    /// #198 item 5: take (and clear) the pending receiver-count persists so the
    /// async caller can write them in order. `mem::take` empties the buffer, so
    /// a value is written at most once per drain regardless of how many polls
    /// queued it.
    pub fn drain_pending_persists(&self) -> Vec<(i64, i32)> {
        match self.pending_persist.write() {
            Ok(mut m) => std::mem::take(&mut *m).into_iter().collect(),
            Err(_) => Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // #196: post-restart receiver self-check state
    // -----------------------------------------------------------------------

    /// #196: seed the pre-restart per-output receiver baseline (read from the
    /// settings table at startup). Called once before the startup senders are
    /// created; a poisoned lock silently keeps the empty baseline.
    pub fn seed_pre_restart_counts(&self, counts: HashMap<i64, i32>) {
        if let Ok(mut m) = self.pre_restart_counts.write() {
            *m = counts;
        }
    }

    /// #196: the pre-restart receiver count recorded for `playlist_id`, or `0`
    /// if none (unknown output, or a poisoned lock — the safe direction: an
    /// unknown output is treated as "had no receiver", so it is not flagged
    /// unless it is currently on program).
    pub fn pre_restart_count(&self, playlist_id: i64) -> i32 {
        self.pre_restart_counts
            .read()
            .ok()
            .and_then(|m| m.get(&playlist_id).copied())
            .unwrap_or(0)
    }

    /// #196: mark that the startup senders are ready — starts the +30 s
    /// self-check clock. Idempotent: only the FIRST call sets the instant.
    pub fn mark_senders_ready(&self) {
        if let Ok(mut t) = self.senders_ready_at.write() {
            if t.is_none() {
                *t = Some(Instant::now());
            }
        }
    }

    /// #196: how long since the startup senders were ready, or `None` if they
    /// are not ready yet (before which nothing is self-checked). A poisoned lock
    /// reads as `None` (no self-check — the safe direction).
    ///
    /// mutants::skip — a wall-clock elapsed read (like `since_created`); a mutant
    /// is catchable only by a wall-time assertion. The self-check DECISION it
    /// feeds (`sp_core::health::no_receiver_after_restart`) is exhaustively
    /// mutation-scored in sp-core.
    #[cfg_attr(test, mutants::skip)]
    pub fn elapsed_since_ready(&self) -> Option<Duration> {
        self.senders_ready_at
            .read()
            .ok()
            .and_then(|t| t.map(|i| i.elapsed()))
    }

    /// #196: record that `playlist_id` has reached `connections >= 1` since the
    /// restart (latches the self-check off for it this process).
    pub fn mark_reconnected(&self, playlist_id: i64) {
        if let Ok(mut s) = self.reconnected.write() {
            s.insert(playlist_id);
        }
    }

    /// #196: whether `playlist_id` has reconnected since the restart.
    /// (`is_ok_and`: a poisoned lock reads as `false` inside std, so there is no
    /// separate mutable fallback literal — the reachable `false` comes from the
    /// set not containing the id, which the unit test exercises.)
    pub fn has_reconnected(&self, playlist_id: i64) -> bool {
        self.reconnected
            .read()
            .is_ok_and(|s| s.contains(&playlist_id))
    }

    /// #196: record the "no receiver after restart" WARN for `playlist_id`.
    /// Returns `true` iff this is the FIRST time (so the caller logs exactly
    /// once per output, not once per poll). A poisoned lock returns `false`
    /// (skip the WARN rather than spam) — the `false` lives inside `is_ok_and`,
    /// so the only observable return is the reachable `HashSet::insert` result.
    pub fn mark_warned_no_receiver(&self, playlist_id: i64) -> bool {
        self.warned_no_receiver
            .write()
            .is_ok_and(|mut s| s.insert(playlist_id))
    }

    /// #196: clear the "no receiver after restart" WARN latch when an output
    /// recovers, so a genuinely new failure re-warns.
    pub fn clear_warned_no_receiver(&self, playlist_id: i64) {
        if let Ok(mut s) = self.warned_no_receiver.write() {
            s.remove(&playlist_id);
        }
    }

    /// #196: record the advertised source URL a sender was assigned at creation.
    /// `None` is ignored (keeps any previously-recorded URL rather than erasing
    /// it — a stub/failed create should not clobber a known URL).
    pub fn set_sender_url(&self, playlist_id: i64, url: Option<String>) {
        let Some(url) = url else {
            return;
        };
        if let Ok(mut map) = self.sender_urls.write() {
            map.insert(playlist_id, url);
        }
    }

    /// #196: the advertised source URL recorded for `playlist_id`, or `None` if
    /// none has been recorded yet. A poisoned lock reads as `None`.
    pub fn sender_url(&self, playlist_id: i64) -> Option<String> {
        self.sender_urls
            .read()
            .ok()
            .and_then(|m| m.get(&playlist_id).cloned())
    }

    /// #167: record that the engine created one more playback pipeline. Called
    /// from `PlaybackEngine::ensure_pipeline` at creation (once per new
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

    /// #127 / #173: evaluate the receiver-recovery ladder for one pipeline and
    /// apply the state mutation. Returns `Some(step)` iff the engine should
    /// execute that OBS recovery rung now, else `None`. `is_dark` is true iff the
    /// pipeline is Playing on program with the dark-wall `degraded_reason` set
    /// (`connections == 0`).
    pub fn evaluate_recovery(
        &self,
        playlist_id: i64,
        is_dark: bool,
        consecutive_bad_polls: u32,
        now_100ns: i64,
    ) -> Option<RecoveryStep> {
        self.recovery
            .evaluate(playlist_id, is_dark, consecutive_bad_polls, now_100ns)
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
        let base_degraded_reason = compute_degraded_reason(
            &canonical_state,
            connections,
            observed_fps,
            nominal_fps,
            consecutive_bad_polls,
        );
        // #196 item 5: if this output is dark (Playing on program, connections=0)
        // but NO OBS NDI input advertises its stream, the receiver-side recovery
        // ladder is not the tool for it — set a distinct reason and skip the
        // ladder (`effective_dark_reason` returns a NON-dark-wall reason, so
        // `is_dark` below is false). This stops the every-10 s degraded/recovered
        // flap the incident saw for an output whose OBS scene did not exist yet.
        let has_obs_input = self.output_has_obs_input(playlist_id);
        let degraded_reason = effective_dark_reason(base_degraded_reason, has_obs_input);

        // #196 item 4: post-restart receiver self-check. Once an output reaches
        // a receiver, latch it as reconnected (and clear any earlier WARN); then
        // — 30 s after the startup senders are ready — an output on program (or
        // one that had a receiver before the restart) that still has none is
        // flagged with a DISTINCT reason, so the receiver-side recovery ladder
        // is NOT run for it (it cannot clear a restart wedge — only another
        // restart re-rolls it). This is a NON-dark-wall reason, so `is_dark`
        // below stays false. The whole decision is the pure, mutation-scored
        // `sp_core::health::no_receiver_after_restart`.
        let on_program = matches!(canonical_state, PlaybackStateLabel::Playing);
        if connections >= 1 {
            self.ndi_health_registry.mark_reconnected(playlist_id);
            self.ndi_health_registry
                .clear_warned_no_receiver(playlist_id);
        }
        let elapsed_since_ready = self.ndi_health_registry.elapsed_since_ready();
        let reconnected = self.ndi_health_registry.has_reconnected(playlist_id);
        // #196: from the moment the senders are ready until an on-program output
        // reconnects, the #173 receiver-side ladder is suppressed for it — the
        // ladder cannot clear a restart wedge (it can deepen it), so it must
        // NEVER run during the post-restart window (the self-check surfaces the
        // failure at +30 s instead). This closes the ~10–30 s window where the
        // dark-wall reason would otherwise arm the ladder before the +30 s
        // `no_receiver_after_restart` reason takes over.
        let ladder_suppressed = sp_core::health::ladder_suppressed_after_restart(
            elapsed_since_ready,
            reconnected,
            on_program,
            connections,
        );
        let degraded_reason = if degraded_reason.as_deref() != Some(NO_OBS_INPUT_REASON)
            && sp_core::health::no_receiver_after_restart(
                elapsed_since_ready,
                reconnected,
                on_program,
                self.ndi_health_registry.pre_restart_count(playlist_id),
                connections,
            ) {
            if self
                .ndi_health_registry
                .mark_warned_no_receiver(playlist_id)
            {
                warn!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    "ndi: {} — receiver did not return after the restart (self-check; NOT running the ladder)",
                    sp_core::health::NO_RECEIVER_AFTER_RESTART_REASON,
                );
            }
            Some(sp_core::health::NO_RECEIVER_AFTER_RESTART_REASON.to_string())
        } else {
            degraded_reason
        };

        // Look up the previous snapshot from the registry to detect
        // connection-count changes and degraded transitions for logging.
        let prev = self
            .ndi_health_registry
            .snapshots()
            .into_iter()
            .find(|s| s.playlist_id == playlist_id);
        let prev_connections = prev.as_ref().map(|s| s.connections);
        let prev_degraded = prev.as_ref().and_then(|s| s.degraded_reason.clone());

        // #196: persist the current receiver count whenever it changes (incl.
        // the first snapshot) so the NEXT restart's self-check baseline knows
        // this output had (or lost) a receiver before it. #198 item 5: only
        // QUEUE it here — this handler is sync, and a `tokio::spawn` from a sync
        // caller with no reactor panics (and a flapping count would spawn one
        // detached, unordered task per 5 s poll). The async pipeline-event
        // handler drains and awaits the queued writes.
        if prev_connections != Some(connections) {
            self.ndi_health_registry
                .queue_receiver_count_persist(playlist_id, connections);
        }

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

        // #127 / #173 receiver-side recovery: evaluate the dark-wall ladder for
        // this pipeline BEFORE building the snapshot, so the fired rung is
        // recorded on it. `is_dark` = Playing on program with the dark-wall
        // reason (connections == 0) AND not in the #196 post-restart window
        // (where the ladder must never run — it cannot clear a restart wedge).
        let is_dark = degraded_reason.as_deref() == Some(DARK_WALL_REASON) && !ladder_suppressed;
        let recovery_step_fired = self.ndi_health_registry.evaluate_recovery(
            playlist_id,
            is_dark,
            consecutive_bad_polls,
            heartbeat_100ns,
        );
        // Surface the ladder rung on the snapshot: the rung fired this poll, or
        // the last rung still in flight this outage (carried from the previous
        // snapshot); cleared to None the moment the receiver re-attaches.
        let recovery_step = if is_dark {
            recovery_step_fired.or_else(|| prev.as_ref().and_then(|s| s.recovery_step))
        } else {
            None
        };

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
            recovery_step,
            // #196: the advertised host:port this sender landed on, recorded in
            // the registry at sender creation (`create_startup_senders` / the
            // runtime ensure path), so it survives across heartbeats and both
            // the SDK-clocked and paced heartbeat paths.
            sender_url: self.ndi_health_registry.sender_url(playlist_id),
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
                // #192 round 3: a third grep-stable line — the pipeline-loop
                // stage timing (decode / submit / audio max) + the raw
                // send_video_async call max/p99, so the A/B box test names the
                // stalling stage. Zero on the idle / paused / paced paths.
                info!(
                    "{}",
                    crate::playback::loop_stats::format_loop_stats_line(&ndi_name, &loop_stats)
                );
            }
        }

        self.ndi_health_registry.update(snapshot);

        // #127 / #173: if the ladder fired a recovery rung this poll, execute it
        // over the healthy OBS WebSocket (clear+restore → toggle → recreate).
        // The rung was chosen above by `evaluate_recovery`.
        if let Some(step) = recovery_step_fired {
            match self.obs_cmd_tx.as_ref() {
                Some(tx) => match tx.try_send(crate::obs::ObsCommand::NudgeNdiReceiver {
                    ndi_name: ndi_name.clone(),
                    step,
                }) {
                    Ok(()) => warn!(
                        playlist_id,
                        ndi_name = %ndi_name,
                        ?step,
                        "ndi-recovery: dark wall — running recovery rung over OBS"
                    ),
                    Err(e) => warn!(
                        playlist_id,
                        ndi_name = %ndi_name,
                        error = %e,
                        "ndi-recovery: failed to queue OBS recovery rung"
                    ),
                },
                None => warn!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    "ndi-recovery: dark wall but no OBS command channel wired"
                ),
            }
        }
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
        return Some(DARK_WALL_REASON.to_string());
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
