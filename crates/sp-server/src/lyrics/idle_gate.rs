//! #154 idle gate — no heavy lyrics processing while the LED wall is in use.
//!
//! The shared win-resolume box runs the LED-wall playback path (MF hardware
//! decoder → NDI → OBS → Arena) on the SAME GPU the lyrics worker's vocal
//! isolation (Mel-Roformer + dereverb) and mtl forced-alignment need. Running
//! those heavy subprocesses while the wall is live starves the decoder/NDI
//! latency path — playback stutters and, on 2026-09-14, the display driver
//! timed out (`LiveKernelEvent` 141 ×5) and the OS hard-reset.
//!
//! This module is the PURE decision core of the gate (owner directive #154 /
//! #144: no heavy processing while the wall is in use). It has no I/O and is
//! unit-tested directly; the thin `impl LyricsWorker` seam that reads the live
//! in-process handles (`NdiHealthRegistry` snapshots + `ObsState`) lives at the
//! bottom of the file so `worker.rs` stays under the 1000-line cap.
//!
//! It only changes WHEN heavy stages run, never the output — so it is NOT a
//! `LYRICS_PIPELINE_VERSION` bump. The `gpu_policy` WDDM priority + VRAM cap
//! stay as defence in depth (secondary); this gate is the primary mechanism.

use crate::playback::ndi_health::PlaybackStateLabel;
use std::time::{Duration, Instant};

/// A read-only snapshot of "is the wall in use right now?" — the three signals
/// that make heavy lyrics processing contend with live output on the shared
/// win-resolume box, plus a `known` readiness flag (#167).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WallActivity {
    /// At least one playback pipeline is `Playing` on OBS program.
    pub any_playing: bool,
    /// OBS is actively streaming an output.
    pub obs_streaming: bool,
    /// OBS is actively recording an output.
    pub obs_recording: bool,
    /// Whether this reading is TRUSTWORTHY yet (#167). Before every created
    /// pipeline has reported a heartbeat (or the startup grace elapses) the NDI
    /// health registry is still empty, so a missing `Playing` reads as idle — the
    /// unsafe default for a box whose whole job is to keep the wall fed. While
    /// `known == false` the reading is UNKNOWN and [`in_use`](Self::in_use)
    /// returns `true`, so a heavy step picks the CPU plan / defers at startup
    /// rather than spawning a GPU RoFormer on a live wall.
    pub known: bool,
}

impl Default for WallActivity {
    /// A default reading is a KNOWN idle wall — the safe value for unit tests and
    /// the no-registry seam. (The startup-UNKNOWN case is constructed explicitly
    /// with `known: false`; #167.)
    fn default() -> Self {
        Self {
            any_playing: false,
            obs_streaming: false,
            obs_recording: false,
            known: true,
        }
    }
}

impl WallActivity {
    /// The wall is in use iff the reading is not yet trustworthy (`!known`, #167)
    /// or any of the three live signals is active.
    pub(crate) fn in_use(&self) -> bool {
        !self.known || self.any_playing || self.obs_streaming || self.obs_recording
    }

    /// Short, generic reason for the gate log / dashboard, or `None` when idle.
    /// Playing takes precedence (the most direct "wall is showing content"); an
    /// UNKNOWN reading (#167) reports the startup grace.
    pub(crate) fn reason(&self) -> Option<&'static str> {
        if self.any_playing {
            Some("output playing")
        } else if self.obs_streaming {
            Some("OBS streaming")
        } else if self.obs_recording {
            Some("OBS recording")
        } else if !self.known {
            Some("startup grace (wall unknown)")
        } else {
            None
        }
    }
}

/// The startup readiness grace (#167): the wall reading stays UNKNOWN no longer
/// than this after engine start, even if a pipeline never reports (a cap so a
/// stuck heartbeat cannot defer heavy work forever).
pub(crate) const STARTUP_GRACE: Duration = Duration::from_secs(30);

/// The hard startup floor (#167): NO heavy step of any kind runs for this long
/// after engine start, so the wall pipelines come up on a fully quiet box (the
/// post-deploy E2E samples the engine in exactly this window).
pub(crate) const HEAVY_STEP_STARTUP_FLOOR: Duration = Duration::from_secs(60);

/// Is the wall-activity reading trustworthy yet (#167)? `expected` pipelines were
/// created; `reported` have sent at least one heartbeat. The reading is KNOWN
/// once every created pipeline has reported OR the startup grace has elapsed —
/// whichever comes first. `expected == 0` (no pipelines created yet) is NOT known
/// until the grace elapses, so a box that has not yet created its outputs still
/// defers heavy work.
pub(crate) fn activity_known(expected: usize, reported: usize, since_start: Duration) -> bool {
    (expected > 0 && reported >= expected) || since_start >= STARTUP_GRACE
}

/// Should EVERY heavy step be deferred right now purely because the engine only
/// just started (#167)? True for the first [`HEAVY_STEP_STARTUP_FLOOR`] after
/// engine start, regardless of wall activity. Pure.
pub(crate) fn startup_floor_defers(since_start: Duration) -> bool {
    since_start < HEAVY_STEP_STARTUP_FLOOR
}

/// True iff any pipeline health snapshot reports `Playing`. `Playing` is
/// already reconciled by `handle_health_snapshot` to mean "an output is
/// playing AND OBS is on its scene" (a playing-but-off-program pipeline is
/// mapped to `Paused`), so this is precisely "the wall is showing an output".
pub(crate) fn any_playing<'a>(mut states: impl Iterator<Item = &'a PlaybackStateLabel>) -> bool {
    states.any(|s| matches!(s, PlaybackStateLabel::Playing))
}

/// The idle-only gate decision: should heavy work be deferred right now? Pure.
/// `gate_enabled` is `true` in production (#162 folded the old
/// `lyrics_gate_when_playing` boolean into `lyrics_processing_mode`; this pure
/// core is only consulted on the idle-only path, where the gate is on), and the
/// pure `false` case is retained for the decision-core tests.
pub(crate) fn should_defer(gate_enabled: bool, activity: WallActivity) -> bool {
    gate_enabled && activity.in_use()
}

/// Idle-settle hysteresis (2026-09-14 incident): a single idle sample is not
/// enough — OBS scene re-evaluation, operator scene switches and song changes
/// flip every pipeline off `Playing` for a few seconds, and the worker resumed
/// a 5-minute GPU job while SP-fast was playing. Heavy work resumes only after
/// the wall has read idle for this long WITHOUT interruption.
pub(crate) const WALL_IDLE_SETTLE: Duration = Duration::from_secs(30);

/// Tracks how long the wall has read continuously idle. Pure — `now` is passed in.
#[derive(Debug, Default)]
pub(crate) struct IdleSettle {
    idle_since: Option<Instant>,
}

impl IdleSettle {
    /// Feed one gate sample. Returns `true` when heavy work must still be
    /// DEFERRED: the wall is in use, or it went idle less than
    /// `WALL_IDLE_SETTLE` ago. A busy sample resets the settle clock.
    pub(crate) fn defer(&mut self, in_use: bool, now: Instant) -> bool {
        if in_use {
            self.idle_since = None;
            return true;
        }
        let since = *self.idle_since.get_or_insert(now);
        now.saturating_duration_since(since) < WALL_IDLE_SETTLE
    }

    /// How long the wall has been continuously idle (None while in use).
    pub(crate) fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.idle_since
            .map(|since| now.saturating_duration_since(since))
    }
}

/// Once-per-transition logger for the gate, plus the shared idle-settle clock.
/// `note` returns `Some(line)` only when the busy state flips, so the
/// "waiting — wall in use" INFO logs on each transition rather than every worker
/// tick. `settle` carries the idle-settle hysteresis (see `defer_settled`) —
/// both workers already hold this struct in a `Mutex`, so parking the settle
/// state here needs no worker constructor changes.
#[derive(Debug, Default)]
pub(crate) struct GateLog {
    last_busy: Option<bool>,
    settle: IdleSettle,
}

impl GateLog {
    /// Record the current busy state and return a log line iff it changed.
    /// `detail` names the concrete cause (e.g. `"SP-fast Playing"`) for the
    /// busy→ line; the idle→ line reports the worker resuming. Busy-flag
    /// transitions only — the idle-settle clock lives in `defer_settled`.
    pub(crate) fn note(&mut self, busy: bool, detail: &str) -> Option<String> {
        if self.last_busy == Some(busy) {
            return None;
        }
        let was_busy = self.last_busy == Some(true);
        self.last_busy = Some(busy);
        if busy {
            Some(format!("lyrics_worker: waiting — wall in use ({detail})"))
        } else if was_busy {
            // Only log the resume when we were actually waiting — the first-ever
            // (idle) observation at startup must not emit a spurious line.
            Some("lyrics_worker: wall idle — resuming heavy processing".to_string())
        } else {
            None
        }
    }

    /// The settled gate decision both workers route through: defer heavy work
    /// while the gate is enabled AND the wall is either in use OR has read idle
    /// for less than `WALL_IDLE_SETTLE`. A disabled gate never defers (unchanged
    /// behaviour). Feeds one sample to the shared `IdleSettle` clock.
    pub(crate) fn defer_settled(
        &mut self,
        gate_enabled: bool,
        activity: WallActivity,
        now: Instant,
    ) -> bool {
        gate_enabled && self.settle.defer(activity.in_use(), now)
    }
}

/// Read a wall-activity snapshot from the in-process handles, for a worker that
/// is NOT the `LyricsWorker` (the #14 stem worker reuses this gate). Same data
/// source as `LyricsWorker::wall_activity`; `None` handles read as idle. Kept a
/// free function so both workers share ONE implementation without the stem
/// worker touching `worker.rs`.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn wall_activity_from(
    ndi_health_registry: Option<&std::sync::Arc<crate::playback::ndi_health::NdiHealthRegistry>>,
    obs_state: Option<&std::sync::Arc<tokio::sync::RwLock<crate::obs::ObsState>>>,
) -> WallActivity {
    let (any_playing, known) = match ndi_health_registry {
        Some(reg) => (
            any_playing(reg.snapshots().iter().map(|s| &s.state)),
            // #167: the reading is trustworthy only once every created pipeline
            // has reported (or the startup grace elapsed). No registry (unit
            // tests) → a known-idle wall.
            activity_known(
                reg.created_pipelines(),
                reg.reported_pipelines(),
                reg.since_created(),
            ),
        ),
        None => (false, true),
    };
    let (obs_streaming, obs_recording) = match obs_state {
        Some(obs) => {
            let s = obs.read().await;
            (s.streaming, s.recording)
        }
        None => (false, false),
    };
    WallActivity {
        any_playing,
        obs_streaming,
        obs_recording,
        known,
    }
}

// ---------------------------------------------------------------------------
// Live-handle seam — reads the in-process engine health registry + OBS state.
// I/O only (RwLock reads + one DB setting read); the decision it feeds is the
// pure logic above. Kept here (not in worker.rs) for the 1000-line cap.
// ---------------------------------------------------------------------------

impl crate::lyrics::worker::LyricsWorker {
    /// Read the live wall-activity snapshot from the in-process handles. Uses
    /// the engine's own `NdiHealthRegistry` (same data as `/api/v1/ndi/health`,
    /// no HTTP loop-back) and the shared `ObsState`. Missing handles (unit
    /// tests) read as idle.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_activity(&self) -> WallActivity {
        let (any_playing, known) = match &self.ndi_health_registry {
            Some(reg) => (
                any_playing(reg.snapshots().iter().map(|s| &s.state)),
                // #167: trustworthy only once every created pipeline reported
                // (or the grace elapsed); no registry (tests) → known-idle.
                activity_known(
                    reg.created_pipelines(),
                    reg.reported_pipelines(),
                    reg.since_created(),
                ),
            ),
            None => (false, true),
        };
        let (obs_streaming, obs_recording) = match &self.obs_state {
            Some(obs) => {
                let s = obs.read().await;
                (s.streaming, s.recording)
            }
            None => (false, false),
        };
        WallActivity {
            any_playing,
            obs_streaming,
            obs_recording,
            known,
        }
    }

    /// Full gate evaluation for `idle-only` mode: whether heavy work should be
    /// deferred now, plus the activity snapshot (for the log detail / dashboard).
    /// Only called from the idle-only control path (`loop_should_defer` /
    /// `defer_before_mtl`), where the gate is definitionally ON (#162 folded the
    /// old `lyrics_gate_when_playing` boolean into `lyrics_processing_mode`), so
    /// `gate_enabled` is always `true` here.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_gate_should_defer(&self) -> (bool, WallActivity) {
        let activity = self.wall_activity().await;
        // Idle-settle hysteresis lives in the shared `GateLog` (held in a Mutex
        // by both workers). Sample it AFTER the awaits and drop the lock before
        // returning — never hold the mutex across an `.await`.
        let now = Instant::now();
        let defer = match self.wall_gate_log.lock() {
            Ok(mut g) => g.defer_settled(true, activity, now),
            Err(_) => should_defer(true, activity),
        };
        (defer, activity)
    }

    /// Human detail for the gate log / dashboard, e.g. `"SP-fast Playing"`. For
    /// the playing case it names the actual on-program NDI output; otherwise it
    /// falls back to the generic `WallActivity::reason`.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_busy_detail(&self, activity: WallActivity) -> String {
        if !activity.in_use() {
            // Deferring only because the idle-settle window has not elapsed yet
            // (the wall is idle right now). Read the settle clock under the same
            // shared lock the gate decision used.
            let idle_secs = match self.wall_gate_log.lock() {
                Ok(g) => g
                    .settle
                    .idle_for(Instant::now())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                Err(_) => 0,
            };
            return format!(
                "wall just went idle — settling {}s of {}s",
                idle_secs,
                WALL_IDLE_SETTLE.as_secs()
            );
        }
        if activity.any_playing
            && let Some(reg) = &self.ndi_health_registry
            && let Some(name) = reg
                .snapshots()
                .iter()
                .find(|s| matches!(s.state, PlaybackStateLabel::Playing))
                .map(|s| s.ndi_name.clone())
        {
            return format!("{name} Playing");
        }
        activity.reason().unwrap_or("wall in use").to_string()
    }

    /// Emit the once-per-transition INFO log for the gate. Call every tick with
    /// the current busy state; `GateLog` suppresses no-change repeats.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn note_wall_gate(&self, busy: bool, detail: &str) {
        if let Ok(mut g) = self.wall_gate_log.lock()
            && let Some(line) = g.note(busy, detail)
        {
            tracing::info!("{line}");
        }
    }

    /// Gate #2 (#154): after isolation, before the mtl spawn. If the wall is
    /// busy now, broadcast the waiting stage for this song and return `true` so
    /// the caller defers the WHOLE song (`WaitingForWall`) rather than spawning
    /// the second heavy stage or degrading to the g35t base tier. Only call when
    /// mtl would actually run heavy work (a candidate + an isolated vocal WAV).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn defer_before_mtl(&self) -> bool {
        let (defer, activity) = self.wall_gate_should_defer().await;
        if defer {
            let detail = self.wall_busy_detail(activity).await;
            self.note_wall_gate(true, &detail);
            // Same song-less "waiting — wall in use" badge as the loop-level
            // gate (`enter_wall_wait`), so a mid-song defer does not flicker the
            // dashboard between a song-named waiting stage and the generic badge.
            self.enter_wall_wait(&detail).await;
        }
        defer
    }

    /// Surface the "waiting — wall in use" worker state to the dashboard through
    /// the same `current_processing` field the WS `LyricsQueueUpdate` carries.
    /// A synthetic (song-less) `LyricsProcessingState` — the dashboard renders a
    /// badge from the stage when song/artist are empty.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn enter_wall_wait(&self, detail: &str) {
        let state = sp_core::ws::LyricsProcessingState {
            video_id: 0,
            youtube_id: String::new(),
            song: String::new(),
            artist: String::new(),
            stage: format!("waiting — wall in use ({detail})"),
            provider: None,
            started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        };
        *self.current_processing.write().await = Some(state);
    }
}

#[cfg(test)]
#[path = "idle_gate_tests.rs"]
mod tests;
