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

/// A read-only snapshot of "is the wall in use right now?" — the three signals
/// that make heavy lyrics processing contend with live output on the shared
/// win-resolume box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WallActivity {
    /// At least one playback pipeline is `Playing` on OBS program.
    pub any_playing: bool,
    /// OBS is actively streaming an output.
    pub obs_streaming: bool,
    /// OBS is actively recording an output.
    pub obs_recording: bool,
}

impl WallActivity {
    /// The wall is in use iff any of the three signals is active.
    pub(crate) fn in_use(&self) -> bool {
        self.any_playing || self.obs_streaming || self.obs_recording
    }

    /// Short, generic reason for the gate log / dashboard, or `None` when idle.
    /// Playing takes precedence (the most direct "wall is showing content").
    pub(crate) fn reason(&self) -> Option<&'static str> {
        if self.any_playing {
            Some("output playing")
        } else if self.obs_streaming {
            Some("OBS streaming")
        } else if self.obs_recording {
            Some("OBS recording")
        } else {
            None
        }
    }
}

/// True iff any pipeline health snapshot reports `Playing`. `Playing` is
/// already reconciled by `handle_health_snapshot` to mean "an output is
/// playing AND OBS is on its scene" (a playing-but-off-program pipeline is
/// mapped to `Paused`), so this is precisely "the wall is showing an output".
pub(crate) fn any_playing<'a>(mut states: impl Iterator<Item = &'a PlaybackStateLabel>) -> bool {
    states.any(|s| matches!(s, PlaybackStateLabel::Playing))
}

/// The gate decision: should heavy work be deferred right now? Pure — the
/// operator setting `lyrics_gate_when_playing` (default ON) is passed in as
/// `gate_enabled`; when OFF the gate never defers (today's behaviour).
pub(crate) fn should_defer(gate_enabled: bool, activity: WallActivity) -> bool {
    gate_enabled && activity.in_use()
}

/// Parse the `lyrics_gate_when_playing` DB setting. Default ON (`true`) so a
/// deploy/upgrade with no setting row gates by default; `false`/`0`/`off`/`no`
/// (case- and whitespace-insensitive) disable it. Mirrors the
/// `lyrics_worker_enabled` parse in `worker.rs`.
pub(crate) fn gate_setting_enabled(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "false" || v == "0" || v == "off" || v == "no")
        }
    }
}

/// Once-per-transition logger for the gate. `note` returns `Some(line)` only
/// when the busy state flips, so the "waiting — wall in use" INFO logs on each
/// transition rather than every 5-s worker tick.
#[derive(Debug, Default)]
pub(crate) struct GateLog {
    last_busy: Option<bool>,
}

impl GateLog {
    /// Record the current busy state and return a log line iff it changed.
    /// `detail` names the concrete cause (e.g. `"SP-fast Playing"`) for the
    /// busy→ line; the idle→ line reports the worker resuming.
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
        let any_playing = match &self.ndi_health_registry {
            Some(reg) => any_playing(reg.snapshots().iter().map(|s| &s.state)),
            None => false,
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
        }
    }

    /// The `lyrics_gate_when_playing` operator setting (default ON), read live
    /// each tick so a dashboard flip takes effect within one worker poll.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn gate_when_playing_enabled(&self) -> bool {
        let raw = crate::db::models::get_setting(&self.pool, "lyrics_gate_when_playing")
            .await
            .ok()
            .flatten();
        gate_setting_enabled(raw.as_deref())
    }

    /// Full gate evaluation: whether heavy work should be deferred now, plus the
    /// activity snapshot (for the log detail / dashboard).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_gate_should_defer(&self) -> (bool, WallActivity) {
        let enabled = self.gate_when_playing_enabled().await;
        let activity = self.wall_activity().await;
        (should_defer(enabled, activity), activity)
    }

    /// Human detail for the gate log / dashboard, e.g. `"SP-fast Playing"`. For
    /// the playing case it names the actual on-program NDI output; otherwise it
    /// falls back to the generic `WallActivity::reason`.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn wall_busy_detail(&self, activity: WallActivity) -> String {
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
    pub(crate) async fn defer_before_mtl(
        &self,
        video_id: i64,
        youtube_id: &str,
        song: &str,
        artist: &str,
        started_at_unix_ms: i64,
    ) -> bool {
        let (defer, activity) = self.wall_gate_should_defer().await;
        if defer {
            let detail = self.wall_busy_detail(activity).await;
            self.note_wall_gate(true, &detail);
            self.broadcast_stage(
                video_id,
                youtube_id,
                song,
                artist,
                &format!("waiting — wall in use ({detail})"),
                None,
                started_at_unix_ms,
            )
            .await;
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
