//! #317: the ring of late MAX boundaries (`max.late_events`).
//! Wired via `#[cfg(test)] #[path = "program_max_late_tests.rs"] mod tests;`.

use std::time::{Duration, Instant};

use super::{LateKind, LateRing, Lateness, MAX_LATE_EVENTS, MaxLateEvent};
use crate::playback::program_max::MaxOut;

const US: Duration = Duration::from_micros(1);

fn event(utc_ms: i64, late_us: u64) -> MaxLateEvent {
    MaxLateEvent {
        utc_ms,
        kind: LateKind::Compose,
        late_us,
        after_offer_us: 0,
    }
}

#[test]
fn the_ring_keeps_the_newest_32_oldest_first() {
    assert_eq!(MAX_LATE_EVENTS, 32, "as many as ndi.readback_late_events");
    let mut ring = LateRing::default();
    assert!(ring.events().is_empty());
    for i in 0..(MAX_LATE_EVENTS as i64 + 3) {
        ring.push(event(i, i as u64 * 10));
    }
    let kept = ring.events();
    assert_eq!(kept.len(), MAX_LATE_EVENTS);
    assert_eq!(kept[0], event(3, 30), "the three oldest went");
    assert_eq!(kept[MAX_LATE_EVENTS - 1], event(34, 340), "the newest last");
    let stamps: Vec<i64> = kept.iter().map(|e| e.utc_ms).collect();
    assert_eq!(
        stamps,
        (3..35).collect::<Vec<_>>(),
        "in order, none skipped"
    );
}

#[test]
fn the_ring_below_its_bound_drops_nothing() {
    let mut ring = LateRing::default();
    for i in 0..(MAX_LATE_EVENTS as i64 - 1) {
        ring.push(event(i, 1));
    }
    assert_eq!(ring.events().len(), MAX_LATE_EVENTS - 1);
    assert_eq!(ring.events()[0].utc_ms, 0);
    ring.push(event(99, 1));
    assert_eq!(
        ring.events().len(),
        MAX_LATE_EVENTS,
        "full, nothing dropped yet"
    );
    assert_eq!(ring.events()[0].utc_ms, 0);
}

#[test]
fn a_late_compose_and_a_late_send_are_two_events_compose_first() {
    let offered = Instant::now();
    let late = Lateness {
        offered,
        compose: Some((offered + 12_000 * US, offered + 14_500 * US)),
        send: Some((offered + 36_000 * US, offered + 41_250 * US)),
    };
    assert_eq!(
        late.events(17_632_000_123_456),
        [
            MaxLateEvent {
                utc_ms: 1_763_200_012,
                kind: LateKind::Compose,
                late_us: 2_500,
                after_offer_us: 14_500,
            },
            MaxLateEvent {
                utc_ms: 1_763_200_012,
                kind: LateKind::Send,
                late_us: 5_250,
                after_offer_us: 41_250,
            },
        ]
    );
}

#[test]
fn a_boundary_on_time_is_no_event() {
    let offered = Instant::now();
    let on_time = Lateness {
        offered,
        compose: None,
        send: None,
    };
    assert!(on_time.events(10_000).is_empty());
    let send_only = Lateness {
        offered,
        compose: None,
        send: Some((offered + 3 * US, offered + 7 * US)),
    };
    let events = send_only.events(20_000);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].kind,
            events[0].late_us,
            events[0].after_offer_us,
            events[0].utc_ms
        ),
        (LateKind::Send, 4, 7, 2)
    );
}

#[test]
fn max_status_carries_the_ring_and_an_on_time_boundary_adds_nothing() {
    let max = MaxOut::new();
    let offered = Instant::now();
    max.record_late(
        50_000,
        &Lateness {
            offered,
            compose: None,
            send: None,
        },
    );
    assert!(max.status().late_events.is_empty());
    max.record_late(
        70_000,
        &Lateness {
            offered,
            compose: Some((offered, offered + 900 * US)),
            send: Some((offered + 1_000 * US, offered + 1_200 * US)),
        },
    );
    max.record_late(
        90_000,
        &Lateness {
            offered,
            compose: Some((offered, offered + 40 * US)),
            send: None,
        },
    );
    let kept = max.status().late_events;
    let shown: Vec<(i64, LateKind, u64, u64)> = kept
        .iter()
        .map(|e| (e.utc_ms, e.kind, e.late_us, e.after_offer_us))
        .collect();
    assert_eq!(
        shown,
        [
            (7, LateKind::Compose, 900, 900),
            (7, LateKind::Send, 200, 1_200),
            (9, LateKind::Compose, 40, 40),
        ]
    );
}

#[test]
fn an_event_serialises_with_its_wire_names() {
    let json = serde_json::to_value(MaxLateEvent {
        utc_ms: 1_763_200_012,
        kind: LateKind::Send,
        late_us: 5,
        after_offer_us: 6,
    })
    .expect("serialises");
    assert_eq!(
        json,
        serde_json::json!({
            "utc_ms": 1_763_200_012_i64,
            "kind": "send",
            "late_us": 5,
            "after_offer_us": 6,
        })
    );
    assert_eq!(
        serde_json::to_value(LateKind::Compose).expect("serialises"),
        "compose"
    );
}
