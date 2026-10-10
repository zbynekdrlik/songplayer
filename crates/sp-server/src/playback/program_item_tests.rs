//! #228: the item on `SP-program` (`ItemTrack`, `grid_frame`) and the
//! shared record (`ProgramItem`). Wired via `#[cfg(test)] #[path =
//! "program_item_tests.rs"] mod tests;` in `program_item.rs`.

use super::*;

/// A boundary's wire stamp (UTC 100 ns), and a grid slot.
const T0: i64 = 17_600_000_000_000_000;
const SLOT: i64 = 333_333;

/// The test item's playlist and video.
const PID: i64 = 5;
const VIDEO: i64 = 42;

fn mark(seq: u64, start_ms: u64) -> Option<ItemMark> {
    Some(ItemMark {
        seq,
        video_id: VIDEO,
        start_ms,
    })
}

#[test]
fn grid_frame_rounds_the_media_time_to_the_nearest_30_fps_frame() {
    assert_eq!(grid_frame(0), 0);
    assert_eq!(grid_frame(330_000), 1, "33 ms is frame 1");
    assert_eq!(grid_frame(660_000), 2, "66 ms is frame 2");
    assert_eq!(grid_frame(166_666), 0, "just under half a frame");
    assert_eq!(grid_frame(166_667), 1, "just over half a frame");
    assert_eq!(grid_frame(1_279_660_000), 3839, "the clip's last frame");
    assert_eq!(grid_frame(-1_000), 0, "before frame 0");
    assert_eq!(grid_frame(1_500_000_000_000_000), u32::MAX);
}

#[test]
fn a_frame_before_any_mark_is_shown_but_names_no_item() {
    let mut track = ItemTrack::default();
    let shown = track.observe(Some(PID), Some(0), T0, None);
    assert_eq!(
        shown,
        Some(ItemFrame {
            frame: 0,
            media_100ns: 0,
            wire_100ns: T0,
        })
    );
    assert_eq!(track.status(), None, "no mark named the video yet");
}

#[test]
fn a_marked_item_counts_its_frames_from_where_its_play_started() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, mark(1, 0));
    assert_eq!(
        track.status(),
        Some(ItemStatus {
            playlist_id: PID,
            video_id: VIDEO,
            started_at_utc_ns: T0 * 100,
            position_ms: 0,
            frame: 0,
            frame_utc_ns: T0 * 100,
        })
    );
    let shown = track.observe(Some(PID), Some(330_000), T0 + SLOT, mark(1, 0));
    assert_eq!(shown.map(|s| s.frame), Some(1));
    assert_eq!(
        track.status(),
        Some(ItemStatus {
            playlist_id: PID,
            video_id: VIDEO,
            started_at_utc_ns: T0 * 100,
            position_ms: 33,
            frame: 1,
            frame_utc_ns: (T0 + SLOT) * 100,
        })
    );
}

/// A starvation repeat shows the same frame again (a hold the receiver
/// sees); neither it nor the frame after it moves `started_at`.
#[test]
fn a_repeated_frame_keeps_its_index_and_the_start() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, mark(1, 0));
    track.observe(Some(PID), Some(330_000), T0 + SLOT, mark(1, 0));
    let repeat = track.observe(Some(PID), Some(330_000), T0 + 2 * SLOT, mark(1, 0));
    assert_eq!(repeat.map(|s| s.frame), Some(1));
    let next = track.observe(Some(PID), Some(660_000), T0 + 3 * SLOT, mark(1, 0));
    assert_eq!(next.map(|s| s.frame), Some(2));
    let status = track.status().unwrap();
    assert_eq!(status.started_at_utc_ns, T0 * 100);
    assert_eq!(status.frame_utc_ns, (T0 + 3 * SLOT) * 100);
}

/// A resume at 60 s (its `Started` reports 60 000 ms): the pts start at 0
/// again, the frame index counts from the item's frame 0.
#[test]
fn a_play_from_a_position_counts_from_the_item_s_frame_0() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, mark(1, 0));
    track.observe(Some(PID), Some(660_000), T0 + 2 * SLOT, mark(1, 0));
    let t1 = T0 + 1_000 * SLOT;
    let shown = track.observe(Some(PID), Some(0), t1, mark(2, 60_000));
    assert_eq!(
        shown,
        Some(ItemFrame {
            frame: 1800,
            media_100ns: 600_000_000,
            wire_100ns: t1,
        })
    );
    let status = track.status().unwrap();
    assert_eq!(status.position_ms, 60_000);
    assert_eq!(status.started_at_utc_ns, (t1 - 600_000_000) * 100);
}

/// The `Started` is handled after the song's first frame already went out
/// (the race the trace's song mark has too): the mark arrives while the pts
/// go on, and `started_at` is set again from it.
#[test]
fn a_mark_that_arrives_late_sets_the_start_again() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, None);
    let shown = track.observe(Some(PID), Some(330_000), T0 + SLOT, mark(1, 0));
    assert_eq!(shown.map(|s| s.frame), Some(1));
    let status = track.status().unwrap();
    assert_eq!(status.started_at_utc_ns, (T0 + SLOT - 330_000) * 100);
}

/// The item loops (its playlist's mode): the pts go back to 0 with no new
/// mark yet, and its media time 0 is on the wire again.
#[test]
fn a_loop_back_to_frame_0_sets_the_start_again() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, mark(1, 0));
    track.observe(Some(PID), Some(330_000), T0 + SLOT, mark(1, 0));
    let t2 = T0 + 3_840 * SLOT;
    let shown = track.observe(Some(PID), Some(0), t2, mark(1, 0));
    assert_eq!(shown.map(|s| s.frame), Some(0));
    assert_eq!(track.status().unwrap().started_at_utc_ns, t2 * 100);
}

/// A boundary with no item frame (paused, a fill, the pre-roll) keeps the
/// item as it was; a boundary of another source ends it, and coming back
/// applies the source's mark afresh.
#[test]
fn another_source_ends_the_item_and_a_frameless_boundary_keeps_it() {
    let mut track = ItemTrack::default();
    track.observe(Some(PID), Some(0), T0, mark(1, 0));
    track.observe(Some(PID), Some(330_000), T0 + SLOT, mark(1, 0));
    let before = track.status();
    assert_eq!(
        track.observe(Some(PID), None, T0 + 2 * SLOT, mark(1, 0)),
        None
    );
    assert_eq!(track.status(), before, "a paused item stays on its frame");

    assert!(track.observe(Some(-2), None, T0 + 3 * SLOT, None).is_none());
    assert_eq!(track.status(), None, "Blank on program: no item");
    track.observe(Some(6), Some(0), T0 + 4 * SLOT, None);
    assert_eq!(track.status(), None, "another playlist with no mark");
    track.observe(None, Some(0), T0 + 5 * SLOT, None);
    assert_eq!(track.status(), None, "nothing selected");

    let back = T0 + 6 * SLOT;
    track.observe(Some(PID), Some(990_000), back, mark(1, 0));
    let status = track.status().unwrap();
    assert_eq!(status.frame, 3);
    assert_eq!(status.started_at_utc_ns, (back - 990_000) * 100);
}

#[test]
fn the_burn_starts_off_and_follows_its_switch() {
    let item = ProgramItem::default();
    assert!(!item.burn_on());
    item.set_burn(true);
    assert!(item.burn_on());
    item.set_burn(false);
    assert!(!item.burn_on());
    assert_eq!(item.burned(), 0);
    item.count_burned();
    item.count_burned();
    assert_eq!(item.burned(), 2);
}

#[test]
fn each_mark_replaces_the_playlist_s_last_one_with_a_newer_seq() {
    let item = ProgramItem::default();
    assert_eq!(item.mark_of(PID), None);
    item.mark(PID, VIDEO, 0);
    assert_eq!(
        item.mark_of(PID),
        Some(ItemMark {
            seq: 1,
            video_id: VIDEO,
            start_ms: 0,
        })
    );
    item.mark(PID, 43, 1_000);
    item.mark(6, VIDEO, 0);
    assert_eq!(
        item.mark_of(PID),
        Some(ItemMark {
            seq: 2,
            video_id: 43,
            start_ms: 1_000,
        })
    );
    assert_eq!(item.mark_of(6).map(|m| m.seq), Some(3));
    assert_eq!(item.mark_of(7), None);
}

/// The sender never waits on the engine or the API: a held lock = no new
/// mark, and a publish skipped, this boundary.
#[test]
fn the_sender_s_reads_and_writes_never_wait() {
    let item = ProgramItem::default();
    item.mark(PID, VIDEO, 0);
    {
        let _held = item.marks.lock().unwrap();
        assert_eq!(item.mark_of(PID), None);
    }
    assert!(item.mark_of(PID).is_some());

    let status = ItemStatus {
        playlist_id: PID,
        video_id: VIDEO,
        started_at_utc_ns: 1,
        position_ms: 2,
        frame: 3,
        frame_utc_ns: 4,
    };
    item.publish(Some(status));
    assert_eq!(item.on_air(), Some(status));
    {
        let _held = item.on_air.lock().unwrap();
        item.publish(None);
    }
    assert_eq!(item.on_air(), Some(status), "the held boundary skipped");
    item.publish(None);
    assert_eq!(item.on_air(), None);
}
