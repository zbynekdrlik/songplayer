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

#[test]
fn the_first_canvas_is_written_at_once_and_the_next_slot_is_one_frame_later() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(
        clock.wait_us(T0),
        0,
        "a canvas waits for its first slot: now"
    );
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert_eq!(clock.wait_us(T0), FRAME_US, "slot 1 is one frame later");
    assert_eq!(clock.wait_us(T0 + 10_000), 30_000);
    assert_eq!(
        clock.take_due(T0 + FRAME_US - 1),
        None,
        "not before its slot"
    );
    assert_eq!(
        clock.take_due(T0 + FRAME_US),
        Some((&"A", 1)),
        "exactly at it"
    );
    assert_eq!(clock.wait_us(T0 + FRAME_US), FRAME_US);
}

#[test]
fn exactly_25_frames_per_second_of_the_monotonic_clock() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    // A feeder that wakes every millisecond for one second writes 25 frames,
    // one at a time.
    let mut frames = 0;
    for ms in 0..1_000 {
        if let Some((_, n)) = clock.take_due(T0 + ms * 1_000) {
            assert_eq!(n, 1, "one frame per slot at {ms} ms");
            frames += n;
        }
    }
    assert_eq!(frames, 25);
    assert_eq!(
        clock.take_due(T0 + 1_000_000),
        Some((&"A", 1)),
        "the 26th starts second 2"
    );
    let stats = clock.stats();
    assert_eq!(stats.written, 26);
    assert_eq!(stats.max_burst, 1);
}

#[test]
fn a_pause_repeats_the_last_canvas_on_every_slot() {
    // The decode stops offering (a pause): the stream keeps flowing with the
    // last picture, frozen on the very next slot.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    clock.offer("B");
    assert_eq!(clock.take_due(T0 + FRAME_US), Some((&"B", 1)));
    for k in 2..=51 {
        assert_eq!(
            clock.take_due(T0 + k * FRAME_US),
            Some((&"B", 1)),
            "slot {k} repeats the last canvas"
        );
    }
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 52,
            repeated: 50,
            skipped: 0,
            max_burst: 1,
        }
    );
}

#[test]
fn the_newest_canvas_takes_the_slot_and_a_replaced_one_comes_back() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    // A was written: replacing it hands it back, it was not skipped.
    assert_eq!(clock.offer("B"), Some("A"));
    assert_eq!(clock.stats().skipped, 0);
    // B never reached a slot: replacing it skips it.
    assert_eq!(clock.offer("C"), Some("B"));
    assert_eq!(clock.stats().skipped, 1);
    assert_eq!(
        clock.take_due(T0 + FRAME_US),
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
fn every_slot_a_blocked_write_missed_is_written_at_once() {
    // The feeder's write blocked for 200 ms: slots 1-5 passed. All five go out
    // together, so the video timeline never falls behind the audio's.
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert_eq!(clock.take_due(T0 + 5 * FRAME_US), Some((&"A", 5)));
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 6,
            repeated: 5,
            skipped: 0,
            max_burst: 5,
        }
    );
    // A new canvas arrived during the next block: it fills the first missed
    // slot, then repeats; the burst counts it once as fresh.
    clock.offer("B");
    assert_eq!(clock.take_due(T0 + 9 * FRAME_US + 1), Some((&"B", 4)));
    assert_eq!(
        clock.stats(),
        VideoClockStats {
            written: 10,
            repeated: 8,
            skipped: 0,
            max_burst: 5,
        }
    );
    assert_eq!(clock.wait_us(T0 + 9 * FRAME_US + 1), FRAME_US - 1);
}

#[test]
fn the_wait_runs_to_the_next_slot_and_never_below_zero() {
    let mut clock = VideoClock::new();
    clock.offer("A");
    assert_eq!(clock.take_due(T0), Some((&"A", 1)));
    assert_eq!(clock.wait_us(T0), FRAME_US);
    assert_eq!(clock.wait_us(T0 + FRAME_US), 0, "slot 1 is due");
    assert_eq!(
        clock.wait_us(T0 + FRAME_US + 10_000),
        0,
        "and overdue, not negative"
    );
    assert_eq!(clock.take_due(T0 + FRAME_US + 10_000), Some((&"A", 1)));
    assert_eq!(clock.wait_us(T0 + FRAME_US + 10_000), 30_000, "slot 2");
}
