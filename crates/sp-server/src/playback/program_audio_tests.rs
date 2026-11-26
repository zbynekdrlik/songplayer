//! #289 step 2: the program's audio side (`program_audio.rs`): the audio a
//! commit moves out, the line, the hold to the stamp, the margin, and the
//! `program-audio` loop over a real bus.
//! Wired via `#[cfg(test)] #[path = "program_audio_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use sp_ndi::AudioFrame;

use super::{
    AudioJob, AudioLine, AudioSide, HOLD_MAX_100NS, LINE_BOUND, Limited, LineState, LineTake,
    MarginWindow, MinMargin, hold_100ns, hold_to_stamp, program_spc, run_audio_loop, silent_block,
    take_audio,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{AudioTake, ProgramBus, ProgramJob};
use crate::playback::program_max_send::SendClock;
use crate::playback::program_transition::MixJob;
use crate::playback::stat_window::Worst;
use crate::playback::submit_handoff::SubmitJob;

const T0: i64 = 17_900_000_000_000_000;

fn block(level: f32) -> AudioFrame {
    AudioFrame {
        data: vec![level; 3200],
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }
}

fn pair(stamp: i64, level: f32) -> SubmitJob {
    SubmitJob {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![7u8; 6]).into(),
        audio: vec![block(level)],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
        media_pts_100ns: None,
        blend: None,
    }
}

fn limited(stamp: i64) -> Limited {
    Limited {
        stamp_100ns: stamp,
        audio: None,
        fed_100ns: 0,
        limited_frames: 0,
    }
}

#[test]
fn a_source_s_audio_is_moved_out_and_its_picture_stays() {
    let mut job = ProgramJob::Source(pair(T0, 0.25));
    match take_audio(&mut job) {
        AudioJob::Source { stamp_100ns, audio } => {
            assert_eq!(stamp_100ns, T0);
            assert_eq!(audio.len(), 1);
            assert_eq!(audio[0].data[0], 0.25);
        }
        other => panic!("{other:?}"),
    }
    let ProgramJob::Source(job) = job else {
        panic!("still a source")
    };
    assert!(job.audio.is_empty());
    assert_eq!(job.video.expect_cpu()[0], 7, "the picture stays");
}

#[test]
fn a_standby_and_an_empty_mix_are_the_silence() {
    let mut job = ProgramJob::Standby { stamp_100ns: T0 };
    assert!(matches!(
        take_audio(&mut job),
        AudioJob::Standby { stamp_100ns: T0 }
    ));
    let mut empty = ProgramJob::Mix(MixJob {
        stamp_100ns: T0 + 9,
        from: None,
        to: None,
        slot: 1,
        n_slots: 9,
    });
    let audio = take_audio(&mut empty);
    assert!(matches!(audio, AudioJob::Standby { stamp_100ns } if stamp_100ns == T0 + 9));
    assert_eq!(audio.stamp_100ns(), T0 + 9);
}

#[test]
fn a_mix_carries_both_sides_first_blocks_and_its_place_in_the_window() {
    let mut job = ProgramJob::Mix(MixJob {
        stamp_100ns: T0,
        from: Some(pair(T0, 0.1)),
        to: None,
        slot: 3,
        n_slots: 9,
    });
    match take_audio(&mut job) {
        AudioJob::Mix {
            stamp_100ns,
            from,
            to,
            first,
            total,
        } => {
            assert_eq!(stamp_100ns, T0);
            assert_eq!(from.expect("from's block").data[0], 0.1);
            assert!(to.is_none());
            assert_eq!((first, total), (3 * 1600, 9 * 1600));
        }
        other => panic!("{other:?}"),
    }
    let ProgramJob::Mix(mix) = job else { panic!() };
    assert!(mix.from.expect("the picture stays").audio.is_empty());
    assert_eq!(program_spc(), 1600);
    assert_eq!(silent_block()[0].data.len(), 3200);
}

#[test]
fn the_line_hands_out_a_stamp_s_block_passing_older_ones_over() {
    let mut line = LineState::default();
    assert_eq!(line.len(), 0);
    assert!(matches!(line.take(T0), LineTake::NotYet));
    line.push(limited(T0));
    line.push(limited(T0 + 1));
    line.push(limited(T0 + 3));
    assert_eq!(line.len(), 3);
    assert!(matches!(line.take(T0 + 1), LineTake::Block(b) if b.stamp_100ns == T0 + 1));
    assert_eq!(line.status().stale, 1, "T0 was passed over");
    assert!(matches!(line.take(T0 + 2), LineTake::Missing));
    assert!(matches!(line.take(T0 + 3), LineTake::Block(_)));
    assert_eq!(line.len(), 0);
    assert_eq!(line.status().stale, 1);
}

#[test]
fn a_full_line_drops_its_oldest_block() {
    assert_eq!(LINE_BOUND, 12, "a full program queue's worth and two");
    let mut line = LineState::default();
    for k in 0..=LINE_BOUND as i64 {
        line.push(limited(T0 + k));
    }
    assert_eq!(line.len(), LINE_BOUND);
    assert_eq!(line.status().dropped, 1);
    assert!(
        matches!(line.take(T0 + 1), LineTake::Block(_)),
        "the oldest went"
    );
    assert_eq!(line.status().stale, 0);
}

#[test]
fn the_sender_waits_for_a_block_the_audio_thread_is_about_to_push() {
    let line = Arc::new(AudioLine::default());
    let pusher = line.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        pusher.push(limited(T0));
    });
    let got = line.take(T0, Duration::from_secs(5));
    thread.join().unwrap();
    assert_eq!(got.expect("the block").stamp_100ns, T0);
    assert_eq!(line.status().missing, 0);
    assert!(line.take(T0 + 1, Duration::from_millis(1)).is_none());
    assert_eq!(line.status().missing, 1, "it never came");
}

#[test]
fn a_job_is_held_to_its_stamp_only_within_the_lead_and_a_slot() {
    assert_eq!(HOLD_MAX_100NS, 666_668);
    assert_eq!(hold_100ns(T0 + 300_000, T0), 300_000);
    assert_eq!(hold_100ns(T0 + HOLD_MAX_100NS, T0), HOLD_MAX_100NS);
    assert_eq!(
        hold_100ns(T0 + HOLD_MAX_100NS + 1, T0),
        0,
        "a backward step"
    );
    assert_eq!(hold_100ns(T0, T0), 0);
    assert_eq!(hold_100ns(T0 + 1, T0), 1, "100 ns ahead is held");
    assert_eq!(hold_100ns(T0 - 5, T0), 0, "late: at once");
}

/// A clock that records the one wait asked of it.
struct Recording {
    start: Instant,
    waited: Option<Duration>,
}

impl SendClock for Recording {
    fn now(&mut self) -> Instant {
        self.start
    }
    fn wait_until(&mut self, due: Instant) {
        self.waited = Some(due - self.start);
    }
}

#[test]
fn the_hold_waits_to_the_stamp_then_serves_at_the_wall_s_now() {
    let mut clock = Recording {
        start: Instant::now(),
        waited: None,
    };
    let reads = AtomicI64::new(0);
    let now = || {
        reads.fetch_add(1, Ordering::Relaxed);
        T0 + 250_000
    };
    let served = hold_to_stamp(&mut clock, T0 + 250_000, T0, &now);
    assert_eq!(clock.waited, Some(Duration::from_micros(25_000)));
    assert_eq!(served, T0 + 250_000);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    let mut clock = Recording {
        start: Instant::now(),
        waited: None,
    };
    assert_eq!(hold_to_stamp(&mut clock, T0, T0 + 7, &now), T0 + 7);
    assert_eq!(clock.waited, None, "a job not ahead waits for nothing");
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}

#[test]
fn the_margin_is_the_smallest_lead_and_counts_the_boundaries_ahead() {
    assert_eq!(
        MinMargin(None).worst(MinMargin(Some(5))),
        MinMargin(Some(5))
    );
    assert_eq!(
        MinMargin(Some(5)).worst(MinMargin(None)),
        MinMargin(Some(5))
    );
    assert_eq!(
        MinMargin(Some(5)).worst(MinMargin(Some(-2))),
        MinMargin(Some(-2))
    );
    let mut m = MarginWindow::default();
    assert_eq!(m.margin_us_min(), None);
    m.observe(T0, T0 - 333_333);
    m.observe(T0, T0 - 120_000);
    m.observe(T0, T0);
    assert_eq!(m.ahead(), 2, "the hand-off at the stamp is not ahead");
    assert_eq!(m.margin_us_min(), Some(0));
    m.observe(T0, T0 + 50);
    assert_eq!(m.margin_us_min(), Some(-5));
}

#[test]
fn the_audio_side_limits_and_stamps_the_silence_of_a_standby() {
    let mut side = AudioSide::default();
    assert!(side.outputs().is_none());
    let out = side.process(AudioJob::Standby { stamp_100ns: T0 }, &|| T0 - 9);
    assert!(out.audio.is_none());
    assert_eq!(
        (out.stamp_100ns, out.fed_100ns, out.limited_frames),
        (T0, T0 - 9, 0)
    );
    let hot = side.process(
        AudioJob::Source {
            stamp_100ns: T0 + 333_333,
            audio: vec![block(1.5)],
        },
        &|| 0,
    );
    assert_eq!(hot.limited_frames, 1600);
    assert!(hot.audio.unwrap()[0].data.iter().all(|s| s.abs() <= 0.981));
}

/// The loop over a real bus: every committed boundary's audio reaches the
/// line in order, and a stop ends it.
#[test]
fn the_audio_loop_feeds_the_line_and_ends_on_a_stop() {
    let bus = Arc::new(ProgramBus::new().with_audio_ahead(1));
    assert!(matches!(
        bus.take_audio_timeout(Duration::ZERO),
        AudioTake::Idle
    ));
    bus.select_initial(5, None);
    let thread_bus = bus.clone();
    let thread = std::thread::spawn(move || {
        let mut side = AudioSide::new();
        run_audio_loop(&mut side, &thread_bus, || 42);
    });
    let b0 = sp_core::genlock::floor_boundary_100ns(T0, 30);
    let b1 = sp_core::genlock::strict_next_boundary_100ns(b0, 30);
    bus.offer(5, pair(b0, 0.1));
    bus.offer(5, pair(b1, 0.2));
    let first = bus.audio_line().take(b0, Duration::from_secs(5));
    let first = first.expect("b0's block");
    assert_eq!(first.fed_100ns, 42);
    assert_eq!(first.audio.unwrap()[0].data[0], 0.1);
    let second = bus.audio_line().take(b1, Duration::from_secs(5));
    assert_eq!(second.expect("b1's block").audio.unwrap()[0].data[0], 0.2);
    bus.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(thread.is_finished(), "the stop ended the loop");
    thread.join().unwrap();
    assert!(matches!(
        bus.take_audio_timeout(Duration::ZERO),
        AudioTake::Stopped
    ));
    assert_eq!(bus.status().health.audio.lead_slots, 1);
}

/// Both readers are woken by a commit, never left to their own timeouts:
/// the audio thread and the sender each wait a minute and get the boundary
/// at once.
#[test]
fn a_commit_wakes_the_audio_thread_and_the_sender() {
    let bus = Arc::new(ProgramBus::new().with_audio_ahead(1));
    bus.select_initial(5, None);
    let (tx, rx) = std::sync::mpsc::channel();
    let audio_bus = bus.clone();
    let audio_tx = tx.clone();
    let audio = std::thread::spawn(move || {
        let got = audio_bus.take_audio_timeout(Duration::from_secs(60));
        audio_tx.send(matches!(got, AudioTake::Job(_))).unwrap();
    });
    let video_bus = bus.clone();
    let video = std::thread::spawn(move || {
        let got = video_bus.take_timeout(Duration::from_secs(60));
        let job = matches!(got, crate::playback::program_bus::Take::Job(..));
        tx.send(job).unwrap();
    });
    std::thread::sleep(Duration::from_millis(30));
    bus.offer(5, pair(sp_core::genlock::floor_boundary_100ns(T0, 30), 0.1));
    let first = rx.recv_timeout(Duration::from_secs(5));
    let second = rx.recv_timeout(Duration::from_secs(5));
    bus.stop(); // frees a reader a mutant left waiting
    audio.join().unwrap();
    video.join().unwrap();
    assert_eq!(first, Ok(true), "woken with the boundary");
    assert_eq!(second, Ok(true), "both woken with it");
}
