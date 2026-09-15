//! Unit tests for the #154 idle gate pure decision core.
//!
//! RED-first: these assert the intended behaviour against the stubbed module
//! (which returns the wrong constants), so they FAIL until the GREEN commit
//! fills in the logic. No I/O — the live-handle seam (`impl LyricsWorker`) is
//! exercised by the worker-loop test in `worker_tests.rs`.

use super::*;
use crate::playback::ndi_health::PlaybackStateLabel;
use std::time::Duration;

// ---- any_playing ----------------------------------------------------------

#[test]
fn any_playing_true_when_one_pipeline_playing() {
    let states = [
        PlaybackStateLabel::Paused,
        PlaybackStateLabel::Playing,
        PlaybackStateLabel::Idle,
    ];
    assert!(any_playing(states.iter()));
}

#[test]
fn any_playing_false_when_none_playing() {
    let states = [
        PlaybackStateLabel::Paused,
        PlaybackStateLabel::Idle,
        PlaybackStateLabel::WaitingForScene,
    ];
    assert!(!any_playing(states.iter()));
}

#[test]
fn any_playing_false_on_empty() {
    let states: [PlaybackStateLabel; 0] = [];
    assert!(!any_playing(states.iter()));
}

// ---- WallActivity::in_use / reason ---------------------------------------

#[test]
fn in_use_true_when_playing() {
    let a = WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    };
    assert!(a.in_use());
    assert_eq!(a.reason(), Some("output playing"));
}

#[test]
fn in_use_true_when_obs_streaming_only() {
    let a = WallActivity {
        any_playing: false,
        obs_streaming: true,
        obs_recording: false,
        known: true,
    };
    assert!(a.in_use());
    assert_eq!(a.reason(), Some("OBS streaming"));
}

#[test]
fn in_use_true_when_obs_recording_only() {
    let a = WallActivity {
        any_playing: false,
        obs_streaming: false,
        obs_recording: true,
        known: true,
    };
    assert!(a.in_use());
    assert_eq!(a.reason(), Some("OBS recording"));
}

#[test]
fn in_use_false_when_all_idle() {
    let a = WallActivity::default();
    assert!(!a.in_use());
    assert_eq!(a.reason(), None);
}

#[test]
fn reason_prefers_playing_over_obs() {
    let a = WallActivity {
        any_playing: true,
        obs_streaming: true,
        obs_recording: true,
        known: true,
    };
    assert_eq!(a.reason(), Some("output playing"));
}

// ---- should_defer --------------------------------------------------------

#[test]
fn should_defer_true_when_enabled_and_busy() {
    let busy = WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    };
    assert!(should_defer(true, busy));
}

#[test]
fn should_defer_false_when_gate_off_even_if_busy() {
    // Operator override (setting OFF) = today's behaviour: never defer.
    let busy = WallActivity {
        any_playing: true,
        obs_streaming: true,
        obs_recording: true,
        known: true,
    };
    assert!(!should_defer(false, busy));
}

#[test]
fn should_defer_false_when_enabled_and_idle() {
    assert!(!should_defer(true, WallActivity::default()));
}

// ---- GateLog transition logging ------------------------------------------

#[test]
fn gate_log_emits_only_on_transition() {
    let mut log = GateLog::default();
    // First observation of "busy" emits.
    assert!(log.note(true, "SP-fast Playing").is_some());
    // Repeated "busy" with no change is silent.
    assert!(log.note(true, "SP-fast Playing").is_none());
    assert!(log.note(true, "SP-slow Playing").is_none());
    // Flip to idle emits.
    assert!(log.note(false, "").is_some());
    // Repeated idle is silent.
    assert!(log.note(false, "").is_none());
    // Flip back to busy emits again.
    assert!(log.note(true, "OBS recording").is_some());
}

#[test]
fn gate_log_busy_line_carries_detail() {
    let mut log = GateLog::default();
    let line = log.note(true, "SP-fast Playing").expect("transition line");
    assert!(line.contains("waiting"), "line was: {line}");
    assert!(line.contains("SP-fast Playing"), "line was: {line}");
}

#[test]
fn gate_log_first_idle_observation_is_silent() {
    let mut log = GateLog::default();
    // Never was busy → the first idle tick must NOT emit a spurious "resuming".
    assert!(log.note(false, "").is_none());
    // The first busy still emits, and the following idle then reports the resume.
    assert!(log.note(true, "SP-fast Playing").is_some());
    assert!(log.note(false, "").is_some());
}

// ---- #167 startup grace: WallActivity.known + the two pure gates ----------

#[test]
fn activity_unknown_reads_as_in_use() {
    // The reproduction: before any pipeline has reported (registry empty), all
    // three live signals are false but the reading is UNKNOWN — it MUST read as
    // in-use so a heavy step picks cpu-idle / defers instead of the GPU on a
    // possibly-live wall.
    let unknown = WallActivity {
        any_playing: false,
        obs_streaming: false,
        obs_recording: false,
        known: false,
    };
    assert!(unknown.in_use(), "an unknown reading must read as in-use");
    assert_eq!(unknown.reason(), Some("startup grace (wall unknown)"));
}

#[test]
fn known_idle_reads_idle() {
    // Once the reading is KNOWN and every signal is idle, the wall is genuinely
    // idle — heavy work may run at full speed.
    let known_idle = WallActivity {
        any_playing: false,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    };
    assert!(!known_idle.in_use());
    assert_eq!(known_idle.reason(), None);
}

#[test]
fn known_playing_reads_in_use() {
    let known_playing = WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    };
    assert!(known_playing.in_use());
    assert_eq!(known_playing.reason(), Some("output playing"));
}

#[test]
fn activity_known_false_before_any_pipeline_reports_within_grace() {
    // 2 pipelines created, 0 reported, 5 s after start (< 30 s grace) → UNKNOWN.
    assert!(!activity_known(2, 0, Duration::from_secs(5)));
    // 2 created, 1 reported (not all) → still UNKNOWN.
    assert!(!activity_known(2, 1, Duration::from_secs(5)));
    // No pipelines created yet, within grace → UNKNOWN (a box that has not spun
    // up its outputs must still defer).
    assert!(!activity_known(0, 0, Duration::from_secs(5)));
}

#[test]
fn activity_known_true_when_every_pipeline_reported() {
    // 2 created, 2 reported → KNOWN even before the grace elapses.
    assert!(activity_known(2, 2, Duration::from_secs(1)));
    // More reported than expected (a stale count) still counts as all reported.
    assert!(activity_known(2, 3, Duration::from_secs(1)));
    // Boundary: exactly all reported.
    assert!(activity_known(1, 1, Duration::from_secs(0)));
}

#[test]
fn activity_known_true_once_grace_elapses_even_if_none_reported() {
    // The cap: a stuck heartbeat must not defer heavy work forever.
    assert!(activity_known(3, 0, STARTUP_GRACE)); // boundary: exactly at grace
    assert!(activity_known(3, 0, STARTUP_GRACE + Duration::from_secs(1)));
}

#[test]
fn startup_grace_defers_heavy_step() {
    // No heavy step for the first 60 s after engine start.
    assert!(startup_floor_defers(Duration::from_secs(0)));
    assert!(startup_floor_defers(Duration::from_secs(59)));
    // Boundary: exactly at the floor no longer defers.
    assert!(!startup_floor_defers(HEAVY_STEP_STARTUP_FLOOR));
    assert!(!startup_floor_defers(
        HEAVY_STEP_STARTUP_FLOOR + Duration::from_secs(1)
    ));
}
