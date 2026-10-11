//! #233 (review): the host reads SongPlayer's timeline from SongPlayer's
//! anchors, so no relabel of its own can put a block a slot off.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::playback::fleet_shift::shift_100ns;
use crate::playback::vban_out::VbanClock;

/// A counter the test sets.
fn counter() -> (Arc<AtomicI64>, Mono) {
    let at = Arc::new(AtomicI64::new(0));
    let read = at.clone();
    (at, Arc::new(move || read.load(Ordering::SeqCst)))
}

#[test]
fn an_anchor_carries_the_timeline_on_the_counter() {
    let a = Anchor {
        timeline_100ns: 5_000_000,
        mono_100ns: 70_000,
    };
    assert_eq!(a.at(70_000), 5_000_000);
    assert_eq!(a.at(70_300), 5_000_300);
    assert_eq!(a.at(69_900), 4_999_900);
}

/// SongPlayer's timeline after it followed date steps to K 5 is UTC − D(5);
/// a host that had walls of its own and registered one epoch less would
/// read UTC − D(4), a slot off. The host reads SongPlayer's anchor alone:
/// the same instant, the same reading, whatever the host's own relabel.
#[test]
fn the_host_reads_songplayers_timeline_whatever_its_own_relabel_would_be() {
    let utc: i64 = 17_900_000_000_000_000;
    let songplayer = utc - shift_100ns(5);
    let own_relabel = utc - shift_100ns(4);
    assert_eq!(songplayer - own_relabel, -333_333, "a slot apart");
    let (mono, read) = counter();
    mono.store(1_000, Ordering::SeqCst);
    let shared = Arc::new(SharedAnchor::default());
    shared.set(Anchor {
        timeline_100ns: songplayer,
        mono_100ns: 1_000,
    });
    let mut clock = RemoteClock::new(shared, read);
    assert_eq!(clock.read(), songplayer);
    mono.store(334_333, Ordering::SeqCst);
    assert_eq!(clock.now_100ns(), songplayer + 333_333);
}

/// Each anchor moves the clock as SongPlayer's wall moved: a follow's jump
/// ahead at once; a hold never back — the clock waits for the line.
#[test]
fn the_clock_follows_each_anchor_and_never_goes_back() {
    let (mono, read) = counter();
    let shared = Arc::new(SharedAnchor::default());
    let mut clock = RemoteClock::new(shared.clone(), read);
    mono.store(40, Ordering::SeqCst);
    assert_eq!(clock.read(), 40, "no anchor yet: the counter");
    shared.set(Anchor {
        timeline_100ns: 10_000,
        mono_100ns: 100,
    });
    mono.store(200, Ordering::SeqCst);
    assert_eq!(clock.read(), 10_100);
    // SongPlayer's wall jumped 250 µs ahead (a follow's remainder).
    shared.set(Anchor {
        timeline_100ns: 12_600,
        mono_100ns: 300,
    });
    assert_eq!(clock.read(), 12_500);
    // Then held 400 µs: its line stands 4 000 behind the clock.
    shared.set(Anchor {
        timeline_100ns: 8_600,
        mono_100ns: 300,
    });
    mono.store(2_000, Ordering::SeqCst);
    assert_eq!(clock.read(), 12_500, "held, never back");
    mono.store(4_300, Ordering::SeqCst);
    assert_eq!(clock.read(), 12_600, "on again once the line passes it");
}

#[test]
fn the_shared_anchor_is_the_last_one_set() {
    let shared = SharedAnchor::default();
    assert_eq!(shared.get(), None);
    let a = Anchor {
        timeline_100ns: 3,
        mono_100ns: 4,
    };
    shared.set(a);
    shared.set(Anchor {
        timeline_100ns: 7,
        ..a
    });
    assert_eq!(
        shared.get(),
        Some(Anchor {
            timeline_100ns: 7,
            mono_100ns: 4
        })
    );
}

/// The worker's waits really sleep (a lower bound only), and the clock owes
/// no slew (it is SongPlayer's wall that follows a step).
#[test]
fn the_clock_sleeps_and_owes_nothing() {
    let (_, read) = counter();
    let mut clock = RemoteClock::new(Arc::default(), read);
    let started = Instant::now();
    clock.sleep_100ns(20_000);
    assert!(started.elapsed() >= Duration::from_millis(2));
    clock.sleep_100ns(-5);
    assert_eq!(clock.slew_owed_100ns(), 0);
}
