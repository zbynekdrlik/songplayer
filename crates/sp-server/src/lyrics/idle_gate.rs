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
        // RED stub — GREEN ORs the three signals.
        false
    }

    /// Short, generic reason for the gate log / dashboard, or `None` when idle.
    /// Playing takes precedence (the most direct "wall is showing content").
    pub(crate) fn reason(&self) -> Option<&'static str> {
        // RED stub — GREEN returns the specific active signal.
        None
    }
}

/// True iff any pipeline health snapshot reports `Playing`. `Playing` is
/// already reconciled by `handle_health_snapshot` to mean "an output is
/// playing AND OBS is on its scene" (a playing-but-off-program pipeline is
/// mapped to `Paused`), so this is precisely "the wall is showing an output".
pub(crate) fn any_playing<'a>(states: impl Iterator<Item = &'a PlaybackStateLabel>) -> bool {
    // RED stub — GREEN scans for PlaybackStateLabel::Playing.
    let _ = states;
    false
}

/// The gate decision: should heavy work be deferred right now? Pure — the
/// operator setting `lyrics_gate_when_playing` (default ON) is passed in as
/// `gate_enabled`; when OFF the gate never defers (today's behaviour).
pub(crate) fn should_defer(gate_enabled: bool, activity: WallActivity) -> bool {
    // RED stub — GREEN gates on enabled && in_use.
    let _ = (gate_enabled, activity);
    false
}

/// Parse the `lyrics_gate_when_playing` DB setting. Default ON (`true`) so a
/// deploy/upgrade with no setting row gates by default; `false`/`0`/`off`/`no`
/// disable it. Mirrors the `lyrics_worker_enabled` parse in `worker.rs`.
pub(crate) fn gate_setting_enabled(raw: Option<&str>) -> bool {
    // RED stub — GREEN parses the falsey tokens, default true.
    let _ = raw;
    false
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
        // RED stub — GREEN emits a line on transition only.
        let _ = (busy, detail);
        None
    }
}

#[cfg(test)]
#[path = "idle_gate_tests.rs"]
mod tests;
