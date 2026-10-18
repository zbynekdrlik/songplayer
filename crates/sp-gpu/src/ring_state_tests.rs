//! Tests for the picture ring's decisions (#279 P2): the size classes, the
//! copy box, the keys, an acquire's answer, the slots a write takes and a
//! drop frees, the writer's wait and the compositor's open slots.

use std::cell::Cell;
use std::time::Duration;

use super::{
    Acquired, KEY_CONSUMER, KEY_PRODUCER, MAX_SIZE_CLASSES, OPENED_IDLE_COMPOSES, OPENED_MAX,
    OpenedKeys, RING_CAPACITY, RING_WAIT, Release, RingFormat, RingState, SizeClass, SlotDraw,
    SlotKey, SlotOpen, SlotState, Take, Touch, WRITER_ACQUIRE_MS, copy_box, hresult_text,
    make_texture, missing_subresource, producer_key, slot_draw, stale_texture, why_killed,
    writer_step,
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
fn a_subresource_past_the_source_s_slices_times_its_mips_is_missing() {
    assert_eq!(
        missing_subresource(3, 4, 1),
        None,
        "a decoder array's last slice"
    );
    assert_eq!(missing_subresource(4, 4, 1), Some(4), "one past it");
    assert_eq!(missing_subresource(9, 4, 1), Some(4), "well past it");
    assert_eq!(missing_subresource(0, 1, 1), None);
    assert_eq!(missing_subresource(1, 1, 1), Some(1));
    assert_eq!(missing_subresource(5, 3, 2), None, "3 slices of 2 mips");
    assert_eq!(missing_subresource(6, 3, 2), Some(6));
    assert_eq!(
        missing_subresource(u32::MAX, u32::MAX, 2),
        Some(u32::MAX),
        "saturated"
    );
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
fn a_ring_s_death_names_its_hresult_in_hex() {
    assert_eq!(hresult_text(Some(0x80)), "0x00000080");
    assert_eq!(hresult_text(Some(0x887A_0005)), "0x887a0005");
    assert_eq!(hresult_text(None), "", "none told it");
}

#[test]
fn an_answer_names_the_hresult_it_was_read_from() {
    for hresult in [0, 0x102, 0x80, 0x887A_0005, 1] {
        assert_eq!(Acquired::of(hresult).hresult(), hresult);
    }
}

#[test]
fn a_writer_writes_only_on_a_held_key_and_kills_the_ring_on_any_other() {
    assert_eq!(writer_step(Acquired::Held), Ok(()));
    let whys = [
        writer_step(Acquired::Busy),
        writer_step(Acquired::Abandoned),
        writer_step(Acquired::Failed(0x8007_0057)),
    ];
    for why in whys {
        assert!(why.is_err_and(|why| !why.is_empty()), "{why:?}");
    }
    assert!(
        whys[0] != whys[1] && whys[1] != whys[2] && whys[0] != whys[2],
        "each says its own why: {whys:?}"
    );
    assert_eq!(
        Duration::from_millis(WRITER_ACQUIRE_MS.into()),
        RING_WAIT,
        "a writer waits for its key no longer than for its slot"
    );
}

/// `slot_draw` with a device-removed answer that must never be read.
fn not_asked(open: SlotOpen) -> SlotDraw {
    slot_draw(open, || panic!("the device was asked for {open:?}"))
}

/// `slot_draw` with the device-removed answer `removed`, and whether it was
/// asked.
fn asked(open: SlotOpen, removed: bool) -> (SlotDraw, bool) {
    let was_asked = Cell::new(false);
    let draw = slot_draw(open, || {
        was_asked.set(true);
        removed
    });
    (draw, was_asked.get())
}

#[test]
fn the_compositor_draws_a_held_slot_a_busy_one_black_and_closes_a_dead_ring_s() {
    assert_eq!(
        not_asked(SlotOpen::Acquired(Acquired::Held)),
        SlotDraw::Draw
    );
    assert_eq!(
        not_asked(SlotOpen::Acquired(Acquired::Busy)),
        SlotDraw::Black
    );
    assert_eq!(not_asked(SlotOpen::Dead), SlotDraw::Gone);
    assert_eq!(
        not_asked(SlotOpen::Acquired(Acquired::Abandoned)),
        SlotDraw::KillRing,
        "a device was lost holding its key"
    );
}

#[test]
fn a_failed_slot_is_the_compositor_s_lost_device_or_else_kills_the_ring() {
    for open in [
        SlotOpen::Failed,
        SlotOpen::Acquired(Acquired::Failed(0x887A_0005)),
    ] {
        assert_eq!(asked(open, true), (SlotDraw::DeviceLost, true), "{open:?}");
        assert_eq!(asked(open, false), (SlotDraw::KillRing, true), "{open:?}");
    }
}

#[test]
fn each_kill_names_its_cause() {
    let whys = [
        why_killed(SlotOpen::Acquired(Acquired::Abandoned)),
        why_killed(SlotOpen::Failed),
        why_killed(SlotOpen::Acquired(Acquired::Failed(1))),
    ];
    assert!(whys.iter().all(|why| !why.is_empty()));
    assert!(
        whys[0] != whys[1] && whys[1] != whys[2] && whys[0] != whys[2],
        "{whys:?}"
    );
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
    assert!(ring.kill(), "its first kill");
    assert!(!ring.kill(), "already dead");
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

/// A slot of the ring under allocation `alloc_gen`.
fn state(alloc_gen: u64) -> SlotState {
    SlotState {
        class: fhd(),
        alloc_gen,
        in_use: false,
        written: true,
    }
}

#[test]
fn a_writer_s_texture_is_stale_once_its_slot_is_emptied_or_made_again() {
    assert!(!stale_texture(Some(state(3)), 3));
    assert!(stale_texture(Some(state(4)), 3), "made again");
    assert!(stale_texture(None, 3), "emptied");
}

#[test]
fn a_write_makes_the_texture_of_a_new_slot_or_one_it_does_not_hold() {
    assert!(make_texture(true, None, 5), "a new slot");
    assert!(make_texture(true, Some(4), 5), "a slot made again");
    assert!(!make_texture(false, Some(5), 5), "its texture held");
    assert!(
        make_texture(false, None, 5),
        "a reused slot with no texture"
    );
    assert!(
        make_texture(false, Some(4), 5),
        "a texture of another allocation"
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
fn the_open_slot_bound_holds_both_sides_of_a_fade_between_two_rings() {
    assert_eq!(OPENED_MAX, 2 * RING_CAPACITY);
    assert_eq!(OPENED_IDLE_COMPOSES, 90, "3 s at one compose a boundary");
}

#[test]
fn a_slot_no_compose_drew_for_the_idle_bound_closes() {
    let mut keys = OpenedKeys::default();
    assert!(keys.begin_compose().is_empty());
    let _ = keys.touch(key(1, 0, 1));
    let _ = keys.touch(key(1, 1, 1));
    for _ in 1..OPENED_IDLE_COMPOSES {
        assert!(keys.begin_compose().is_empty());
    }
    assert!(
        keys.begin_compose().is_empty(),
        "undrawn for exactly the bound: still open"
    );
    let _ = keys.touch(key(1, 1, 1));
    assert_eq!(
        keys.begin_compose(),
        vec![key(1, 0, 1)],
        "slot 0, undrawn one compose past the bound, closes"
    );
    assert_eq!(keys.len(), 1);
    assert_eq!(keys.touch(key(1, 1, 1)), kept(), "drawn since: still open");
    assert_eq!(
        keys.touch(key(1, 0, 1)),
        opened(Vec::new()),
        "a closed slot is opened again"
    );
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

/// #279 P3a: a slot's texture holds its class's NV12 bytes in VRAM, and the
/// ring's VRAM is every made slot's.
#[test]
fn the_vram_is_every_made_slots_nv12_bytes() {
    assert_eq!(fhd().bytes(), 3_110_400);
    assert_eq!(class(641, 359).bytes(), 642 * 360 * 3 / 2, "its even size");
    let mut ring = RingState::new(4);
    assert_eq!(ring.vram_bytes(), 0);
    for c in [fhd(), fhd(), class(640, 360)] {
        assert!(matches!(ring.take(c), Take::Create { .. }));
    }
    assert_eq!(ring.vram_bytes(), 2 * 3_110_400 + 345_600);
}
