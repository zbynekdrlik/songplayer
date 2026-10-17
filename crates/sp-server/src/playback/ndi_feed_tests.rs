//! #279 P1b: the NDI feed (depth, take and drop-older, late pictures, the
//! coalesce, freshness), the per-boundary source rule (the 30-slot edge on
//! both sides, a setting flip), the way back after a fallback
//! (`ReturnGate`), the switch lines (a `StateLog` of the source) and the
//! telemetry.
//! Wired via `#[cfg(test)] #[path = "ndi_feed_tests.rs"] mod tests;`.

use sp_core::config::NdiSource;
use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};

use super::{
    BlackUyvy, CpuWhy, FeedCore, NDI_FEED_DEPTH, NDI_FEED_STALE_SLOTS, NDI_RETURN_ON_TIME,
    NDI_SWITCH_LOG_EVERY_100NS, NdiFeed, NdiPick, Pushed, ReturnGate, UYVY_BLACK_PAIR, black_uyvy,
    delay_slots, fourcc, ndi_source_for, uyvy_len,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::MAX_STAT_WINDOW;
use crate::playback::stat_window::StateLog;

/// A grid index of 2026: 1.76e9 s · 30.
const K0: i64 = 52_800_000_000;

/// The k-th grid boundary from `K0`.
fn b(k: i64) -> i64 {
    grid_boundary_100ns(K0 + k, GENLOCK_GRID_FPS)
}

/// A picture of its own allocation whose bytes name `k`.
fn pic(k: u8) -> SharedFrame {
    SharedFrame::new(vec![k; 8])
}

fn holds(got: Option<SharedFrame>, k: u8) -> bool {
    got.is_some_and(|p| p[..] == [k; 8])
}

#[test]
fn a_take_returns_its_stamps_picture_and_drops_every_older_one() {
    let mut feed = FeedCore::default();
    assert_eq!(feed.push(b(1), pic(1)), Pushed::Kept);
    assert_eq!(feed.push(b(2), pic(2)), Pushed::Kept);
    assert_eq!(feed.push(b(3), pic(3)), Pushed::Kept);
    assert!(holds(feed.take(b(2)), 2), "boundary 2's own picture");
    assert!(holds(feed.take(b(3)), 3), "a newer one stays");
    assert!(feed.take(b(1)).is_none(), "an older one went with the take");
}

#[test]
fn a_take_of_a_picture_that_never_came_drops_the_older_ones_too() {
    let mut feed = FeedCore::default();
    feed.push(b(1), pic(1));
    feed.push(b(3), pic(3));
    assert!(feed.take(b(2)).is_none(), "boundary 2 was never read back");
    assert!(holds(feed.take(b(3)), 3));
    assert!(feed.take(b(1)).is_none());
}

#[test]
fn the_feed_keeps_three_pictures_then_drops_the_oldest() {
    assert_eq!(NDI_FEED_DEPTH, 3);
    let mut feed = FeedCore::default();
    for k in 1..=3u8 {
        assert_eq!(feed.push(b(i64::from(k)), pic(k)), Pushed::Kept, "{k}");
    }
    assert_eq!(feed.coalesced, 0, "three fit");
    assert_eq!(feed.push(b(4), pic(4)), Pushed::Coalesced);
    assert_eq!(feed.push(b(5), pic(5)), Pushed::Coalesced);
    assert_eq!(feed.coalesced, 2);
    assert!(feed.take(b(2)).is_none(), "the oldest went first");
    for k in 3..=5u8 {
        assert!(holds(feed.take(b(i64::from(k))), k), "the newest stay: {k}");
    }
}

#[test]
fn a_picture_read_back_after_its_boundary_was_sent_is_late() {
    let mut feed = FeedCore::default();
    feed.push(b(1), pic(1));
    assert!(holds(feed.take(b(1)), 1));
    assert!(feed.take(b(2)).is_none(), "boundary 2 went out without it");
    assert_eq!(
        feed.push(b(2), pic(2)),
        Pushed::Late,
        "its boundary was served"
    );
    assert_eq!(feed.push(b(1), pic(1)), Pushed::Late, "an older one too");
    assert_eq!(feed.late, 2);
    assert_eq!(
        feed.push(b(3), pic(3)),
        Pushed::Kept,
        "the next one is on time"
    );
    assert!(holds(feed.take(b(3)), 3));
    assert_eq!(feed.late, 2);
}

#[test]
fn the_newest_stamp_is_the_last_picture_kept_never_a_late_one() {
    let mut feed = FeedCore::default();
    assert_eq!(feed.newest(), None);
    feed.push(b(1), pic(1));
    assert_eq!(feed.newest(), Some(b(1)));
    assert!(feed.take(b(4)).is_none());
    feed.push(b(3), pic(3));
    assert_eq!(
        feed.newest(),
        Some(b(1)),
        "a late picture never entered the feed: no sign of a fresh readback"
    );
    feed.push(b(5), pic(5));
    assert_eq!(feed.newest(), Some(b(5)), "an on-time one is");
}

/// The rule's 30-slot edge on both sides, and a setting flip.
#[test]
fn ndi_comes_from_the_gpu_while_the_setting_says_gpu_and_the_feed_is_fresh() {
    assert_eq!(NDI_FEED_STALE_SLOTS, 30);
    let at = b(100);
    for behind in [0, 1, 29, 30] {
        assert_eq!(
            ndi_source_for(NdiSource::Gpu, Some(b(100 - behind)), at),
            NdiPick::Gpu,
            "{behind} slots behind"
        );
    }
    assert_eq!(
        ndi_source_for(NdiSource::Gpu, Some(b(69)), at),
        NdiPick::Cpu(CpuWhy::Stale),
        "31 slots behind"
    );
    assert_eq!(
        ndi_source_for(NdiSource::Gpu, Some(b(101)), at),
        NdiPick::Gpu,
        "a picture ahead of the boundary"
    );
    assert_eq!(
        ndi_source_for(NdiSource::Gpu, None, at),
        NdiPick::Cpu(CpuWhy::NoReadback)
    );
    assert_eq!(
        ndi_source_for(NdiSource::Cpu, Some(b(99)), at),
        NdiPick::Cpu(CpuWhy::Setting),
        "a fresh feed, the setting flipped to cpu"
    );
}

#[test]
fn a_pick_names_its_source_and_its_reason() {
    assert_eq!(NdiPick::Gpu.source(), NdiSource::Gpu);
    for why in [
        CpuWhy::Setting,
        CpuWhy::NoReadback,
        CpuWhy::Stale,
        CpuWhy::Settling,
    ] {
        assert_eq!(NdiPick::Cpu(why).source(), NdiSource::Cpu);
    }
    assert_eq!(
        [
            CpuWhy::Setting.as_str(),
            CpuWhy::NoReadback.as_str(),
            CpuWhy::Stale.as_str(),
            CpuWhy::Settling.as_str()
        ],
        ["setting", "no_readback", "stale", "settling"]
    );
    assert_eq!(
        (delay_slots(NdiSource::Gpu), delay_slots(NdiSource::Cpu)),
        (1, 0)
    );
    assert_eq!(
        (fourcc(NdiSource::Gpu), fourcc(NdiSource::Cpu)),
        ("UYVY", "NV12")
    );
}

/// #279 ROZHODNUTÉ 6232669201 item 3: only a fallback from the GPU (not
/// the setting's) arms the gate; armed, 29 on-time readbacks in a row hold
/// the canvas and the 30th lets the GPU through; the GPU disarms it.
#[test]
fn a_fallback_arms_the_gate_and_30_on_time_readbacks_let_the_gpu_through() {
    assert_eq!(NDI_RETURN_ON_TIME, 30);
    let mut gate = ReturnGate::default();
    assert!(!gate.counting());
    assert_eq!(gate.admit(NdiPick::Gpu), NdiPick::Gpu, "not armed");
    gate.after(NdiSource::Cpu, NdiPick::Cpu(CpuWhy::NoReadback));
    assert!(
        !gate.counting(),
        "the canvas before the GPU was ever on (a first entry) arms nothing"
    );
    for why in [CpuWhy::NoReadback, CpuWhy::Stale] {
        let mut gate = ReturnGate::default();
        gate.after(NdiSource::Gpu, NdiPick::Cpu(why));
        assert!(gate.counting(), "{why:?}");
        assert_eq!(gate.admit(NdiPick::Gpu), NdiPick::Cpu(CpuWhy::Settling));
    }
    let mut gate = ReturnGate::default();
    gate.after(NdiSource::Gpu, NdiPick::Cpu(CpuWhy::Setting));
    assert!(!gate.counting(), "the setting's canvas never arms it");

    let mut gate = ReturnGate::default();
    gate.after(NdiSource::Gpu, NdiPick::Cpu(CpuWhy::Stale));
    for n in 1..NDI_RETURN_ON_TIME {
        gate.observe(true);
        assert_eq!(
            gate.admit(NdiPick::Gpu),
            NdiPick::Cpu(CpuWhy::Settling),
            "{n} in a row"
        );
        gate.after(NdiSource::Cpu, NdiPick::Cpu(CpuWhy::Settling));
        assert!(gate.counting(), "the canvas keeps it armed");
    }
    gate.observe(true);
    assert_eq!(gate.admit(NdiPick::Gpu), NdiPick::Gpu, "30 in a row");
    assert_eq!(
        gate.admit(NdiPick::Cpu(CpuWhy::Stale)),
        NdiPick::Cpu(CpuWhy::Stale),
        "a canvas pick passes as it is"
    );
    gate.after(NdiSource::Cpu, NdiPick::Gpu);
    assert!(!gate.counting(), "back on the GPU: disarmed");
}

#[test]
fn a_late_readback_starts_the_gate_s_count_again() {
    let mut gate = ReturnGate::default();
    gate.after(NdiSource::Gpu, NdiPick::Cpu(CpuWhy::Stale));
    for _ in 0..29 {
        gate.observe(true);
    }
    gate.observe(false);
    for _ in 0..29 {
        gate.observe(true);
    }
    assert_eq!(
        gate.admit(NdiPick::Gpu),
        NdiPick::Cpu(CpuWhy::Settling),
        "29 since the late one"
    );
    gate.observe(true);
    assert_eq!(gate.admit(NdiPick::Gpu), NdiPick::Gpu);
    gate.after(NdiSource::Cpu, NdiPick::Cpu(CpuWhy::Setting));
    gate.observe(true);
    assert!(
        !gate.counting(),
        "the setting's canvas disarms it, and a disarmed gate counts nothing"
    );
}

/// NDI's switch lines (a `StateLog` of the source, as the `SP-program`
/// sender keeps it): a change of source is said once, at most once per
/// 5 s of the timeline; one held back is said by the first boundary after
/// the window, as the source is then, with the boundaries held back; a
/// change that went back inside the window says nothing.
#[test]
fn a_switch_is_said_once_and_one_held_back_after_the_window() {
    assert_eq!(NDI_SWITCH_LOG_EVERY_100NS, 50_000_000);
    let every = NDI_SWITCH_LOG_EVERY_100NS;
    let (gpu, cpu) = (NdiSource::Gpu, NdiSource::Cpu);
    let mut log = StateLog::<NdiSource>::default();
    assert_eq!(
        log.observe(b(0), every, cpu),
        None,
        "the canvas at the start: nothing to say"
    );
    assert_eq!(log.observe(b(1), every, gpu), Some(0));
    assert_eq!(log.observe(b(2), every, gpu), None, "no change");
    assert_eq!(log.observe(b(3), every, cpu), None, "inside the window");
    assert_eq!(log.observe(b(4), every, cpu), None);
    let after = b(1) + every;
    assert_eq!(
        log.observe(after, every, cpu),
        Some(2),
        "the source as it is now, two boundaries held back"
    );
    assert_eq!(*log.logged(), cpu);
    assert_eq!(log.observe(after + 1, every, gpu), None, "held back");
    assert_eq!(
        log.observe(after + 2, every, cpu),
        None,
        "back where the log is: nothing to say"
    );
    let later = after + every;
    assert_eq!(log.observe(later, every, cpu), None, "still there");
    assert_eq!(log.observe(later + 1, every, gpu), Some(1));
    assert_eq!(*log.logged(), gpu);
}

#[test]
fn the_uyvy_black_is_limited_range_black_in_every_pair() {
    assert_eq!(UYVY_BLACK_PAIR, [128, 16, 128, 16]);
    let black = [0, 0, 0, 255];
    assert_eq!(
        sp_gpu::uyvy::pack_pair(black, black),
        UYVY_BLACK_PAIR,
        "the GPU pass's own conversion of a black pair (one rule)"
    );
    let black = black_uyvy(6, 3);
    assert_eq!(black.len(), 6 * 3 * 2, "3 pairs a row, 3 rows");
    assert!(black.chunks(4).all(|pair| pair == UYVY_BLACK_PAIR));
    assert_eq!(black_uyvy(1920, 1080).len(), 1920 * 1080 * 2);
}

#[test]
fn a_uyvy_picture_is_two_bytes_a_pixel() {
    assert_eq!(uyvy_len(6, 3), 36);
    assert_eq!(uyvy_len(1920, 1080), 4_147_200);
}

#[test]
fn the_uyvy_black_is_built_once_per_size() {
    let mut cache = BlackUyvy::default();
    let first = cache.get(6, 3);
    assert!(cache.get(6, 3).ptr_eq(&first), "the same allocation again");
    let other = cache.get(8, 2);
    assert_eq!(other.len(), 8 * 2 * 2, "another size is built anew");
    assert!(!other.ptr_eq(&first));
}

#[test]
fn the_status_counts_what_both_threads_recorded() {
    let feed = NdiFeed::default();
    let fresh = feed.status(NdiSource::Cpu);
    assert_eq!(
        (fresh.setting, fresh.source, fresh.delay_slots, fresh.fourcc),
        ("cpu", "cpu", 0, "NV12"),
        "before any boundary"
    );
    assert_eq!(
        (
            fresh.repeats,
            fresh.switches,
            fresh.readback_us_p99,
            fresh.uyvy_us_p99
        ),
        (0, 0, 0, 0)
    );
    feed.push(b(1), pic(1));
    assert_eq!(feed.newest_stamp(), Some(b(1)));
    assert!(holds(feed.take(b(1)), 1));
    feed.push(b(1), pic(1));
    for k in 2..=6u8 {
        feed.push(b(i64::from(k)), pic(k));
    }
    feed.record_boundary(NdiSource::Gpu, true, false);
    feed.record_boundary(NdiSource::Gpu, false, true);
    feed.record_boundary(NdiSource::Gpu, false, true);
    feed.record_readback(700, Some(40));
    feed.record_readback(900, None);
    let status = feed.status(NdiSource::Gpu);
    assert_eq!(
        (
            status.setting,
            status.source,
            status.delay_slots,
            status.fourcc
        ),
        ("gpu", "gpu", 1, "UYVY")
    );
    assert_eq!(
        (
            status.repeats,
            status.switches,
            status.readback_late,
            status.feed_coalesced
        ),
        (2, 1, 1, 2),
        "two repeats, one switch, boundary 1 late, 2..=6 over a depth of 3"
    );
    assert_eq!(
        (status.readback_us_p99, status.uyvy_us_p99),
        (900, 40),
        "a readback with no pass time adds none"
    );
    feed.record_boundary(NdiSource::Cpu, true, false);
    let status = feed.status(NdiSource::Cpu);
    assert_eq!((status.source, status.switches), ("cpu", 2));
}

/// The windows keep the last `MAX_STAT_WINDOW` figures (MAX's window):
/// the oldest goes.
#[test]
fn the_readback_window_keeps_the_last_900() {
    assert_eq!(MAX_STAT_WINDOW, 900);
    let feed = NdiFeed::default();
    feed.record_readback(5_000, Some(5_000));
    feed.record_readback(1, Some(1));
    assert_eq!(
        feed.status(NdiSource::Gpu).readback_us_p99,
        5_000,
        "two figures: the p99 is the larger"
    );
    for _ in 0..30 {
        feed.record_readback(9_000, Some(9_000));
    }
    for _ in 0..MAX_STAT_WINDOW - 30 {
        feed.record_readback(2, Some(2));
    }
    let status = feed.status(NdiSource::Gpu);
    assert_eq!(
        (status.readback_us_p99, status.uyvy_us_p99),
        (9_000, 9_000),
        "the 30 large ones are still in the window"
    );
    for _ in 0..30 {
        feed.record_readback(3, Some(3));
    }
    let status = feed.status(NdiSource::Gpu);
    assert_eq!(
        (status.readback_us_p99, status.uyvy_us_p99),
        (3, 3),
        "30 newer ones pushed them out"
    );
}
