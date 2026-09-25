//! Unit tests for the per-pipeline lock-state event window (#149, Lane 1) plus
//! the `format_genlock_line` structured-log helper and a structural guard that
//! the periodic INFO path renders it. RED-first: references
//! `crate::playback::lock_state::EventWindow`, the two new
//! `PipelineHealthSnapshot` fields, and `ndi_health::format_genlock_line`, none
//! of which exist until the GREEN commit — a documented compile-failure RED.

use crate::playback::lock_state::{EVENT_WINDOW_CAP, EventWindow, lock_for_heartbeat};
use sp_core::playback::TransportState::{Idle, Paused, Playing};

/// 100-ns units per second — the sample-timestamp unit (mirrors sp-core).
const U: i64 = sp_core::genlock::UNITS_PER_SECOND;

// `push` / `counts_in_window` carry the cumulative `seq` (boundaries serviced)
// as the rate base (#168 round 6): `push(ts, seq, late, repeats, resyncs)`,
// `counts_in_window -> (slots, late, repeats, resyncs)`.

#[test]
fn empty_window_reports_zero() {
    let w = EventWindow::new();
    assert_eq!(w.counts_in_window(0, 60 * U), (0, 0, 0, 0));
    assert!(w.is_empty());
    assert_eq!(w.len(), 0);
}

#[test]
fn cold_start_diffs_against_first_sample() {
    // Only 5 s of history (< 60 s) → baseline is the FIRST sample so the 150
    // slots + single late since startup are not missed.
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0, 0);
    w.push(5 * U, 150, 1, 0, 0);
    assert_eq!(w.counts_in_window(5 * U, 60 * U), (150, 1, 0, 0));
}

#[test]
fn counts_difference_over_full_window() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0, 0);
    w.push(20 * U, 600, 0, 0, 0);
    w.push(40 * U, 1200, 2, 1, 0);
    w.push(60 * U, 1800, 2, 1, 3);
    // window [0, 60]: baseline = oldest sample not older than 60 s = t0; newest
    // seq 1800 → slots 1800, events (2,1,3).
    assert_eq!(w.counts_in_window(60 * U, 60 * U), (1800, 2, 1, 3));
}

#[test]
fn slots_difference_over_full_window() {
    // A clean minute: 1800 slots emitted, no events.
    let mut w = EventWindow::new();
    w.push(0, 1000, 0, 0, 0);
    w.push(60 * U, 2800, 0, 0, 0);
    assert_eq!(w.counts_in_window(60 * U, 60 * U), (1800, 0, 0, 0));
}

#[test]
fn aging_slides_baseline_forward() {
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0, 0);
    w.push(20 * U, 600, 0, 0, 0);
    w.push(40 * U, 1200, 2, 1, 0);
    w.push(60 * U, 1800, 2, 1, 3);
    // now=80 s, window [20, 80]: baseline = t20 (seq600) → slots 2400-600=1800,
    // events still counted.
    w.push(80 * U, 2400, 2, 1, 3);
    assert_eq!(w.counts_in_window(80 * U, 60 * U), (1800, 2, 1, 3));
    // now=105 s, window [45, 105]: baseline = t60 (seq1800,2,1,3) → slots
    // 2550-1800=750, all events aged out.
    w.push(105 * U, 2550, 2, 1, 3);
    assert_eq!(w.counts_in_window(105 * U, 60 * U), (750, 0, 0, 0));
}

#[test]
fn boundary_sample_is_inclusive_at_exactly_60s() {
    // A sample exactly 60 s old is "not older than 60 s" → it is the inclusive
    // baseline; one tick later it leaves the window and the baseline advances.
    let mut w = EventWindow::new();
    w.push(0, 0, 0, 0, 0);
    w.push(30 * U, 900, 3, 0, 0);
    w.push(60 * U, 1800, 3, 0, 0);
    assert_eq!(w.counts_in_window(60 * U, 60 * U), (1800, 3, 0, 0));
    assert_eq!(w.counts_in_window(60 * U + 1, 60 * U), (900, 0, 0, 0));
}

#[test]
fn counter_reset_after_anchor_restarts_window() {
    // The pacer zeroes its cumulative counters on anchor() (play/seek/new song);
    // a cumulative DECREASE is a reset → the ring restarts from that sample.
    let mut w = EventWindow::new();
    w.push(0, 100, 5, 2, 1);
    w.push(5 * U, 130, 6, 2, 1);
    w.push(10 * U, 0, 0, 0, 0); // seq + late decrease → reset
    assert_eq!(w.counts_in_window(10 * U, 60 * U), (0, 0, 0, 0));
    w.push(15 * U, 150, 1, 0, 0); // slots + a late after the reset
    assert_eq!(w.counts_in_window(15 * U, 60 * U), (150, 1, 0, 0));
}

#[test]
fn memory_is_bounded() {
    let mut w = EventWindow::new();
    for i in 0..200 {
        w.push(i * 5 * U, i as u64, 0, 0, 0);
    }
    assert!(w.len() <= EVENT_WINDOW_CAP, "ring must stay bounded");
    assert!(!w.is_empty());
}

// ---- lock_for_heartbeat: the engine seam (push → difference → derive) ----

fn paced(
    seq: u64,
    late: u64,
    repeats: u64,
    resyncs: u64,
) -> crate::playback::ndi_health::PacingStats {
    crate::playback::ndi_health::PacingStats {
        enabled: true,
        seq,
        late_frames: late,
        repeats,
        resyncs,
        ..Default::default()
    }
}

#[test]
fn lock_for_heartbeat_holds_locked_on_a_clean_grid() {
    use sp_core::genlock::lock_state::LockState;
    let mut w = EventWindow::new();
    // Two heartbeats 60 s apart: 1800 slots, 100 late (≈ 5.6 %), 360 structural
    // 24→30 repeats — a holding 24-fps grid → LOCKED.
    let _ = lock_for_heartbeat(&mut w, 0, &paced(0, 0, 0, 0), true, 2, 24.0, 30, Playing);
    let (s, r) = lock_for_heartbeat(
        &mut w,
        60 * U,
        &paced(1800, 100, 360, 0),
        true,
        2,
        24.0,
        30,
        Playing,
    );
    assert_eq!(s, LockState::Locked);
    assert_eq!(r, "locked");
}

#[test]
fn lock_for_heartbeat_degrades_on_a_stall() {
    use sp_core::genlock::lock_state::LockState;
    let mut w = EventWindow::new();
    // 750 late / 1800 slots ≈ 42 % → the sender-side stall → DEGRADED (late).
    let _ = lock_for_heartbeat(&mut w, 0, &paced(0, 0, 0, 0), true, 2, 24.0, 30, Playing);
    let (s, r) = lock_for_heartbeat(
        &mut w,
        60 * U,
        &paced(1800, 750, 360, 0),
        true,
        2,
        24.0,
        30,
        Playing,
    );
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "late > 25 % of slots in 60 s");
}

// #168 round 6b regression pair — the EXACT box read (22.9.2026 17:56 UTC,
// SP-slow, a 23.976-fps NTSC-24 file on the 30-fps grid): slots 1803, late 3,
// repeats 362 (= 20.1 % ≈ the structural 1 − 24/30 conversion), resyncs 0, clock
// ok, pacing on, 2 receivers. With the DECODER's `source_fps` (23.976) the
// 20.1 % repeats are the by-design conversion → LOCKED; with the paced path's
// grid-valued `nominal_fps` (30.0) the rule expects 0 % → the same repeats
// falsely DEGRADE. Same counts, different `source_fps` → the whole fix.

#[test]
fn lock_for_heartbeat_source_fps_23976_holds_locked() {
    use sp_core::genlock::lock_state::LockState;
    let mut w = EventWindow::new();
    let _ = lock_for_heartbeat(&mut w, 0, &paced(0, 0, 0, 0), true, 2, 23.976, 30, Playing);
    let (s, r) = lock_for_heartbeat(
        &mut w,
        60 * U,
        &paced(1803, 3, 362, 0),
        true,
        2,
        23.976,
        30,
        Playing,
    );
    assert_eq!(
        s,
        LockState::Locked,
        "24-fps structural repeats must stay LOCKED"
    );
    assert_eq!(r, "locked");
}

#[test]
fn lock_for_heartbeat_grid_source_fps_falsely_degrades() {
    use sp_core::genlock::lock_state::LockState;
    let mut w = EventWindow::new();
    // Feeding the grid rate (30.0, what the paced `nominal_fps` reads) as the
    // source is the BUG: expected repeat 0 % → the 20.1 % structural repeats trip
    // the margin → DEGRADED. This pins WHY `source_fps` must be the decoder rate.
    let _ = lock_for_heartbeat(&mut w, 0, &paced(0, 0, 0, 0), true, 2, 30.0, 30, Playing);
    let (s, r) = lock_for_heartbeat(
        &mut w,
        60 * U,
        &paced(1803, 3, 362, 0),
        true,
        2,
        30.0,
        30,
        Playing,
    );
    assert_eq!(s, LockState::Degraded);
    assert_eq!(r, "repeats above the fps conversion in 60 s");
}

// #150: the engine seam maps the RAW transport to `decoding` — only `Playing`
// decodes. The SAME standby window (1800 slots, 1800 frozen-last-frame repeats,
// a 23.976-fps file, the box read of Paused SP-fast / SP-dabing 24.9.2026)
// reads LOCKED while Paused or Idle and DEGRADED while Playing.

fn standby_minute(
    transport: sp_core::playback::TransportState,
) -> (sp_core::genlock::lock_state::LockState, &'static str) {
    let mut w = EventWindow::new();
    let _ = lock_for_heartbeat(
        &mut w,
        0,
        &paced(0, 0, 0, 0),
        true,
        2,
        23.976,
        30,
        transport,
    );
    lock_for_heartbeat(
        &mut w,
        60 * U,
        &paced(1800, 2, 1800, 0),
        true,
        2,
        23.976,
        30,
        transport,
    )
}

#[test]
fn lock_for_heartbeat_paused_standby_repeats_hold_locked() {
    use sp_core::genlock::lock_state::LockState;
    assert_eq!(standby_minute(Paused), (LockState::Locked, "locked"));
}

#[test]
fn lock_for_heartbeat_idle_standby_repeats_hold_locked() {
    use sp_core::genlock::lock_state::LockState;
    assert_eq!(standby_minute(Idle), (LockState::Locked, "locked"));
}

#[test]
fn lock_for_heartbeat_playing_repeats_on_every_slot_degrade() {
    use sp_core::genlock::lock_state::LockState;
    assert_eq!(
        standby_minute(Playing),
        (
            LockState::Degraded,
            "repeats above the fps conversion in 60 s"
        )
    );
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
        source_fps: 30.0,
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
            av_align_err_ms: -12.5,
            av_corrections: 3,
            av_corrected_samples: 144,
            wall_anchor_max_step_us: 40_012,
            wall_anchor_wide_brackets: 2,
            wall_anchor_slewed_us: 3_000,
            ..Default::default()
        },
        audio: AudioStats {
            enabled: true,
            samples_per_boundary: 1600,
            underruns: 9,
            overflows: 0,
            buffer_ms: 66,
            emitter: Default::default(),
        },
        lock_state: LockState::Degraded,
        lock_reason: "late > 25 % of slots in 60 s".to_string(),
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
        "av_align_err_ms=-12.5",
        "av_corrections=3",
        "av_corrected_samples=144",
        // #147: the pacer wall's anchor telemetry.
        "wall_anchor_max_step_us=40012",
        "wall_anchor_wide_brackets=2",
        "wall_anchor_slewed_us=3000",
        "underruns=9",
        "clock_ok=true",
        "lock=DEGRADED",
        "reason=\"late > 25 % of slots in 60 s\"",
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
