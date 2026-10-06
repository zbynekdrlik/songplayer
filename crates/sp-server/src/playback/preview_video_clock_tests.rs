//! Unit tests for the preview video feeder's monotonic frame schedule (#221).
//! Pure: every time is an explicit µs value, nothing sleeps.

use super::*;

/// Where every schedule in these tests starts (µs on the feeder's clock); not
/// 0 or 1, so a `Some(0)` / `Some(1)` origin cannot pass.
const T0: u64 = 7_000_000;

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

/// A clock whose frame 0 ("A") was written at `T0`.
fn started() -> VideoClock<&'static str> {
    let mut clock = VideoClock::new();
    clock.offer("A", T0);
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock
}

/// One second of a source at `period_us` (canvas `j` arrives at
/// `T0 + j × period_us`), the feeder waking every millisecond: every write.
fn one_second_of(clock: &mut VideoClock<u64>, period_us: u64) -> Vec<(u64, u64)> {
    let mut writes = Vec::new();
    let mut j: u64 = 0;
    for ms in 0..1_000 {
        while j * period_us <= ms * 1_000 {
            clock.offer(j, T0 + j * period_us);
            j += 1;
        }
        writes.extend(writes_at(clock, T0 + ms * 1_000, 3));
    }
    writes
}

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
    // frame 0 at its arrival.
    assert_eq!(clock.offer("A", T0), None);
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

#[test]
fn the_first_canvas_is_written_at_once_and_a_slot_waits_for_a_new_picture() {
    let mut clock = VideoClock::new();
    clock.offer("A", T0);
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
    // A new picture takes slot 1, decided half a slot after its time (60 ms).
    clock.offer("B", T0 + 5_000);
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
    // re-encoded differently every few frames). The next picture fills every
    // slot decided before it arrived with the last one, then takes its own
    // slot: the video timeline still counts every slot of the monotonic clock.
    let mut clock = started();
    for k in 1..=10 {
        assert_eq!(
            clock.take_due(T0 + k * FRAME_US + DECIDE_LATE_US),
            None,
            "slot {k} of the pause writes nothing"
        );
    }
    // B arrives at 430 ms: slots 1-10 were decided before it (the last at
    // 420 ms), they get A; B's own slot 11 is decided at 460 ms.
    clock.offer("B", T0 + 430_000);
    assert_eq!(writes_at(&mut clock, T0 + 430_000, 4), vec![("A", 10)]);
    assert!(writes_at(&mut clock, T0 + 459_999, 4).is_empty());
    assert_eq!(writes_at(&mut clock, T0 + 460_000, 4), vec![("B", 1)]);
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 12,
            repeated: 10,
            skipped: 0,
            max_burst: 10,
        }
    );
}

#[test]
fn a_30_fps_source_fills_exactly_25_slots_a_second_with_the_nearest_pictures() {
    // Canvas j arrives at j × 33.333 ms. Each slot gets the newest canvas
    // that arrived by its decision (k × 40 ms + 20 ms): the nearest picture;
    // the ones in between are skipped (back to the pool).
    let mut clock = VideoClock::new();
    let writes = one_second_of(&mut clock, 33_333);
    let expected: Vec<(u64, u64)> = [
        0, 1, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 21, 22, 23, 24, 25, 27, 28, 29,
    ]
    .iter()
    .map(|&id| (id, 1))
    .collect();
    assert_eq!(writes, expected);
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
fn a_24_fps_source_fills_a_slot_it_misses_with_the_last_picture() {
    // Canvas j arrives at j × 41.667 ms, slower than the 25 slots a second.
    // Canvas 12 arrives at 500.004 ms, 4 µs after slot 12's decision: slot 12
    // repeats canvas 11 and canvas 12 takes slot 13. Every picture lands
    // within about half a slot of its slot's time (−18..+22 ms).
    let mut clock = VideoClock::new();
    let writes = one_second_of(&mut clock, 41_667);
    let expected: Vec<(u64, u64)> = (0..=11)
        .chain([11])
        .chain(12..=23)
        .map(|id| (id, 1))
        .collect();
    assert_eq!(writes, expected);
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 25,
            repeated: 1,
            skipped: 0,
            max_burst: 1,
        }
    );
}

#[test]
fn the_newest_canvas_takes_the_slot_and_a_replaced_one_comes_back() {
    let mut clock = started();
    // A was written and stays the gap's fill picture: offering B replaces
    // nothing.
    assert_eq!(clock.offer("B", T0 + 1_000), None);
    assert_eq!(clock.stats().skipped, 0);
    // B never reached a slot: replacing it skips it.
    assert_eq!(clock.offer("C", T0 + 2_000), Some("B"));
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
fn a_stalled_feeder_still_puts_a_picture_on_the_slot_it_arrived_for() {
    // B arrived at 10 ms, in time for slot 1, but the feeder's write blocked
    // until 220 ms: B still takes slot 1. Slots 2-5 passed with nothing new;
    // C (230 ms) fills them with B, then takes slot 6 at 260 ms.
    let mut clock = started();
    clock.offer("B", T0 + 10_000);
    assert_eq!(writes_at(&mut clock, T0 + 220_000, 4), vec![("B", 1)]);
    clock.offer("C", T0 + 230_000);
    assert_eq!(writes_at(&mut clock, T0 + 230_000, 4), vec![("B", 4)]);
    assert_eq!(writes_at(&mut clock, T0 + 260_000, 4), vec![("C", 1)]);
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 7,
            repeated: 4,
            skipped: 0,
            max_burst: 4,
        }
    );
}

#[test]
fn a_picture_arriving_at_a_decision_takes_that_slot_and_one_us_later_the_next() {
    // Slot 1 is decided at 60 ms. B arriving exactly then takes it.
    let mut clock = started();
    clock.offer("B", T0 + 60_000);
    assert_eq!(writes_at(&mut clock, T0 + 60_000, 4), vec![("B", 1)]);
    // C arriving in the same instant, after B was written, takes slot 2
    // (never one already written).
    clock.offer("C", T0 + 60_000);
    assert!(writes_at(&mut clock, T0 + 60_000, 4).is_empty());
    assert_eq!(writes_at(&mut clock, T0 + 100_000, 4), vec![("C", 1)]);

    // B arriving 1 µs after slot 1's decision is too late for it: slot 1
    // gets A, B waits for slot 2 (100 ms).
    let mut clock = started();
    clock.offer("B", T0 + 60_001);
    assert_eq!(writes_at(&mut clock, T0 + 60_001, 4), vec![("A", 1)]);
    assert!(writes_at(&mut clock, T0 + 99_999, 4).is_empty());
    assert_eq!(writes_at(&mut clock, T0 + 100_000, 4), vec![("B", 1)]);
}

#[test]
fn the_wait_runs_to_the_next_decision_and_is_zero_while_a_fill_is_due() {
    let mut clock = started();
    clock.offer("B", T0);
    assert_eq!(clock.wait_us(T0), 60_000, "slot 1 is decided at 60 ms");
    assert_eq!(clock.wait_us(T0 + 60_000), 0, "due");
    assert_eq!(clock.wait_us(T0 + 70_000), 0, "overdue, not negative");
    assert_eq!(clock.take_due(T0 + 70_000), Some((&"B", 1)));
    clock.offer("C", T0 + 70_000);
    assert_eq!(clock.wait_us(T0 + 70_000), 30_000, "slot 2 at 100 ms");
    assert_eq!(writes_at(&mut clock, T0 + 100_000, 4), vec![("C", 1)]);
    // D arrives after slot 3's decision (140 ms): a fill is due at once.
    clock.offer("D", T0 + 150_000);
    assert_eq!(clock.wait_us(T0 + 150_000), 0);
}

#[test]
fn a_gap_of_ten_seconds_is_filled_and_a_longer_one_restarts_the_encoder() {
    // B arrives 10.04 s after frame 0: slots 1-250 were decided before it, the
    // bound (MAX_GAP_FILL_SLOTS, 10 s), written at once; B takes slot 251.
    let mut clock = started();
    clock.offer("B", T0 + 10_040_000);
    assert!(!clock.must_restart(), "250 slots are filled");
    assert_eq!(
        writes_at(&mut clock, T0 + 10_040_000, 4),
        vec![("A", MAX_GAP_FILL_SLOTS)]
    );
    assert_eq!(writes_at(&mut clock, T0 + 10_060_000, 4), vec![("B", 1)]);
    assert_eq!(clock.stats().written, 252);

    // One slot more: nothing is written, the feeder ends the encoder run.
    let mut clock = started();
    clock.offer("B", T0 + 10_080_000);
    assert!(clock.must_restart(), "251 slots are too many");
    assert!(writes_at(&mut clock, T0 + 10_080_000, 4).is_empty());
    assert_eq!(clock.stats().written, 1, "nothing was written");
}

#[test]
fn no_restart_without_a_pending_picture_or_before_the_first() {
    let mut clock = VideoClock::new();
    assert!(!clock.must_restart(), "no canvas yet");
    clock.offer("A", T0);
    assert!(!clock.must_restart(), "frame 0 is never a gap");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert!(
        !clock.must_restart(),
        "a pause with no new picture waits, whatever its length"
    );
    clock.offer("B", T0 + 5_000_000);
    assert!(!clock.must_restart(), "a 5 s gap is filled");
}

#[test]
fn the_replaced_fill_picture_goes_back_to_the_pool_once() {
    let mut clock = started();
    assert_eq!(clock.released(), None, "frame 0 replaced nothing");
    clock.offer("B", T0 + 1_000);
    assert_eq!(clock.take_due(T0 + 60_000), Some((&"B", 1)));
    assert_eq!(clock.released(), Some("A"), "B is the fill picture now");
    assert_eq!(clock.released(), None, "handed back once");
}

#[test]
fn take_stats_starts_a_new_max_burst_window() {
    let mut clock = started();
    clock.offer("B", T0 + 10_000);
    assert_eq!(writes_at(&mut clock, T0 + 220_000, 4), vec![("B", 1)]);
    clock.offer("C", T0 + 230_000);
    assert_eq!(writes_at(&mut clock, T0 + 230_000, 4), vec![("B", 4)]);
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
