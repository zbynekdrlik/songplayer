//! Unit tests for the per-pipeline lock-state event window (#149, Lane 1) plus
//! the `format_genlock_line` structured-log helper and a structural guard that
//! the periodic INFO path renders it. RED-first: references
//! `crate::playback::lock_state::EventWindow`, the two new
//! `PipelineHealthSnapshot` fields, and `ndi_health::format_genlock_line`, none
//! of which exist until the GREEN commit — a documented compile-failure RED.

use crate::playback::lock_state::{EVENT_WINDOW_CAP, EventWindow};

/// 100-ns units per second — the sample-timestamp unit (mirrors sp-core).
const U: i64 = sp_core::genlock::UNITS_PER_SECOND;

#[test]
fn empty_window_reports_zero() {
    let w = EventWindow::new();
    assert_eq!(w.counts_in_window(0, 60 * U), (0, 0, 0));
    assert!(w.is_empty());
    assert_eq!(w.len(), 0);
}

#[test]
fn cold_start_diffs_against_first_sample() {
    // Only 5 s of history (< 60 s) → baseline is the FIRST sample so the single
    // late event since startup is not missed.
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(5 * U, 1, 0, 0);
    assert_eq!(w.counts_in_window(5 * U, 60 * U), (1, 0, 0));
}

#[test]
fn counts_difference_over_full_window() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(20 * U, 0, 0, 0);
    w.push(40 * U, 2, 1, 0);
    w.push(60 * U, 2, 1, 3);
    // window [0, 60]: baseline = oldest sample not older than 60 s = t0 (0,0,0);
    // newest = (2,1,3).
    assert_eq!(w.counts_in_window(60 * U, 60 * U), (2, 1, 3));
}

#[test]
fn aging_slides_baseline_forward() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(20 * U, 0, 0, 0);
    w.push(40 * U, 2, 1, 0);
    w.push(60 * U, 2, 1, 3);
    // now=80 s, window [20, 80]: baseline = t20 (0,0,0) → events still counted.
    w.push(80 * U, 2, 1, 3);
    assert_eq!(w.counts_in_window(80 * U, 60 * U), (2, 1, 3));
    // now=105 s, window [45, 105]: baseline = t60 (2,1,3) → all events aged out.
    w.push(105 * U, 2, 1, 3);
    assert_eq!(w.counts_in_window(105 * U, 60 * U), (0, 0, 0));
}

#[test]
fn boundary_sample_is_inclusive_at_exactly_60s() {
    // A sample exactly 60 s old is "not older than 60 s" → it is the inclusive
    // baseline; one tick later it leaves the window and the baseline advances.
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0);
    w.push(30 * U, 3, 0, 0);
    w.push(60 * U, 3, 0, 0);
    assert_eq!(w.counts_in_window(60 * U, 60 * U), (3, 0, 0));
    assert_eq!(w.counts_in_window(60 * U + 1, 60 * U), (0, 0, 0));
}

#[test]
fn counter_reset_after_anchor_restarts_window() {
    // The pacer zeroes its cumulative counters on anchor() (play/seek/new song);
    // a cumulative DECREASE is a reset → the ring restarts from that sample.
    let mut w = EventWindow::new();
    w.push(0, 5, 2, 1);
    w.push(5 * U, 6, 2, 1);
    w.push(10 * U, 0, 0, 0); // decrease → reset
    assert_eq!(w.counts_in_window(10 * U, 60 * U), (0, 0, 0));
    w.push(15 * U, 1, 0, 0); // a late after the reset
    assert_eq!(w.counts_in_window(15 * U, 60 * U), (1, 0, 0));
}

#[test]
fn memory_is_bounded() {
    let mut w = EventWindow::new();
    for i in 0..200 {
        w.push(i * 5 * U, 0, 0, 0);
    }
    assert!(w.len() <= EVENT_WINDOW_CAP, "ring must stay bounded");
    assert!(!w.is_empty());
}

// ---- format_genlock_line + structural guard ----

fn sample_snapshot() -> crate::playback::ndi_health::PipelineHealthSnapshot {
    use crate::playback::clock_health::{DantesyncStatus, evaluate};
    use crate::playback::ndi_health::{AudioStats, PacingStats, PlaybackStateLabel};
    use sp_core::genlock::lock_state::LockState;

    let clock = evaluate(Some(&DantesyncStatus {
        is_locked: Some(true),
        mode: Some("NANO".to_string()),
        offset_ns: None,
        ntp_failed: None,
        ntp_age_s: None,
    }));
    crate::playback::ndi_health::PipelineHealthSnapshot {
        playlist_id: 7,
        ndi_name: "SP-fast".to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 2,
        frames_submitted_total: 100,
        frames_submitted_last_5s: 30,
        observed_fps: 30.0,
        nominal_fps: 30.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock,
        pacing: PacingStats {
            enabled: true,
            seq: 42,
            late_frames: 3,
            max_late_us: 900,
            jitter_p99_us: 120,
            repeats: 5,
            resyncs: 1,
            relatches: 2,
            dropped: 7,
            lag_slots: 4,
            iter_p99_us: 4200,
            prep_p99_us: 3800,
            ..Default::default()
        },
        audio: AudioStats {
            enabled: true,
            residual_ppm: -12.5,
            applied_ppm: 8.0,
            samples_per_boundary: 1600,
            underruns: 9,
            overflows: 0,
            buffer_ms: 66,
            emitter: Default::default(),
        },
        lock_state: LockState::Degraded,
        lock_reason: "late/repeats/resyncs in 60 s".to_string(),
        burn_on: false,
        recovery_step: None,
        sender_url: None,
        transport: sp_core::playback::TransportState::Idle,
    }
}

#[test]
fn format_genlock_line_contains_every_key_token() {
    let line = crate::playback::ndi_health::format_genlock_line(&sample_snapshot());
    assert!(
        line.starts_with("ndi: genlock "),
        "line must start with the grep anchor: {line}"
    );
    for tok in [
        "playlist_id=7",
        "ndi_name=SP-fast",
        "seq=42",
        "late=3",
        "p99_us=120",
        "repeats=5",
        "resyncs=1",
        "relatches=2",
        "lag=4",
        "audio_ppm=-12.5",
        "underruns=9",
        "clock_ok=true",
        "lock=DEGRADED",
        "reason=\"late/repeats/resyncs in 60 s\"",
    ] {
        assert!(line.contains(tok), "missing token `{tok}` in line: {line}");
    }
}

/// Structural guard: the once-per-UTC-minute periodic INFO path in
/// `ndi_health.rs` MUST render the genlock line via `format_genlock_line`,
/// gated by `should_log_periodic_heartbeat`. Fires red if a refactor drops the
/// call from that path.
#[test]
fn periodic_log_path_calls_format_genlock_line() {
    let src = include_str!("ndi_health.rs").replace("\r\n", "\n");
    let guard = src
        .find("should_log_periodic_heartbeat(prev_heartbeat_ts, cur)")
        .expect("periodic heartbeat guard must exist");
    let call = src
        .find("format_genlock_line(&snapshot)")
        .expect("periodic path must render the genlock line");
    assert!(
        call > guard,
        "the genlock line must be emitted inside the periodic-heartbeat guard"
    );
}
