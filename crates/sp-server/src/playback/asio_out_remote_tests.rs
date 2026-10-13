//! #233 (the spawn finding): an ASIO output's two sides — the spec that
//! builds both, the host's report applied on SongPlayer's side, and the
//! host-down state.

use std::time::Duration;

use super::*;
use crate::playback::asio_out::{AsioOut, AsioSnapshot, AsioStatus};
use crate::playback::asio_state::Reason;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_queue::Take;

fn spec() -> AsioSpec {
    AsioSpec {
        id: "out-2".into(),
        driver: "Waves SoundGrid".into(),
        channels: [3, 4],
        delay_ms: 250,
        loop_check: true,
    }
}

/// The spec an output was built from is the spec it gives back (the host
/// builds its side from it).
#[test]
fn an_output_gives_back_the_spec_it_was_built_from() {
    let out = AsioOut::from_spec(&spec());
    assert_eq!(out.spec(), spec());
    assert_eq!(out.id(), "out-2");
    // Its delay adds to the servo's target: two slots + 250 ms.
    assert_eq!(out.target_100ns(), 666_666 + 2_500_000);
    let zero = AsioOut::from_spec(&AsioSpec {
        delay_ms: 0,
        ..spec()
    });
    assert_eq!(zero.spec().delay_ms, 0);
}

fn running_status() -> AsioStatus {
    AsioStatus {
        driver: "Waves SoundGrid".into(),
        channels: [3, 4],
        driver_rate: 96_000,
        buffer_frames: 512,
        out_channels: 8,
        sample_type: "Int32LSB",
        ppm: 12.5,
        rate_ppm: 11.0,
        locked: true,
        latency_ms: 71.2,
        offset_ms: 1.5,
        slew_eta_s: Some(3.0),
        cushion_ms: 4.0,
        underruns: 3,
        resyncs: 7,
        ..AsioStatus::default()
    }
}

/// SongPlayer's side shows the host's report; the host's queue drops add
/// to its own queue's and to the blocks its link did not take.
#[test]
fn the_hosts_report_is_the_outputs_status_with_both_sides_drops() {
    let out = AsioOut::from_spec(&spec());
    // The queue's bound at a 250 ms delay is 10 + 8 blocks: 20 pushes drop 2.
    for i in 0..20 {
        out.push(ProgramBlock::silence(i));
    }
    assert_eq!(out.snapshot().blocks_dropped, 2);
    // Two the host's link did not take, counted on the output too.
    assert_eq!(out.note_unsent(), 1);
    assert_eq!(out.note_unsent(), 2);
    assert_eq!(out.snapshot().blocks_dropped, 4);
    out.apply_remote(AsioSnapshot {
        state: "running",
        reason: None,
        status: running_status(),
        blocks_sent: 41,
        blocks_dropped: 5,
    });
    let s = out.snapshot();
    assert_eq!(
        (s.state, s.reason, s.blocks_sent, s.blocks_dropped),
        ("running", None, 41, 9)
    );
    assert_eq!(s.status, running_status());
}

/// A host that is down: waiting, `host_down`, no next try shown, the run's
/// figures cleared and the counters kept.
#[test]
fn a_host_that_is_down_reads_waiting_with_its_reason() {
    let out = AsioOut::from_spec(&spec());
    out.apply_remote(AsioSnapshot {
        state: "running",
        reason: None,
        status: AsioStatus {
            retry_in_s: Some(9.0),
            ..running_status()
        },
        blocks_sent: 41,
        blocks_dropped: 0,
    });
    out.set_host_down("it exited");
    let s = out.snapshot();
    assert_eq!(s.state, "waiting");
    assert_eq!(s.reason, Some(Reason::HostDown("it exited".into())));
    assert_eq!(s.status.reason_code, Some("host_down"));
    assert_eq!(s.status.retry_in_s, None);
    assert_eq!(
        (
            s.status.ppm,
            s.status.rate_ppm,
            s.status.locked,
            s.status.latency_ms,
            s.status.offset_ms,
            s.status.slew_eta_s,
            s.status.cushion_ms
        ),
        (0.0, 0.0, false, 0.0, 0.0, None, 0.0)
    );
    assert_eq!(
        (
            s.status.underruns,
            s.status.resyncs,
            s.status.driver_rate,
            s.blocks_sent
        ),
        (3, 7, 96_000, 41)
    );
}

/// The forwarder's side: the queue's blocks in order, then the stop; its
/// guard marks the output running while it is held.
#[test]
fn the_forwarder_takes_the_queue_in_order_and_holds_the_running_flag() {
    let out = AsioOut::from_spec(&spec());
    out.push(ProgramBlock::silence(1));
    out.push(ProgramBlock::silence(2));
    {
        let _running = out.run_guard();
        assert!(out.is_running());
        assert_eq!(
            out.take_block(Duration::from_millis(1)),
            Take::Block(ProgramBlock::silence(1))
        );
        assert_eq!(
            out.take_block(Duration::from_millis(1)),
            Take::Block(ProgramBlock::silence(2))
        );
        assert_eq!(out.take_block(Duration::from_millis(1)), Take::Idle);
        out.stop();
        assert_eq!(out.take_block(Duration::from_millis(1)), Take::Stopped);
    }
    assert!(!out.is_running());
}
