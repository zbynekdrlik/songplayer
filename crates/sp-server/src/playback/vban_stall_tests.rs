//! #210 part 2: the VBAN thread's per-packet lateness window, exact pins
//! (derived with a scratch Python model), the path from
//! `VbanSender::send_block` to `VbanOut::status`, and the clocks' labels.
//! Wired via `#[cfg(test)] #[path = "vban_stall_tests.rs"] mod tests;`.

use std::sync::Arc;

use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::fleet_shift::FleetShift;
use crate::playback::vban_clock::WallVbanClock;
use crate::playback::vban_out::tests::{FakeClock, RecordingSink, active_config};
use crate::playback::vban_out::{VbanClock, VbanOut, VbanSender};
use crate::playback::vban_packet::{VBAN_PACKETS_PER_SECOND, VBAN_SEND_LATENCY_100NS};
use crate::playback::wallclock::{SystemClock, WallClock};
use sp_core::genlock::UNITS_PER_SECOND;

/// A packet's planned instant (100 ns, VBAN's timeline).
const P: i64 = 17_907_771_311_999_999;

/// Fold in a packet sent at `sent_100ns`, `late_us` after its planned
/// instant, whose send reading is its own label.
fn sent_late(log: &mut VbanStallLog, sent_100ns: i64, late_us: i64) -> Option<VbanStallWarn> {
    log.observe(sent_100ns - late_us * 10, sent_100ns, sent_100ns)
}

#[test]
fn the_limits_are_the_designed_ones() {
    assert_eq!(VBAN_STALL_EVENT_US, 5_000, "5 ms");
    assert_eq!(VBAN_STALL_WARN_US, 10_000, "10 ms");
    assert_eq!(VBAN_STALL_WARN_EVERY_100NS, 5 * UNITS_PER_SECOND, "5 s");
    assert_eq!(VBAN_STALL_RING, 32);
    assert_eq!(
        i64::from(VBAN_STALL_BUCKET_PACKETS),
        60 * VBAN_PACKETS_PER_SECOND,
        "60 s of packets"
    );
}

#[test]
fn a_packet_over_5_ms_late_is_an_event_at_its_utc_millisecond() {
    let mut log = VbanStallLog::default();
    // The send reading's label sits 10 s after it (a fleet shift), so
    // `utc_ms` must come off the label, not off the timeline.
    let shift = 100_000_000;
    let mut late_by = |late_100ns: i64| {
        let sent = P + late_100ns;
        log.observe(P, sent, sent + shift)
    };
    assert_eq!(late_by(-5), None, "sent before its planned instant: 0 late");
    assert_eq!(late_by(50_000), None, "exactly 5 ms");
    assert_eq!(late_by(50_010), None, "5.001 ms: an event, not a WARN");
    assert_eq!(
        late_by(100_000),
        None,
        "exactly 10 ms: an event, not a WARN"
    );
    assert_eq!(
        log.late_events(),
        vec![
            VbanLateEvent {
                utc_ms: 1_790_777_141_205,
                late_us: 5_001,
            },
            VbanLateEvent {
                utc_ms: 1_790_777_141_209,
                late_us: 10_000,
            },
        ]
    );
    assert_eq!(log.late_max_us(), 10_000);
}

#[test]
fn the_ring_keeps_the_last_32_events_oldest_first() {
    let mut log = VbanStallLog::default();
    for i in 0..33_i64 {
        let sent = P + i * UNITS_PER_SECOND;
        sent_late(&mut log, sent, 6_000 + i);
    }
    let events = log.late_events();
    assert_eq!(events.len(), 32, "the bound");
    assert_eq!(
        events[0],
        VbanLateEvent {
            utc_ms: 1_790_777_132_199,
            late_us: 6_001,
        },
        "the first event aged out"
    );
    assert_eq!(events[31].late_us, 6_032, "the newest last");
}

#[test]
fn a_packet_over_10_ms_late_is_warned_at_most_once_per_5_s_with_the_skipped_count() {
    let mut log = VbanStallLog::default();
    let warn = |utc_ms: i64, late_us: u64, suppressed: u64| {
        Some(VbanStallWarn {
            event: VbanLateEvent { utc_ms, late_us },
            suppressed,
        })
    };
    assert_eq!(sent_late(&mut log, P, 10_000), None, "exactly 10 ms");
    let s2 = P + 41_667;
    assert_eq!(
        sent_late(&mut log, s2, 10_001),
        warn(1_790_777_131_204, 10_001, 0),
        "the first one over 10 ms"
    );
    assert_eq!(
        sent_late(&mut log, s2 + UNITS_PER_SECOND, 20_000),
        None,
        "1 s later"
    );
    assert_eq!(
        sent_late(&mut log, s2 + VBAN_STALL_WARN_EVERY_100NS - 1, 12_000),
        None,
        "just under 5 s later"
    );
    let third = s2 + VBAN_STALL_WARN_EVERY_100NS;
    assert_eq!(
        sent_late(&mut log, third, 15_000),
        warn(1_790_777_136_204, 15_000, 2),
        "5 s after the last WARN: warned, with the two it skipped"
    );
    assert_eq!(
        sent_late(&mut log, third + UNITS_PER_SECOND, 9_000),
        None,
        "under 10 ms: neither warned nor skipped"
    );
    let fourth = third + 6 * UNITS_PER_SECOND;
    assert_eq!(
        sent_late(&mut log, fourth, 11_000),
        warn(1_790_777_142_204, 11_000, 0),
        "6 s later: warned, nothing skipped since the last WARN"
    );
    assert_eq!(log.late_events().len(), 7, "every one is an event");
    assert_eq!(log.late_max_us(), 20_000);
}

#[test]
fn the_max_covers_the_bucket_being_filled_and_the_last_full_one() {
    let mut log = VbanStallLog::default();
    let small = |log: &mut VbanStallLog| {
        log.observe(P, P + 3_000, P); // 300 µs: not an event
    };
    log.observe(P, P + 70_000, P); // 7 ms
    for _ in 0..VBAN_STALL_BUCKET_PACKETS - 2 {
        small(&mut log);
    }
    assert_eq!(log.late_max_us(), 7_000, "14 399: one bucket");
    small(&mut log);
    assert_eq!(
        log.late_max_us(),
        7_000,
        "14 400: the full bucket is kept as the last one"
    );
    for _ in 0..VBAN_STALL_BUCKET_PACKETS - 1 {
        small(&mut log);
    }
    assert_eq!(
        log.late_max_us(),
        7_000,
        "28 799: still the last full bucket"
    );
    small(&mut log);
    assert_eq!(
        log.late_max_us(),
        300,
        "28 800: the slow packet is two buckets back — gone"
    );
    assert_eq!(log.late_events().len(), 1, "the ring keeps its event");
}

#[test]
fn a_late_packet_of_the_sender_reaches_the_status_at_its_send_instant() {
    let out = VbanOut::new();
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let l = VBAN_SEND_LATENCY_100NS;
    // The thread reaches the block's packet 0 12 ms after it was due.
    let sent = P + l + 120_000;
    let mut clock = FakeClock::at(sent);
    let mut sink = RecordingSink::on(&clock);
    let n =
        VbanSender::default().send_block(&out, &ProgramBlock::silence(P), &mut sink, &mut clock);
    assert_eq!(n, 8);
    let st = out.status();
    assert_eq!(
        st.late_events,
        vec![
            VbanLateEvent {
                utc_ms: 1_790_777_131_278,
                late_us: 12_000,
            },
            VbanLateEvent {
                utc_ms: 1_790_777_131_278,
                late_us: 7_833,
            },
        ],
        "packet 0 12 ms late, packet 1 (sent right after it) 7.8 ms, packet 2 3.7 ms: no event"
    );
    assert_eq!(st.late_max_us, 12_000);
    assert_eq!(st.late_sends, 3, "over 2 ms: packets 0, 1 and 2");
    assert_eq!(sink.sent[3].0, P + l + 125_000, "packet 3 waited: on time");
}

/// A [`FakeClock`] whose readings are labelled 10 s later (a fleet shift):
/// a late packet's `utc_ms` must come off the clock's label, not off its
/// reading (review round 1).
struct ShiftedClock(FakeClock);

impl VbanClock for ShiftedClock {
    fn now_100ns(&mut self) -> i64 {
        self.0.now_100ns()
    }
    fn sleep_100ns(&mut self, d_100ns: i64) {
        self.0.sleep_100ns(d_100ns);
    }
    fn slew_owed_100ns(&self) -> i64 {
        self.0.slew_owed_100ns()
    }
    fn label_100ns(&self, t_100ns: i64) -> i64 {
        t_100ns + 100_000_000
    }
}

#[test]
fn a_late_packet_of_the_sender_is_stamped_with_its_clocks_label() {
    let out = VbanOut::new();
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = P + VBAN_SEND_LATENCY_100NS + 120_000;
    let mut clock = ShiftedClock(FakeClock::at(sent));
    let mut sink = RecordingSink::on(&clock.0);
    VbanSender::default().send_block(&out, &ProgramBlock::silence(P), &mut sink, &mut clock);
    assert_eq!(
        out.status().late_events[0],
        VbanLateEvent {
            utc_ms: 1_790_777_141_278,
            late_us: 12_000,
        },
        "the send reading + 10 s"
    );
}

#[test]
fn vbans_wall_clock_labels_a_reading_with_its_fleet_shift() {
    let fleet = Arc::new(FleetShift::default());
    // A 10 s date step: K_F = 300 slots, D(300) = 10 s.
    assert_eq!(fleet.follow(0, 100_000_000).slots, 300);
    let clock = WallVbanClock::slewing(WallClock::with_fleet(Box::new(SystemClock), fleet));
    assert_eq!(clock.label_100ns(P), P + 100_000_000);
    assert_eq!(
        FakeClock::at(P).label_100ns(P + 7),
        P + 7,
        "a clock with no fleet shift is its own label"
    );
}
