//! Unit tests for the #154 idle gate pure decision core.
//!
//! RED-first: these assert the intended behaviour against the stubbed module
//! (which returns the wrong constants), so they FAIL until the GREEN commit
//! fills in the logic. No I/O — the live-handle seam (`impl LyricsWorker`) is
//! exercised by the worker-loop test in `worker_tests.rs`.

use super::*;
use crate::playback::ndi_health::PlaybackStateLabel;

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
    };
    assert!(!should_defer(false, busy));
}

#[test]
fn should_defer_false_when_enabled_and_idle() {
    assert!(!should_defer(true, WallActivity::default()));
}

// ---- gate_setting_enabled ------------------------------------------------

#[test]
fn gate_setting_default_on_when_unset() {
    assert!(gate_setting_enabled(None));
}

#[test]
fn gate_setting_off_for_falsey_tokens() {
    for v in ["false", "0", "off", "no", "FALSE", " Off "] {
        assert!(!gate_setting_enabled(Some(v)), "expected {v:?} to disable");
    }
}

#[test]
fn gate_setting_on_for_truthy_tokens() {
    for v in ["true", "1", "on", "yes", ""] {
        assert!(gate_setting_enabled(Some(v)), "expected {v:?} to enable");
    }
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
