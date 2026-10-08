//! #233: the ASIO output's decisions — the reopen backoff (2 / 10 / 30 / 60 s,
//! reset by a 60 s run), what closes an output, the asioMessage replies
//! (iemmixer telemetry.rs), the 2 s stall, the rate admission and its note,
//! the driver buffer's note, the ring size, the latency.

use super::*;

#[test]
fn the_backoff_is_2_10_30_then_every_60_s() {
    let s: Vec<i64> = (1..=6).map(|f| backoff_100ns(f) / 10_000_000).collect();
    assert_eq!(s, vec![2, 10, 30, 60, 60, 60]);
    assert_eq!(
        backoff_100ns(0),
        20_000_000,
        "never less than the start backoff"
    );
    assert_eq!(backoff_100ns(u32::MAX), 600_000_000);
    assert_eq!(BACKOFF_S, [2, 10, 30, 60]);
}

#[test]
fn a_60_s_run_resets_the_backoff() {
    assert_eq!(STABLE_RUN_100NS, 600_000_000);
    assert_eq!(failures_after_close(3, STABLE_RUN_100NS), 1);
    assert_eq!(failures_after_close(3, STABLE_RUN_100NS - 1), 4);
    assert_eq!(failures_after_close(0, 0), 1);
    assert_eq!(failures_after_close(u32::MAX, 0), u32::MAX, "never wraps");
}

#[test]
fn a_reset_a_size_change_or_a_new_rate_closes_the_output() {
    let base = DeviceEvents::default();
    assert_eq!(close_reason(&base, 96_000.0), None);
    assert_eq!(
        close_reason(
            &DeviceEvents {
                reset: true,
                ..base
            },
            96_000.0
        ),
        Some(Reason::Reset)
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                buffer_size_change: true,
                ..base
            },
            96_000.0
        ),
        Some(Reason::Reset)
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                rate_changed: Some(48_000.0),
                ..base
            },
            96_000.0
        ),
        Some(Reason::RateChanged(48_000))
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                rate_changed: Some(96_000.5),
                ..base
            },
            96_000.0
        ),
        None,
        "under 1 Hz"
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                rate_changed: Some(96_001.0),
                ..base
            },
            96_000.0
        ),
        Some(Reason::RateChanged(96_001))
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                rate_changed: Some(95_999.0),
                ..base
            },
            96_000.0
        ),
        Some(Reason::RateChanged(95_999)),
        "1 Hz down"
    );
    assert_eq!(
        close_reason(
            &DeviceEvents {
                resync: true,
                latencies_changed: true,
                overloads: 3,
                callbacks: 9,
                ..base
            },
            96_000.0
        ),
        None
    );
}

#[test]
fn a_zero_rate_is_a_lost_clock() {
    let lost = DeviceEvents {
        rate_changed: Some(0.0),
        ..DeviceEvents::default()
    };
    assert_eq!(close_reason(&lost, 96_000.0), Some(Reason::RateChanged(0)));
    assert_eq!(
        Reason::RateChanged(0).text(),
        "the driver lost its clock (it reports 0 Hz)"
    );
}

#[test]
fn no_callback_for_2_s_is_a_stall() {
    assert_eq!(STALL_100NS, 20_000_000);
    let mut w = StallWatch::default();
    assert!(!w.stalled(10, 0));
    assert!(!w.stalled(10, STALL_100NS - 1));
    assert!(w.stalled(10, STALL_100NS));
    assert!(!w.stalled(11, STALL_100NS + 5), "a callback resets it");
    assert!(!w.stalled(11, 2 * STALL_100NS + 4));
    assert!(w.stalled(11, 2 * STALL_100NS + 5));
}

#[test]
fn the_message_replies_follow_iemmixer() {
    use selector::*;
    assert_eq!(
        [
            SELECTOR_SUPPORTED,
            ENGINE_VERSION,
            RESET_REQUEST,
            BUFFER_SIZE_CHANGE,
            RESYNC_REQUEST,
            LATENCIES_CHANGED,
            SUPPORTS_TIME_INFO,
            SUPPORTS_TIME_CODE,
            OVERLOAD
        ],
        [1, 2, 3, 4, 5, 6, 7, 8, 15],
        "asio.h kAsio… (azo-sys MessageSelector)"
    );
    for supported in [
        ENGINE_VERSION,
        RESET_REQUEST,
        BUFFER_SIZE_CHANGE,
        RESYNC_REQUEST,
        LATENCIES_CHANGED,
        SUPPORTS_TIME_INFO,
        OVERLOAD,
    ] {
        assert_eq!(reply(SELECTOR_SUPPORTED, supported), 1, "{supported}");
    }
    assert_eq!(reply(SELECTOR_SUPPORTED, SUPPORTS_TIME_CODE), 0);
    assert_eq!(reply(SELECTOR_SUPPORTED, 99), 0);
    assert_eq!(reply(ENGINE_VERSION, 0), 2);
    assert_eq!(reply(RESET_REQUEST, 0), 1);
    assert_eq!(reply(RESYNC_REQUEST, 0), 1);
    assert_eq!(reply(LATENCIES_CHANGED, 0), 1);
    assert_eq!(reply(SUPPORTS_TIME_INFO, 0), 1);
    assert_eq!(
        reply(BUFFER_SIZE_CHANGE, 256),
        0,
        "never resized live: the driver then asks a reset"
    );
    assert_eq!(reply(SUPPORTS_TIME_CODE, 0), 0);
    assert_eq!(reply(OVERLOAD, 0), 0);
    assert_eq!(reply(99, 0), 0);
}

#[test]
fn the_rate_is_admitted_and_noted() {
    assert_eq!(admit_rate(96_000.0), Ok(96_000));
    assert_eq!(admit_rate(44_100.4), Ok(44_100));
    assert_eq!(admit_rate(8_000.0), Ok(8_000));
    assert_eq!(admit_rate(384_000.0), Ok(384_000));
    for bad in [7_999.0, 384_001.0, f64::NAN, f64::INFINITY, 0.0, -48_000.0] {
        assert_eq!(
            admit_rate(bad),
            Err(Reason::Refused(format!("the driver reports {bad} Hz"))),
            "{bad}"
        );
    }
    assert_eq!(rate_note(96_000, 96_000), None);
    assert_eq!(
        rate_note(48_000, 96_000).as_deref(),
        Some("the driver runs at 48000 Hz, the network at 96000 Hz")
    );
}

#[test]
fn a_driver_buffer_over_a_third_of_a_slot_is_noted() {
    // Lane 2's envelope: ≤ 512 frames at 48 kHz, ≤ 1024 at 96 kHz.
    assert_eq!(buffer_note(512, 48_000), None);
    assert_eq!(buffer_note(1_024, 96_000), None);
    assert_eq!(buffer_note(490, 44_100), None, "exactly 1/90 s");
    assert!(buffer_note(491, 44_100).is_some());
    assert_eq!(
        buffer_note(2_048, 48_000).as_deref(),
        Some(
            "the driver's buffer of 2048 frames (42.7 ms) is over a third of a grid slot \
             (11.1 ms): the drift servo may re-centre often"
        )
    );
}

#[test]
fn the_ring_holds_the_target_four_slots_and_a_block() {
    // 66.67 ms + 4 × 33.33 ms = 200 ms at 96 kHz = 19_200 frames, + one block out (3_210)
    assert_eq!(
        ring_capacity_frames(96_000.0, 666_666, 3_210),
        19_200 + 3_210
    );
    assert_eq!(
        ring_capacity_frames(48_000.0, 1_666_666, 1_610),
        14_400 + 1_610
    );
}

#[test]
fn the_latency_adds_the_resampler_and_the_driver() {
    let ms = asio_latency_ms(66.7, 256, 128, 96_000.0);
    assert!((ms - (66.7 + 2.6667 + 1.3333)).abs() < 1e-3, "{ms}");
    let ms = asio_latency_ms(70.0, 128, 64, 48_000.0);
    assert!((ms - 74.0).abs() < 1e-9, "{ms}");
}

#[test]
fn every_reason_has_a_code_and_a_text() {
    let reasons = [
        (
            Reason::NotFound {
                present: vec!["Blackmagic ASIO".into(), "ASIO4ALL v2".into()],
            },
            "not_found",
            "the driver is not registered (present: Blackmagic ASIO, ASIO4ALL v2)",
        ),
        (
            Reason::NotFound { present: vec![] },
            "not_found",
            "the driver is not registered (present: none)",
        ),
        (
            Reason::Busy("init failed".into()),
            "busy",
            "the driver refused to start (in use by another program?): init failed",
        ),
        (Reason::Refused("x".into()), "refused", "x"),
        (Reason::Failed("y".into()), "failed", "y"),
        (Reason::Reset, "reset", "the driver asked for a reset"),
        (
            Reason::RateChanged(48_000),
            "rate_changed",
            "the driver's rate changed to 48000 Hz",
        ),
        (
            Reason::Stalled,
            "stalled",
            "no callback from the driver for 2 s",
        ),
        (
            Reason::WindowsOnly,
            "windows_only",
            "ASIO runs on Windows only",
        ),
    ];
    for (r, code, text) in reasons {
        assert_eq!((r.code(), r.text().as_str()), (code, text));
    }
}

/// The dashboard reads a waiting ASIO output's reason by its code
/// (`sp_core::audio_outputs::asio_reason_sk`): every code has its Slovak.
#[test]
fn every_reason_code_has_its_slovak_on_the_dashboard() {
    let reasons = [
        Reason::NotFound { present: vec![] },
        Reason::Busy(String::new()),
        Reason::Refused(String::new()),
        Reason::Failed(String::new()),
        Reason::Reset,
        Reason::RateChanged(0),
        Reason::Stalled,
        Reason::WindowsOnly,
    ];
    for r in reasons {
        assert_ne!(
            sp_core::audio_outputs::asio_reason_sk(r.code()),
            "neznámy dôvod",
            "{r:?}"
        );
    }
}

/// #233 review round 2: a lost clock and a driver another output still
/// holds have their own codes, so the dashboard says what happened (not
/// "the rate changed" / "another program uses it").
#[test]
fn a_lost_clock_and_a_held_driver_have_their_own_codes() {
    let lost = Reason::RateChanged(0);
    assert_eq!(
        (lost.code(), lost.text().as_str()),
        ("clock_lost", "the driver lost its clock (it reports 0 Hz)")
    );
    assert_eq!(Reason::RateChanged(1).code(), "rate_changed");
    assert_eq!(
        (Reason::Held.code(), Reason::Held.text().as_str()),
        (
            "held",
            "another SongPlayer output still holds the driver (the output this one replaces is releasing it)"
        )
    );
    for r in [lost, Reason::Held] {
        assert_ne!(
            sp_core::audio_outputs::asio_reason_sk(r.code()),
            "neznámy dôvod",
            "{r:?}"
        );
    }
}
