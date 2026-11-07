//! The wall-activity reading the heavy steps run by (#154, #162, #167).
//!
//! The shared win-resolume box runs the LED-wall playback path (MF hardware
//! decoder → NDI → OBS → Arena) on the SAME GPU the heavy steps (vocal
//! dereverb, mtl forced alignment, stem separation, dub) need. Running them on
//! the GPU while the wall is live starves the decoder/NDI latency path —
//! playback stutters and, on 2026-09-14, the display driver timed out
//! (`LiveKernelEvent` 141 ×5) and the OS hard-reset. So every heavy step reads
//! [`WallActivity`] and runs CPU-idle while the wall is in use (#162,
//! `heavy_plan.rs`), GPU while it is idle; a GPU step that sees the wall go
//! busy is killed and re-run on CPU (`idle_gate_abort.rs`). Nothing waits for
//! an idle wall except the startup floor (#167).
//!
//! This module is the PURE decision core plus the thin `impl LyricsWorker`
//! seam that reads the live in-process handles (`NdiHealthRegistry` snapshots
//! and `ObsState`), kept here so `worker.rs` stays under the 1000-line cap.
//! It only changes WHERE heavy stages run, never the output — so it is NOT a
//! `LYRICS_PIPELINE_VERSION` bump.

use crate::playback::ndi_health::PlaybackStateLabel;
use std::time::Duration;

/// A read-only snapshot of "is the wall in use right now?" — the three signals
/// that make heavy lyrics processing contend with live output on the shared
/// win-resolume box, plus a `known` readiness flag (#167).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WallActivity {
    /// At least one playback pipeline is `Playing` on SongPlayer's program.
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
///
/// Both this floor and [`STARTUP_GRACE`] are measured from `NdiHealthRegistry`
/// creation (≈ engine start, `since_created`), NOT from per-pipeline readiness.
/// The assumption is that active pipelines are CREATED and report within this
/// window of engine start (true on every normal restart — they come up in
/// seconds, and the created/reported count path then reads them as known). A
/// pathologically slow cold boot that first creates a pipeline > 60 s after
/// engine start would lift the floor before that output plays; the reported-count
/// path still forces UNKNOWN once the pipeline is created-but-unreported, so the
/// residual risk is only the narrow gap before creation on such a boot.
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

/// Read a wall-activity snapshot from the in-process handles, for a worker that
/// is NOT the `LyricsWorker` (the stem, dub and mtl steps). Same data
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
// I/O only (RwLock reads); the decision it feeds is the pure logic above. Kept here (not in worker.rs) for the 1000-line cap.
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
