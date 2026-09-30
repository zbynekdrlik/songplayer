//! The log lines of one NDI health snapshot, a child module of
//! `ndi_health.rs` (split out of `handle_health_snapshot` for its length and
//! the file's 1000-line cap, #221 L4a review): a connection-count change, a
//! degraded / recovered transition, and the once-per-UTC-minute heartbeat,
//! genlock and loop-stats lines. Logging only.

use tracing::{info, warn};

use super::{PipelineHealthSnapshot, format_genlock_line, should_log_periodic_heartbeat};
use crate::playback::loop_stats::{LoopStats, format_loop_stats_line};

/// Log `snapshot` against its pipeline's previous one (`prev`).
/// `scene_active` and (#221 L4a) `receiver_expected` go on the heartbeat
/// line.
///
/// mutants::skip — logging only; the decisions it uses are pure and tested
/// (`should_log_periodic_heartbeat`, `format_genlock_line`,
/// `format_loop_stats_line`).
#[cfg_attr(test, mutants::skip)]
pub(super) fn log_health_snapshot(
    snapshot: &PipelineHealthSnapshot,
    prev: Option<&PipelineHealthSnapshot>,
    scene_active: bool,
    receiver_expected: bool,
    loop_stats: &LoopStats,
) {
    let playlist_id = snapshot.playlist_id;
    let ndi_name = snapshot.ndi_name.as_str();
    let connections = snapshot.connections;

    // Transition logging: connection-count change, degradation, recovery.
    if let Some(prev_connections) = prev.map(|s| s.connections)
        && prev_connections != connections
    {
        info!(
            playlist_id,
            ndi_name = %ndi_name,
            prev = prev_connections,
            now = connections,
            "ndi: connections changed"
        );
    }
    let degraded_reason = snapshot.degraded_reason.as_deref();
    let prev_degraded = prev.and_then(|s| s.degraded_reason.as_deref());
    if degraded_reason.is_some() && prev_degraded.is_none() {
        warn!(
            playlist_id,
            ndi_name = %ndi_name,
            reason = degraded_reason.unwrap_or(""),
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
    let prev_heartbeat_ts = prev.and_then(|s| s.last_heartbeat_ts);
    if let Some(cur) = snapshot.last_heartbeat_ts
        && should_log_periodic_heartbeat(prev_heartbeat_ts, cur)
    {
        info!(
            playlist_id,
            ndi_name = %ndi_name,
            state = ?snapshot.state,
            connections,
            frames_total = snapshot.frames_submitted_total,
            frames_5s = snapshot.frames_submitted_last_5s,
            observed_fps = format!("{:.1}", snapshot.observed_fps),
            nominal_fps = format!("{:.1}", snapshot.nominal_fps),
            scene_active,
            receiver_expected,
            "ndi: heartbeat"
        );
        // #149 item 2: a second, grep-stable genlock telemetry line
        // beside the heartbeat, same once-per-UTC-minute cadence.
        info!("{}", format_genlock_line(snapshot));
        // #192 round 3 + #168 r2: a third grep-stable line — decode/submit/
        // audio stage maxima (SDK-clocked path only) + the raw
        // send_video_async call max/p99, populated on BOTH the SDK-clocked
        // and paced paths, so the A/B / box-test-7 reads name the stall.
        info!("{}", format_loop_stats_line(ndi_name, loop_stats));
    }
}
