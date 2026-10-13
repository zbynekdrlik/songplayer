//! #233: an ASIO output's status across the host's pipe is the status the
//! host read — every field, every fixed word, through the JSON the pipe
//! carries.

use super::*;
use crate::playback::asio_format::AsioSample;
use crate::playback::asio_loop::{LoopEvent, LoopEventKind, LoopState, LoopStatus};
use crate::playback::asio_out::{AsioSnapshot, AsioStatus, HardRecentre, LateCallback};
use crate::playback::asio_state::Reason;

/// A loop check with every field set (#233, the SoundGrid loop check).
fn full_loop() -> LoopStatus {
    let mismatch = LoopEvent {
        kind: LoopEventKind::Mismatch,
        utc_ms: 1_760_000_000_000,
        ago_s: 61.5,
        frames: 512,
    };
    let slip = LoopEvent {
        kind: LoopEventKind::Slip,
        utc_ms: 1_760_000_001_000,
        ago_s: 60.5,
        frames: -7,
    };
    LoopStatus {
        state: LoopState::Locked,
        inputs: [Some(2), Some(3)],
        delay_frames: Some(1_234),
        gain_db: [Some(-0.25), Some(-6.5)],
        checked_s: 120.5,
        blind_s: 3.25,
        mismatches: 1,
        dropouts: 2,
        slips: 3,
        overflows: 4,
        last_event: Some(slip),
        recent: vec![mismatch, slip],
    }
}

fn full() -> AsioSnapshot {
    AsioSnapshot {
        state: "waiting",
        reason: Some(Reason::NotFound {
            present: vec!["DVS".into(), "SG".into()],
        }),
        status: AsioStatus {
            driver: "Waves SoundGrid".into(),
            channels: [4, 5],
            driver_rate: 96_000,
            buffer_frames: 512,
            out_channels: 16,
            sample_type: "Int32LSB24",
            ppm: -12.5,
            rate_ppm: 3.25,
            locked: true,
            latency_ms: 71.125,
            offset_ms: -2.5,
            slew_eta_s: Some(45.5),
            cushion_ms: 13.25,
            underruns: 1,
            resets: 2,
            hard_recentres: 3,
            last_hard_recentre: Some(HardRecentre {
                cause: "excess",
                ms: -66.5,
                lateness_ms: 0.75,
                ago_s: 12.5,
            }),
            overflows: 4,
            overloads: 5,
            retry_in_s: Some(9.5),
            reason_code: Some("not_found"),
            clock_waits: 6,
            resyncs: 7,
            callback_gap_max_ms: 21.5,
            late_callbacks: 8,
            fast_callbacks: 9,
            last_late_callback: Some(LateCallback {
                gap_ms: 21.5,
                ago_s: 3.5,
            }),
            loop_status: full_loop(),
        },
        blocks_sent: 10,
        blocks_dropped: 11,
    }
}

fn through_json(s: &AsioSnapshot) -> AsioSnapshot {
    let json = serde_json::to_string(&WireSnapshot::of(s, None)).unwrap();
    serde_json::from_str::<WireSnapshot>(&json)
        .unwrap()
        .into_snapshot()
}

#[test]
fn a_full_snapshot_crosses_the_pipe_unchanged() {
    assert_eq!(through_json(&full()), full());
}

/// The host worker's start error crosses with the snapshot (SongPlayer
/// rebuilds the output on it).
#[test]
fn the_start_error_crosses_the_pipe() {
    let wire = WireSnapshot::of(&full(), Some("the ASIO thread stopped: it panicked".into()));
    let json = serde_json::to_string(&wire).unwrap();
    let back: WireSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.start_error.as_deref(),
        Some("the ASIO thread stopped: it panicked")
    );
    assert_eq!(back.into_snapshot(), full());
    let none: WireSnapshot =
        serde_json::from_str(&serde_json::to_string(&WireSnapshot::of(&full(), None)).unwrap())
            .unwrap();
    assert_eq!(none.start_error, None);
}

/// Every state, sample type and cause reads back as itself.
#[test]
fn every_fixed_word_reads_back_as_itself() {
    for state in ["running", "opening", "waiting", "disabled"] {
        let s = AsioSnapshot { state, ..full() };
        assert_eq!(through_json(&s).state, state);
    }
    for sample in [
        AsioSample::Int16,
        AsioSample::Int24,
        AsioSample::Int32,
        AsioSample::Float32,
        AsioSample::Int32In16,
        AsioSample::Int32In18,
        AsioSample::Int32In20,
        AsioSample::Int32In24,
    ] {
        let mut s = full();
        s.status.sample_type = sample.name();
        assert_eq!(through_json(&s).status.sample_type, sample.name());
    }
    for cause in ["prime", "deficit", "excess"] {
        let mut s = full();
        s.status.last_hard_recentre = Some(HardRecentre {
            cause,
            ..full().status.last_hard_recentre.unwrap()
        });
        let back = through_json(&s).status.last_hard_recentre.unwrap();
        assert_eq!(back.cause, cause);
    }
}

/// The reason's code is the decoded reason's own.
#[test]
fn the_reason_code_is_the_reasons() {
    let mut s = full();
    s.reason = Some(Reason::HostDown("x".into()));
    s.status.reason_code = Some("host_down");
    assert_eq!(through_json(&s), s);
    s.reason = None;
    s.status.reason_code = None;
    s.state = "running";
    assert_eq!(through_json(&s), s);
}

#[test]
fn an_unknown_word_reads_as_the_safe_one() {
    assert_eq!(known_state("later"), "waiting");
    assert_eq!(known_state("running"), "running");
    assert_eq!(known_sample("Int64LSB"), "");
    assert_eq!(known_sample("Float32LSB"), "Float32LSB");
    assert_eq!(WireCause::of("later").word(), "deficit");
}

/// JSON has no NaN: a non-finite figure goes over as 0, so the frame is
/// never refused.
#[test]
fn a_non_finite_figure_goes_over_as_zero() {
    assert_eq!(finite(f64::NAN), 0.0);
    assert_eq!(finite(f64::INFINITY), 0.0);
    assert_eq!(finite(f64::NEG_INFINITY), 0.0);
    assert_eq!(finite(-2.5), -2.5);
    let mut s = full();
    s.status.ppm = f64::NAN;
    s.status.slew_eta_s = Some(f64::INFINITY);
    s.status.last_late_callback = Some(LateCallback {
        gap_ms: f64::NAN,
        ago_s: 1.0,
    });
    let back = through_json(&s);
    assert_eq!(back.status.ppm, 0.0);
    assert_eq!(back.status.slew_eta_s, Some(0.0));
    assert_eq!(
        back.status.last_late_callback,
        Some(LateCallback {
            gap_ms: 0.0,
            ago_s: 1.0
        })
    );
}

/// #233 (the SoundGrid loop check): every state and event kind crosses the
/// pipe as itself, every figure finite, and a host that predates the loop
/// check (no field) reads off.
#[test]
fn the_loop_check_crosses_the_pipe() {
    for state in [
        LoopState::Off,
        LoopState::Searching,
        LoopState::Locked,
        LoopState::NoReturn,
    ] {
        let mut s = full();
        s.status.loop_status.state = state;
        assert_eq!(through_json(&s).status.loop_status.state, state);
    }
    for kind in [
        LoopEventKind::Mismatch,
        LoopEventKind::Dropout,
        LoopEventKind::Slip,
    ] {
        let mut s = full();
        s.status.loop_status.recent[0].kind = kind;
        assert_eq!(through_json(&s).status.loop_status.recent[0].kind, kind);
    }
    let mut s = full();
    s.status.loop_status.gain_db = [Some(f64::NAN), Some(f64::NEG_INFINITY)];
    s.status.loop_status.checked_s = f64::INFINITY;
    s.status.loop_status.blind_s = f64::NAN;
    s.status.loop_status.recent[1].ago_s = f64::NAN;
    s.status.loop_status.last_event.as_mut().unwrap().ago_s = f64::INFINITY;
    let back = through_json(&s).status.loop_status;
    assert_eq!(back.gain_db, [Some(0.0), Some(0.0)]);
    assert_eq!((back.checked_s, back.blind_s), (0.0, 0.0));
    assert_eq!(back.recent[1].ago_s, 0.0);
    assert_eq!(back.recent[0], full_loop().recent[0], "a finite one stays");
    assert_eq!(back.last_event.unwrap().ago_s, 0.0);
    assert_eq!(
        (back.delay_frames, back.inputs, back.slips),
        (Some(1_234), [Some(2), Some(3)], 3),
        "the rest crosses unchanged"
    );
    // The JSON names the status shows.
    let json = serde_json::to_string(&full().status).unwrap();
    assert!(
        json.contains(r#""loop":{"state":"locked","inputs":[2,3],"delay_frames":1234"#),
        "{json}"
    );
    assert!(json.contains(r#""kind":"slip""#), "{json}");
    // A host that predates it sends no `loop_status`: off.
    let mut old = WireSnapshot::of(&full(), None);
    old.status.loop_status = LoopStatus::default();
    let off = serde_json::to_string(&LoopStatus::default()).unwrap();
    let text = serde_json::to_string(&old)
        .unwrap()
        .replace(&format!(",\"loop_status\":{off}"), "");
    assert!(!text.contains("loop_status"), "{text}");
    let back: WireSnapshot = serde_json::from_str(&text).unwrap();
    assert_eq!(back.status.loop_status, LoopStatus::default());
}
