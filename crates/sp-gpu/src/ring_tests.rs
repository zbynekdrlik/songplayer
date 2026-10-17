//! Tests for the picture ring's shared state and its frames (#279 P2): a
//! frame holds its slot until its last clone drops, a write waits for a
//! drop (bounded), a dead ring refuses writes and wakes its writer.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{FAKE_HANDLE_BASE, FakeRing, GpuFrame, RingCore, RingStats, Taken, Written, hand_over};
use crate::error::GpuError;
use crate::ring_state::{KEY_CONSUMER, KEY_PRODUCER, RingFormat, SizeClass, SlotKey};

/// No wait: the ring must have room.
const NOW: Duration = Duration::ZERO;
/// Far above any wait a test means to see end.
const LONG: Duration = Duration::from_secs(20);

fn frame(ring: &FakeRing, width: u32, height: u32) -> GpuFrame {
    ring.frame(width, height, NOW)
        .unwrap_or_else(|e| panic!("a free slot: {e}"))
}

#[test]
fn a_frame_holds_its_slot_until_its_last_clone_drops() {
    let ring = FakeRing::new(1);
    assert_eq!(ring.stats(), RingStats::default());
    let first = frame(&ring, 1920, 1080);
    let clone = first.clone();
    assert!(clone.ptr_eq(&first));
    assert_eq!((ring.stats().slots, ring.stats().in_use), (1, 1));
    drop(first);
    assert_eq!(ring.stats().in_use, 1, "a clone still refers to it");
    drop(clone);
    assert_eq!((ring.stats().slots, ring.stats().in_use), (1, 0));
    let again = frame(&ring, 1920, 1080);
    assert_eq!(again.key().slot, 0, "the freed slot is written again");
    assert_eq!(again.key().alloc_gen, 1, "its texture kept");
}

#[test]
fn a_frame_carries_its_picture_its_slot_and_its_own_id() {
    let ring = FakeRing::new(2);
    let other = FakeRing::new(1);
    let a = frame(&ring, 1919, 1079);
    let b = frame(&ring, 1280, 720);
    let c = frame(&other, 1280, 720);
    assert_eq!((a.width(), a.height()), (1919, 1079));
    assert_eq!(a.texture_size(), (1920, 1080), "the slot's even texture");
    assert_eq!(a.format(), RingFormat::Nv12);
    assert_eq!(
        (b.width(), b.height(), b.texture_size()),
        (1280, 720, (1280, 720))
    );
    let (ka, kb, kc) = (a.key(), b.key(), c.key());
    assert_eq!((ka.slot, ka.alloc_gen), (0, 1));
    assert_eq!((kb.slot, kb.alloc_gen), (1, 2));
    assert_eq!(ka.ring, kb.ring);
    assert_ne!(ka.ring, kc.ring, "every ring has its own number");
    assert_eq!(
        kc,
        SlotKey {
            ring: kc.ring,
            slot: 0,
            alloc_gen: 1
        }
    );
    assert_eq!(
        (a.shared_handle(), b.shared_handle()),
        (FAKE_HANDLE_BASE + 4, FAKE_HANDLE_BASE + 8),
        "never a real handle's value"
    );
    let ids = [a.id(), b.id(), c.id()];
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "{ids:?}"
    );
    assert!(!a.ptr_eq(&b));
}

#[test]
fn a_frame_prints_its_id_size_and_slot_only() {
    let ring = FakeRing::new(1);
    let a = frame(&ring, 1280, 720);
    let text = format!("{a:?}");
    for part in [
        "GpuFrame",
        &format!("id: {}", a.id()),
        "width: 1280",
        "height: 720",
        "slot: 0",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
}

#[test]
fn a_write_waits_for_a_frame_to_drop() {
    let ring = FakeRing::new(1);
    let held = frame(&ring, 1920, 1080);
    let (done_tx, done_rx) = mpsc::channel();
    let writer = {
        let ring = ring.clone();
        thread::spawn(move || {
            let written = ring.frame(1920, 1080, LONG).map(|f| f.key());
            let _ = done_tx.send(written);
        })
    };
    assert!(
        done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "no write while every slot is in use"
    );
    // `stats` takes the ring's lock, which the writer gives up only inside
    // its wait: reading its counted wait proves it is parked there.
    let parked = (0..20_000).any(|_| {
        thread::sleep(Duration::from_millis(1));
        ring.stats().waits == 1
    });
    assert!(parked, "the writer waits for a slot");
    drop(held);
    let written = done_rx
        .recv_timeout(LONG)
        .expect("the drop wakes the writer");
    assert_eq!(written.map(|key| (key.slot, key.alloc_gen)), Ok((0, 1)));
    writer.join().expect("the writer thread");
    let stats = ring.stats();
    assert_eq!((stats.waits, stats.full, stats.in_use), (1, 0, 0));
}

#[test]
fn a_write_that_finds_no_slot_within_the_bound_fails() {
    let ring = FakeRing::new(1);
    let _held = frame(&ring, 1920, 1080);
    let bound = Duration::from_millis(20);
    assert_eq!(
        ring.frame(1920, 1080, bound).map(|f| f.id()),
        Err(GpuError::RingFull { waited_ms: 20 })
    );
    assert_eq!((ring.stats().waits, ring.stats().full), (1, 1));
    assert_eq!(ring.stats().in_use, 1);
}

#[test]
fn a_dead_ring_refuses_writes_and_wakes_its_writer() {
    let ring = FakeRing::new(1);
    let held = frame(&ring, 1920, 1080);
    assert!(!held.ring_dead());
    let (done_tx, done_rx) = mpsc::channel();
    let writer = {
        let ring = ring.clone();
        thread::spawn(move || {
            let _ = done_tx.send(ring.frame(1920, 1080, LONG).map(|f| f.id()));
        })
    };
    assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err());
    assert!(held.kill_ring("a test", Some(0x80)), "its first kill");
    assert!(!held.kill_ring("a test", None), "already dead");
    assert_eq!(
        done_rx
            .recv_timeout(LONG)
            .expect("the kill wakes the writer"),
        Err(GpuError::RingDead)
    );
    writer.join().expect("the writer thread");
    assert!(held.ring_dead());
    assert!(ring.stats().dead);
    drop(held);
    assert_eq!(
        ring.frame(1920, 1080, NOW).map(|f| f.id()),
        Err(GpuError::RingDead),
        "a free slot changes nothing"
    );
}

#[test]
fn the_fake_ring_dies_like_the_real_one() {
    let ring = FakeRing::new(2);
    assert!(ring.kill());
    assert!(!ring.kill());
    assert!(ring.stats().dead);
    assert_eq!(
        ring.frame(64, 64, NOW).map(|f| f.id()),
        Err(GpuError::RingDead)
    );
}

#[test]
fn a_picture_no_slot_takes_is_refused() {
    let ring = FakeRing::new(1);
    for (width, height) in [(0, 1080), (1920, 0), (16_385, 2), (2, 16_385)] {
        assert_eq!(
            ring.frame(width, height, NOW).map(|f| f.id()),
            Err(GpuError::RingPicture {
                width,
                height,
                surface_width: 16_384,
                surface_height: 16_384,
            })
        );
    }
    assert_eq!(ring.stats(), RingStats::default(), "no slot taken");
    assert!(ring.frame(16_384, 16_384, NOW).is_ok());
}

/// A class of `width`×`height` (NV12).
fn class(width: u32, height: u32) -> SizeClass {
    SizeClass::of(RingFormat::Nv12, width, height)
}

#[test]
fn a_take_says_whether_to_make_the_texture_and_which_key_to_acquire() {
    let core = RingCore::new(2);
    let fhd = class(1920, 1080);
    let made = core.take(fhd, NOW).expect("room");
    assert_eq!(
        made,
        Taken {
            slot: 0,
            alloc_gen: 1,
            create: true,
            key: KEY_PRODUCER
        }
    );
    core.release(0, 1);
    assert_eq!(
        core.take(fhd, NOW),
        Ok(Taken {
            slot: 0,
            alloc_gen: 1,
            create: false,
            key: KEY_PRODUCER
        }),
        "never written: key 0"
    );
    let frame = hand_over(
        &core,
        Written {
            slot: 0,
            alloc_gen: 1,
            class: fhd,
            size: (1920, 1080),
            handle: 0,
            keep: None,
        },
    );
    assert_eq!(
        core.slot(0).map(|s| (s.written, s.in_use)),
        Some((true, true))
    );
    drop(frame);
    assert_eq!(
        core.take(fhd, NOW),
        Ok(Taken {
            slot: 0,
            alloc_gen: 1,
            create: false,
            key: KEY_CONSUMER
        }),
        "written: the key 1 its release left"
    );
    assert_eq!(core.stats().in_use, 1);
}

#[test]
fn a_kept_value_lives_as_long_as_the_frame() {
    let core = RingCore::new(1);
    let fhd = class(1920, 1080);
    let taken = core.take(fhd, NOW).expect("room");
    let keep = std::sync::Arc::new(7_u32);
    let frame = hand_over(
        &core,
        Written {
            slot: taken.slot,
            alloc_gen: taken.alloc_gen,
            class: fhd,
            size: (1920, 1080),
            handle: 0x40,
            keep: Some(keep.clone()),
        },
    );
    assert_eq!(std::sync::Arc::strong_count(&keep), 2);
    assert_eq!(frame.shared_handle(), 0x40);
    let clone = frame.clone();
    drop(frame);
    assert_eq!(std::sync::Arc::strong_count(&keep), 2);
    drop(clone);
    assert_eq!(
        std::sync::Arc::strong_count(&keep),
        1,
        "the NT handle closes"
    );
    assert_eq!(core.stats().in_use, 0);
}
