//! Unit tests for the preview video feeder's monotonic frame schedule (#221).
//! Pure: every time is an explicit µs value, nothing sleeps.

use super::*;

/// Where every schedule in these tests starts (µs on the feeder's clock); not
/// 0 or 1, so a `Some(0)` / `Some(1)` origin cannot pass.
const T0: u64 = 7_000_000;

#[test]
fn one_slot_is_a_25th_of_a_second() {
    let fps = std::hint::black_box(PREVIEW_FPS);
    assert_eq!(fps, 25, "the encoder's -framerate, -r and GOP");
    assert_eq!(
        FRAME_US * fps,
        1_000_000,
        "25 slots fill one second exactly"
    );
}

#[test]
fn nothing_is_due_and_the_schedule_does_not_start_before_a_canvas() {
    let mut clock = VideoClock::<&str>::new();
    assert_eq!(
        clock.take_due(T0 - 2_000_000),
        None,
        "no canvas, nothing to write"
    );
    assert_eq!(
        clock.wait_us(T0),
        IDLE_POLL_US,
        "it only polls for shutdown"
    );
    assert_eq!(clock.start_us(), None);
    assert_eq!(clock.stats(), VideoClockStats::default());

    // The empty call above did not start the schedule: the first canvas is
    // frame 0 when it arrives, not 50 slots late.
    assert_eq!(clock.offer("A"), None);
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert_eq!(
        clock.start_us(),
        Some(T0),
        "frame 0 is the video timeline's origin"
    );
}

#[test]
fn the_default_clock_is_an_empty_one() {
    let clock = VideoClock::<&str>::default();
    assert_eq!(clock.wait_us(T0), IDLE_POLL_US);
    assert_eq!(clock.start_us(), None);
}

/// Every write `take_due` hands out at `now`, at most `limit` of them (a
/// bounded loop: a mutant that never stops answering fails, it cannot hang).
fn writes_at<T: Copy>(clock: &mut VideoClock<T>, now: u64, limit: usize) -> Vec<(T, u64)> {
    let mut out = Vec::new();
    for _ in 0..limit {
        match clock.take_due(now) {
            Some((frame, n)) => out.push((*frame, n)),
            None => break,
        }
    }
    out
}

#[test]
fn the_first_canvas_is_written_at_once_and_a_slot_waits_for_a_new_picture() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(
        clock.wait_us(T0),
        0,
        "a canvas waits for its first slot: now"
    );
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    // #221 A1: nothing new, nothing written (the encoder starves, the picture
    // holds pixel-exact): the feeder only polls.
    assert_eq!(clock.wait_us(T0), IDLE_POLL_US);
    assert_eq!(clock.take_due(T0 + 5 * FRAME_US), None, "no repeat");
    // A new picture takes slot 1, decided half a slot after it (60 ms), so
    // the canvas a slot gets is the one nearest its time, not up to 40 ms old.
    clock.offer("B");
    assert_eq!(clock.wait_us(T0 + 5_000), 55_000);
    assert_eq!(clock.take_due(T0 + 59_999), None, "not before the decision");
    assert_eq!(
        clock.take_due(T0 + 60_000),
        Some((&"B", 1)),
        "exactly at it"
    );
    assert_eq!(clock.wait_us(T0 + 60_000), IDLE_POLL_US);
}

#[test]
fn a_pause_writes_nothing_and_the_next_picture_fills_the_gap() {
    // #221 A1 (ROZHODNUTÉ 6008679010): a pause sends NOTHING, so the encoder
    // starves and the picture freezes pixel-exact (a repeated canvas is
    // re-encoded differently every few frames). The next picture fills the
    // missed slots with the last one, then takes its own slot: the video
    // timeline still counts every slot of the monotonic clock.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    for k in 1..=10 {
        assert_eq!(
            clock.take_due(T0 + k * FRAME_US + 20_000),
            None,
            "slot {k} of the pause writes nothing"
        );
    }
    clock.offer("B");
    // 430 ms: slots 1-10 are due; 1-9 get the last picture, 10 the new one.
    assert_eq!(
        writes_at(&mut clock, T0 + 10 * FRAME_US + 30_000, 4),
        vec![("A", 9), ("B", 1)]
    );
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 11,
            repeated: 9,
            skipped: 0,
            max_burst: 9,
        }
    );
}

#[test]
fn a_30_fps_source_fills_exactly_25_slots_a_second_with_the_nearest_pictures() {
    // Canvas j arrives at j × 33.333 ms; the feeder wakes every millisecond.
    // Slot k is decided at k × 40 ms + 20 ms with the newest canvas, so each
    // slot gets the picture nearest its time; the ones in between are
    // skipped (they go back to the pool).
    let mut clock = VideoClock::new();
    let mut ids = Vec::new();
    let mut j: u64 = 0;
    for ms in 0..1_000 {
        while j * 33_333 <= ms * 1_000 {
            clock.offer(j);
            j += 1;
        }
        ids.extend(writes_at(&mut clock, T0 + ms * 1_000, 3));
    }
    let expected: Vec<(u64, u64)> = [
        0, 1, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 21, 22, 23, 24, 25, 27, 28, 29,
    ]
    .iter()
    .map(|&id| (id, 1))
    .collect();
    assert_eq!(ids, expected);
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 25,
            repeated: 0,
            skipped: 5,
            max_burst: 1,
        }
    );
}

#[test]
fn the_newest_canvas_takes_the_slot_and_a_replaced_one_comes_back() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    // A was written and stays the gap's fill picture: offering B replaces
    // nothing.
    assert_eq!(clock.offer("B"), None);
    assert_eq!(clock.stats().skipped, 0);
    // B never reached a slot: replacing it skips it.
    assert_eq!(clock.offer("C"), Some("B"));
    assert_eq!(clock.stats().skipped, 1);
    assert_eq!(
        clock.take_due(T0 + 60_000),
        Some((&"C", 1)),
        "the newest one"
    );
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 2,
            repeated: 0,
            skipped: 1,
            max_burst: 1,
        }
    );
}

#[test]
fn a_stalled_feeder_fills_the_missed_slots_with_the_last_written_picture() {
    // The feeder's write blocked for 220 ms while B arrived: slots 1-4 passed
    // unwritten. They get A, the last picture the encoder saw; B takes slot 5.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    assert_eq!(
        writes_at(&mut clock, T0 + 220_000, 4),
        vec![("A", 4), ("B", 1)]
    );
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 6,
            repeated: 4,
            skipped: 0,
            max_burst: 4,
        }
    );
}

#[test]
fn the_wait_runs_to_the_next_decision_and_never_below_zero() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    assert_eq!(clock.wait_us(T0), 60_000, "slot 1 is decided at 60 ms");
    assert_eq!(clock.wait_us(T0 + 60_000), 0, "due");
    assert_eq!(clock.wait_us(T0 + 70_000), 0, "overdue, not negative");
    assert_eq!(clock.take_due(T0 + 70_000), Some((&"B", 1)));
    clock.offer("C");
    assert_eq!(clock.wait_us(T0 + 70_000), 30_000, "slot 2 at 100 ms");
}

#[test]
fn a_gap_of_ten_seconds_is_filled_and_a_longer_one_restarts_the_encoder() {
    // 10.06 s after frame 0: slots 1-250 missed, slot 251 is the new picture's.
    // The bound (MAX_GAP_FILL_SLOTS, 10 s) is written at once.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    let at_bound = T0 + 251 * FRAME_US + DECIDE_LATE_US;
    assert!(!clock.must_restart(at_bound), "250 slots are filled");
    assert_eq!(
        writes_at(&mut clock, at_bound, 4),
        vec![("A", MAX_GAP_FILL_SLOTS), ("B", 1)]
    );
    assert_eq!(clock.stats().written, 252);

    // One slot more: nothing is written, the feeder ends the encoder run.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    let past_bound = at_bound + FRAME_US;
    assert!(clock.must_restart(past_bound), "251 slots are too many");
    assert!(writes_at(&mut clock, past_bound, 4).is_empty());
    assert_eq!(clock.stats().written, 1, "nothing was written");
}

#[test]
fn no_restart_without_a_pending_picture_or_before_the_first() {
    let mut clock = VideoClock::new();
    assert!(!clock.must_restart(T0), "no canvas yet");
    clock.offer("A");
    assert!(
        !clock.must_restart(T0 + 600 * FRAME_US),
        "frame 0 is never a gap"
    );
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert!(
        !clock.must_restart(T0 + 600 * FRAME_US),
        "a pause with no new picture waits, whatever its length"
    );
}

#[test]
fn the_replaced_fill_picture_goes_back_to_the_pool_once() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert_eq!(clock.released(), None, "frame 0 replaced nothing");
    clock.offer("B");
    assert_eq!(clock.take_due(T0 + 60_000), Some((&"B", 1)));
    assert_eq!(clock.released(), Some("A"), "B is the fill picture now");
    assert_eq!(clock.released(), None, "handed back once");
}

#[test]
fn take_stats_starts_a_new_max_burst_window() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    assert_eq!(
        writes_at(&mut clock, T0 + 220_000, 4),
        vec![("A", 4), ("B", 1)]
    );
    assert_eq!(clock.take_stats().max_burst, 4, "the window's gap");
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 6,
            repeated: 4,
            skipped: 0,
            max_burst: 0,
        },
        "the totals stay, the window starts over"
    );
}
