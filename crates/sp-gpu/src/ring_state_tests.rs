//! Tests for the picture ring's decisions (#279 P2): the size classes, the
//! copy box, the keys, an acquire's answer, the slots a write takes and a
//! drop frees, the writer's wait and the compositor's open slots.

use std::time::{Duration, Instant};

use super::{
    Acquired, KEY_CONSUMER, KEY_PRODUCER, MAX_SIZE_CLASSES, OPENED_MAX, OpenedKeys, Release,
    RingFormat, RingState, SizeClass, SlotKey, Take, Touch, copy_box, producer_key, wait_left,
};

/// A class of an even `width`×`height` (NV12).
fn class(width: u32, height: u32) -> SizeClass {
    SizeClass::of(RingFormat::Nv12, width, height)
}

const FHD: (u32, u32) = (1920, 1080);
const UHD: (u32, u32) = (3840, 2160);
const QHD: (u32, u32) = (2560, 1440);

fn fhd() -> SizeClass {
    class(FHD.0, FHD.1)
}

fn uhd() -> SizeClass {
    class(UHD.0, UHD.1)
}

fn qhd() -> SizeClass {
    class(QHD.0, QHD.1)
}

/// The slot and allocation of a take that made or reused a slot.
fn slot_of(take: Take) -> (usize, u64) {
    match take {
        Take::Reuse {
            slot, alloc_gen, ..
        }
        | Take::Create { slot, alloc_gen } => (slot, alloc_gen),
        other => panic!("a take that got no slot: {other:?}"),
    }
}

#[test]
fn a_class_rounds_each_side_up_to_even() {
    assert_eq!(
        (class(1919, 1079).width, class(1919, 1079).height),
        (1920, 1080)
    );
    assert_eq!((class(1920, 1080).width, class(1920, 1080).height), FHD);
    assert_eq!((class(1, 3).width, class(1, 3).height), (2, 4));
    assert_eq!(
        class(u32::MAX, 2).width,
        u32::MAX,
        "saturated, never wrapped"
    );
    assert_eq!(class(4, 2).format, RingFormat::Nv12);
}

#[test]
fn the_copy_box_is_the_even_picture_inside_the_surface() {
    // A 1080p picture in NVDEC's 16-row aligned surface.
    assert_eq!(copy_box(FHD, (1920, 1088)), Some(FHD));
    assert_eq!(copy_box((1919, 1079), (1920, 1088)), Some(FHD));
    assert_eq!(
        copy_box((1920, 1088), (1920, 1088)),
        Some((1920, 1088)),
        "a box as large as the surface fits"
    );
    assert_eq!(copy_box((1921, 1080), (1920, 1088)), None, "1922 columns");
    assert_eq!(copy_box((1920, 1089), (1920, 1088)), None, "1090 rows");
    assert_eq!(
        copy_box((1919, 1080), (1919, 1088)),
        None,
        "the even box is one column wider than an odd surface"
    );
    assert_eq!(
        copy_box((1920, 1079), (1920, 1079)),
        None,
        "and one row taller"
    );
}

#[test]
fn an_empty_or_oversized_picture_has_no_copy_box() {
    let big = (16_384, 16_384);
    assert_eq!(copy_box((0, 1080), big), None);
    assert_eq!(copy_box((1920, 0), big), None);
    assert_eq!(copy_box(big, big), Some(big), "16384 is a texture's side");
    assert_eq!(copy_box((16_385, 1080), (32_768, 2160)), None);
    assert_eq!(copy_box((1920, 16_385), (1920, 32_768)), None);
}

#[test]
fn a_new_texture_is_written_under_key_0_and_every_later_write_under_key_1() {
    assert_eq!(producer_key(false), KEY_PRODUCER);
    assert_eq!(producer_key(true), KEY_CONSUMER);
    assert_eq!((KEY_PRODUCER, KEY_CONSUMER), (0, 1));
}

#[test]
fn an_acquire_is_read_from_its_raw_hresult() {
    assert_eq!(Acquired::of(0), Acquired::Held);
    assert_eq!(Acquired::of(0x102), Acquired::Busy);
    assert_eq!(Acquired::of(0x80), Acquired::Abandoned);
    assert_eq!(Acquired::of(0x887A_0005), Acquired::Failed(0x887A_0005));
    assert_eq!(Acquired::of(1), Acquired::Failed(1));
}

#[test]
fn only_a_lost_or_failed_acquire_kills_the_ring() {
    assert!(!Acquired::Held.kills_ring());
    assert!(!Acquired::Busy.kills_ring(), "a busy slot misses one draw");
    assert!(Acquired::Abandoned.kills_ring());
    assert!(Acquired::Failed(0x8007_0057).kills_ring());
}

#[test]
fn writes_make_slots_until_every_one_is_in_use() {
    let mut ring = RingState::new(3);
    assert!(ring.has_room());
    assert_eq!(
        ring.take(fhd()),
        Take::Create {
            slot: 0,
            alloc_gen: 1
        }
    );
    assert_eq!(
        ring.take(fhd()),
        Take::Create {
            slot: 1,
            alloc_gen: 2
        }
    );
    assert_eq!(
        ring.take(fhd()),
        Take::Create {
            slot: 2,
            alloc_gen: 3
        }
    );
    assert!(!ring.has_room());
    assert_eq!(ring.take(fhd()), Take::Full);
    assert_eq!((ring.made(), ring.in_use()), (3, 3));
    let made = ring.slot(1).expect("slot 1 holds a texture");
    assert_eq!(made.class, fhd());
    assert_eq!(made.alloc_gen, 2);
    assert!(made.in_use);
    assert!(!made.written);
    assert_eq!(ring.slot(3), None, "past the ring");
}

#[test]
fn a_dropped_frame_s_slot_is_written_again_under_the_key_its_last_release_left() {
    let mut ring = RingState::new(2);
    let (a, a_gen) = slot_of(ring.take(fhd()));
    let (b, b_gen) = slot_of(ring.take(fhd()));
    assert_eq!(ring.release(b, b_gen), Release::Freed);
    assert!(ring.has_room());
    assert_eq!(ring.in_use(), 1);
    assert_eq!(
        ring.take(fhd()),
        Take::Reuse {
            slot: b,
            alloc_gen: b_gen,
            key: KEY_PRODUCER
        },
        "never written: its new texture's key 0"
    );
    ring.written(a, a_gen);
    assert!(ring.slot(a).expect("slot a").written);
    assert_eq!(ring.release(a, a_gen), Release::Freed);
    assert_eq!(
        ring.take(fhd()),
        Take::Reuse {
            slot: a,
            alloc_gen: a_gen,
            key: KEY_CONSUMER
        },
        "written once: the key 1 its release left"
    );
    assert_eq!(ring.take(fhd()), Take::Full);
}

#[test]
fn the_first_free_slot_of_the_class_is_reused() {
    let mut ring = RingState::new(3);
    let taken: Vec<_> = (0..3).map(|_| slot_of(ring.take(fhd()))).collect();
    assert_eq!(ring.release(taken[2].0, taken[2].1), Release::Freed);
    assert_eq!(ring.release(taken[1].0, taken[1].1), Release::Freed);
    assert_eq!(slot_of(ring.take(fhd())), taken[1]);
    assert_eq!(slot_of(ring.take(fhd())), taken[2]);
}

#[test]
fn a_stale_release_changes_nothing() {
    let mut ring = RingState::new(2);
    let (slot, alloc_gen) = slot_of(ring.take(fhd()));
    assert_eq!(ring.release(slot, alloc_gen + 1), Release::Stale);
    assert_eq!(ring.release(1, 1), Release::Stale, "an empty slot");
    assert_eq!(ring.release(7, alloc_gen), Release::Stale, "past the ring");
    assert_eq!(ring.in_use(), 1);
    assert_eq!(ring.release(slot, alloc_gen), Release::Freed);
    assert_eq!(
        ring.release(slot, alloc_gen),
        Release::Stale,
        "a free slot is not freed twice"
    );
    assert_eq!(ring.in_use(), 0);
}

#[test]
fn a_written_mark_lands_only_on_its_own_allocation() {
    let mut ring = RingState::new(1);
    let (slot, alloc_gen) = slot_of(ring.take(fhd()));
    ring.written(slot, alloc_gen + 1);
    ring.written(5, alloc_gen);
    assert!(!ring.slot(slot).expect("the slot").written);
    ring.written(slot, alloc_gen);
    assert!(ring.slot(slot).expect("the slot").written);
}

#[test]
fn a_dead_ring_takes_no_write_and_ends_every_wait() {
    let mut ring = RingState::new(1);
    let _ = ring.take(fhd());
    assert!(!ring.has_room());
    assert!(!ring.is_dead());
    ring.kill();
    assert!(ring.is_dead());
    assert!(ring.has_room(), "a waiter wakes to see it dead");
    assert_eq!(ring.take(fhd()), Take::Dead);
    assert_eq!(ring.take(uhd()), Take::Dead);
}

#[test]
fn a_second_class_makes_slots_of_its_own_and_keeps_the_first() {
    let mut ring = RingState::new(4);
    let a = slot_of(ring.take(fhd()));
    let b = slot_of(ring.take(fhd()));
    assert_eq!(ring.release(a.0, a.1), Release::Freed);
    assert_eq!(ring.release(b.0, b.1), Release::Freed);
    assert_eq!(
        ring.take(uhd()),
        Take::Create {
            slot: 2,
            alloc_gen: 3
        },
        "an empty slot before a free one of the other class"
    );
    assert_eq!(ring.made(), 3, "the 1080p slots stay");
    assert_eq!(
        ring.take(fhd()),
        Take::Reuse {
            slot: a.0,
            alloc_gen: a.1,
            key: KEY_PRODUCER
        }
    );
}

#[test]
fn a_third_class_drops_the_free_slots_of_the_least_recent_one() {
    let mut ring = RingState::new(4);
    let a = slot_of(ring.take(fhd()));
    let b = slot_of(ring.take(uhd()));
    let a_again = slot_of(ring.take(fhd()));
    for (slot, alloc_gen) in [a, b, a_again] {
        assert_eq!(ring.release(slot, alloc_gen), Release::Freed);
    }
    // 1080p was written last: 4K is the least recent class.
    assert_eq!(
        ring.take(qhd()),
        Take::Create {
            slot: b.0,
            alloc_gen: 4
        },
        "4K's free slot was emptied, and is the first empty one"
    );
    assert_eq!(ring.slot(a.0).expect("1080p kept").class, fhd());
    assert_eq!(ring.slot(a_again.0).expect("1080p kept").class, fhd());
    assert_eq!(ring.made(), 3);
    assert_eq!(MAX_SIZE_CLASSES, 2);
}

#[test]
fn an_in_use_slot_of_a_class_no_longer_kept_goes_when_its_frame_drops() {
    let mut ring = RingState::new(4);
    let a = slot_of(ring.take(fhd()));
    let b = slot_of(ring.take(uhd()));
    let c = slot_of(ring.take(qhd()));
    assert_eq!(ring.slot(a.0).expect("in use").class, fhd());
    assert_eq!(ring.made(), 3, "a slot in use is never dropped");
    assert_eq!(ring.release(a.0, a.1), Release::Dropped);
    assert_eq!(ring.slot(a.0), None);
    assert_eq!(ring.release(b.0, b.1), Release::Freed, "4K is still kept");
    assert_eq!(ring.release(c.0, c.1), Release::Freed);
    assert_eq!(ring.made(), 2);
}

#[test]
fn a_full_ring_writes_a_new_class_over_a_free_slot_of_the_other() {
    let mut ring = RingState::new(2);
    let a = slot_of(ring.take(fhd()));
    let b = slot_of(ring.take(fhd()));
    assert_eq!(ring.take(uhd()), Take::Full, "every slot in use");
    assert_eq!(ring.release(b.0, b.1), Release::Freed);
    assert_eq!(
        ring.take(uhd()),
        Take::Create {
            slot: b.0,
            alloc_gen: 3
        }
    );
    assert_eq!(ring.slot(b.0).expect("remade").class, uhd());
    assert_eq!(ring.slot(a.0).expect("in use").class, fhd());
    assert_eq!(
        ring.release(b.0, b.1),
        Release::Stale,
        "the old allocation of the slot is gone"
    );
    assert_eq!(ring.release(b.0, 3), Release::Freed);
}

#[test]
fn the_wait_ends_at_its_bound() {
    let since = Instant::now();
    let limit = Duration::from_millis(100);
    assert_eq!(wait_left(since, since, limit), Some(limit));
    assert_eq!(
        wait_left(since, since + Duration::from_millis(30), limit),
        Some(Duration::from_millis(70))
    );
    assert_eq!(wait_left(since, since + limit, limit), None);
    assert_eq!(
        wait_left(since, since + Duration::from_millis(250), limit),
        None
    );
    assert_eq!(
        wait_left(since + Duration::from_millis(5), since, limit),
        Some(limit),
        "a clock read before the start counts no wait"
    );
}

fn key(ring: u64, slot: usize, alloc_gen: u64) -> SlotKey {
    SlotKey {
        ring,
        slot,
        alloc_gen,
    }
}

fn opened(close: Vec<SlotKey>) -> Touch {
    Touch { open: true, close }
}

fn kept() -> Touch {
    Touch {
        open: false,
        close: Vec::new(),
    }
}

#[test]
fn a_slot_is_opened_once() {
    let mut keys = OpenedKeys::default();
    assert!(keys.is_empty());
    assert_eq!(keys.touch(key(1, 0, 1)), opened(Vec::new()));
    assert_eq!(keys.touch(key(1, 0, 1)), kept());
    assert_eq!(keys.touch(key(1, 1, 2)), opened(Vec::new()));
    assert_eq!(
        keys.touch(key(2, 0, 1)),
        opened(Vec::new()),
        "another ring's slot 0"
    );
    assert_eq!(keys.len(), 3);
    assert!(!keys.is_empty());
}

#[test]
fn a_slot_made_again_closes_its_old_texture() {
    let mut keys = OpenedKeys::default();
    let _ = keys.touch(key(1, 0, 1));
    let _ = keys.touch(key(1, 1, 2));
    assert_eq!(keys.touch(key(1, 0, 5)), opened(vec![key(1, 0, 1)]));
    assert_eq!(keys.len(), 2);
    assert_eq!(keys.touch(key(1, 1, 2)), kept(), "the other slot stays");
}

#[test]
fn past_the_bound_the_least_recently_drawn_slot_is_closed() {
    let mut keys = OpenedKeys::default();
    for slot in 0..OPENED_MAX {
        assert_eq!(keys.touch(key(1, slot, 1)), opened(Vec::new()));
    }
    assert_eq!(keys.len(), OPENED_MAX);
    assert_eq!(keys.touch(key(1, 0, 1)), kept(), "slot 0 drawn again");
    assert_eq!(
        keys.touch(key(2, 0, 1)),
        opened(vec![key(1, 1, 1)]),
        "slot 1 is now the least recently drawn"
    );
    assert_eq!(keys.touch(key(2, 1, 1)), opened(vec![key(1, 2, 1)]));
    assert_eq!(keys.len(), OPENED_MAX);
}

#[test]
fn a_forgotten_slot_is_opened_again() {
    let mut keys = OpenedKeys::default();
    let _ = keys.touch(key(1, 0, 1));
    let _ = keys.touch(key(1, 1, 1));
    keys.forget(key(1, 0, 1));
    assert_eq!(keys.len(), 1);
    assert_eq!(keys.touch(key(1, 0, 1)), opened(Vec::new()));
    assert_eq!(keys.touch(key(1, 1, 1)), kept());
}
