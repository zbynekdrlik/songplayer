//! #147 (design record 5845527884, Approach 1 (a)): the pipeline-lifetime paced
//! submit consumer keeps EVERY genlock boundary serviced across a song change,
//! a pause and idle→play — Δ exactly one slot, the gap filled with the held
//! picture + silence — and the next pacer continues right after it.
//!
//! The pipeline is driven single-threaded and deterministically: one handoff,
//! the consumer on a twin of the owner's `MockNdiBackend` sender, and a pacer,
//! all on ONE settable clock; `drain` runs the consumer until it waits (in
//! production it runs concurrently on its own thread). The window between two
//! scopes (the old producer's teardown + the next scope's setup) is modelled
//! as slots nobody feeds (`Rig::between_scopes`).
//! Song frames are 8×2 (A) / 12×2 (B) and the standby black 4×2, so the
//! `send_video_async(…,WxH,…)` call strings name each boundary's picture.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{PacedFrame, Pacer, ServiceOutcome, Standby, StandbyBlack};
use crate::playback::submit_handoff::{SUBMIT_HANDOFF_BOUND, SubmitCounters, SubmitJob};
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// A quarter of the 30 fps slot: a detached boundary is filled this late.
const GRACE: i64 = 83_333;

const AUDIO: &str = "send_audio(42,sr=48000,ch=2,spc=1600)";
const BLACK: &str = "send_video_async(42,NV12,4x2,stride=4,30/1)";
const SONG_A: &str = "send_video_async(42,NV12,8x2,stride=8,30/1)";
const SONG_B: &str = "send_video_async(42,NV12,12x2,stride=12,30/1)";

/// A `w`×2 song frame at `pts_ns` with one boundary of stereo audio.
fn frame(w: u32, pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: w,
        height: 2,
        stride: w,
        video: SharedFrame::new(vec![0u8; (w * 2 * 3 / 2) as usize]),
        audio: vec![AudioFrame {
            data: vec![0.25; 1600 * 2],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// `n` `w`×2 frames due on consecutive boundaries from the song's anchor `b(first)`.
fn song(w: u32, first: i64, n: i64) -> VecDeque<PacedFrame> {
    (0..n)
        .map(|j| frame(w, (b(first + j) - b(first)) * 100))
        .collect()
}

/// A submit job stamped `stamp` with a `w`×2 picture and `channels` of audio
/// (no audio frame at all for `None`).
fn job(stamp: i64, w: u32, channels: Option<u32>) -> SubmitJob {
    SubmitJob {
        width: w,
        height: 2,
        stride: w,
        video: SharedFrame::new(vec![0u8; (w * 2 * 3 / 2) as usize]),
        audio: channels
            .map(|ch| AudioFrame {
                data: vec![0.5; 1600 * ch as usize],
                channels: ch,
                sample_rate: 48_000,
                timecode_100ns: None,
            })
            .into_iter()
            .collect(),
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
    }
}

/// Run `consumer` until it waits at wall time `now`: every queued job
/// submitted, every due fill done.
fn drain(handoff: &SharedHandoff, consumer: &mut PacedConsumer<MockNdiBackend>, now: i64) {
    for _ in 0..64 {
        let step = handoff.step_now(now);
        if matches!(step, ConsumerStep::Wait(_) | ConsumerStep::Exit) {
            return;
        }
        consumer.serve(handoff, step);
    }
    panic!("the consumer never waited");
}

/// The paced output as the pipeline runs it (see the module doc).
struct Rig {
    backend: Arc<MockNdiBackend>,
    /// The owning sender, alive past every check: dropping it appends
    /// `send_destroy` (#209 gotcha).
    _owner: NdiSender<MockNdiBackend>,
    handoff: Arc<SharedHandoff>,
    consumer: PacedConsumer<MockNdiBackend>,
    pacer: Pacer,
    clk: SettableClock,
    black: SharedFrame,
}

impl Rig {
    fn new() -> Self {
        let backend = Arc::new(MockNdiBackend::new());
        let owner = NdiSender::new_with_clocking(backend.clone(), "PO", false, false).unwrap();
        let (pacer_wall, clk) = WallClock::settable(0);
        let consumer_wall = WallClock::new(Box::new(clk.clone()));
        let mut submitter = FrameSubmitter::new(owner.twin(), 30, 1);
        submitter.set_paced(true);
        let black = SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2]);
        let picture = Picture {
            width: 4,
            height: 2,
            stride: 4,
            video: black.clone(),
        };
        let consumer = PacedConsumer::new(submitter, 7, consumer_wall, picture);
        let mut pacer = Pacer::with_wallclock(30, true, pacer_wall);
        clk.set(0);
        pacer.anchor(); // the first boundary is b(1)
        Self {
            backend,
            _owner: owner,
            handoff: Arc::new(SharedHandoff::new(SUBMIT_HANDOFF_BOUND)),
            consumer,
            pacer,
            clk,
            black,
        }
    }

    /// Run the consumer until it waits at the current time.
    fn drain(&mut self) {
        drain(&self.handoff, &mut self.consumer, self.clk.get());
    }

    /// No scope feeds boundaries `from..=to` (between two scopes): the
    /// consumer is woken a grace after each and services it.
    fn between_scopes(&mut self, from: i64, to: i64) {
        for k in from..=to {
            self.clk.set(b(k) + GRACE);
            self.drain();
        }
    }

    /// Every audio / video send after the sender's creation (the consumer's
    /// connection-count polls are left out).
    fn sends(&self) -> Vec<String> {
        self.backend.calls()[1..]
            .iter()
            .filter(|c| !c.starts_with("send_get_no_connections"))
            .cloned()
            .collect()
    }

    fn counters(&self) -> SubmitCounters {
        self.handoff.snapshot().0
    }
}

/// `n` audio + video pairs of `video`.
fn pairs(video: &str, n: usize) -> Vec<String> {
    (0..n)
        .flat_map(|_| [AUDIO.to_string(), video.to_string()])
        .collect()
}

#[test]
fn a_song_change_gap_and_a_slow_decoder_open_keep_every_stamp_one_slot_apart() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    // Song A plays b(1)..=b(3).
    {
        let feed = PacedFeed::attach(&h);
        assert_eq!(feed.last_serviced_100ns(), None);
        let mut a = song(8, 1, 3);
        for k in 1..=3 {
            rig.clk.set(b(k));
            let mut sink = feed.sink();
            let out = rig.pacer.service(|| a.pop_front(), &mut sink);
            assert_eq!(out, ServiceOutcome::Emitted, "k={k}");
            rig.drain();
        }
    }
    let last_a = rig.backend.last_async_video_slice();
    // The old producer's teardown + the next scope's setup take 3 slots:
    // nobody feeds b(4)..=b(6).
    for k in 4..=6 {
        rig.between_scopes(k, k);
        assert_eq!(
            rig.backend.last_async_video_slice(),
            last_a,
            "b({k}) holds song A's last picture (same buffer, no copy)"
        );
        let silence = rig.backend.last_audio_planar();
        assert_eq!(silence.len(), 3200, "b({k}): one stereo block");
        assert!(silence.iter().all(|&s| s == 0.0), "b({k}): silence");
    }
    // Song B: the pre-roll continues right after the last serviced stamp, and
    // its decoder takes 3 slots to open (ready on the 4th readiness poll).
    rig.clk.set(b(6) + GRACE + 1_000);
    {
        let feed = PacedFeed::attach(&h);
        assert_eq!(feed.last_serviced_100ns(), Some(b(6)));
        rig.pacer.continue_grid_after(b(6));
        let mut sink = feed.sink();
        let (clk, handoff, consumer) = (rig.clk.clone(), &rig.handoff, &mut rig.consumer);
        let black = StandbyBlack {
            width: 4,
            height: 2,
            stride: 4,
            video: &rig.black,
        };
        let polls = std::cell::Cell::new(0u32);
        rig.pacer.preroll(
            black,
            &mut sink,
            || {
                polls.set(polls.get() + 1);
                (polls.get() >= 4).then_some(())
            },
            |_, until| {
                clk.set(until);
                drain(handoff, consumer, until);
            },
        );
        assert_eq!(polls.get(), 4);
        rig.drain();
        let mut bs = song(12, 10, 3);
        for k in 10..=12 {
            rig.clk.set(b(k));
            let out = rig.pacer.service(|| bs.pop_front(), &mut sink);
            assert_eq!(out, ServiceOutcome::Emitted, "k={k}");
            rig.drain();
        }
    }
    assert_eq!(
        rig.backend.video_timecodes(),
        (1..=12).map(b).collect::<Vec<_>>(),
        "Δ = exactly one slot across the song change"
    );
    let mut want = pairs(SONG_A, 6);
    want.extend(pairs(BLACK, 3));
    want.extend(pairs(SONG_B, 3));
    assert_eq!(rig.sends(), want, "one audio + video pair per boundary");
    // A fill's audio block is stamped on its boundary, like its picture.
    assert_eq!(rig.backend.audio_timecodes()[3..6], [b(4), b(5), b(6)]);
    let c = rig.counters();
    assert_eq!(c.consumer_fill_pairs, 3);
    assert_eq!(c.song_change_unserviced_slots, 0);
    assert_eq!(c.submitted, 12);
    assert_eq!(c.dropped, 0, "nothing coalesced, nothing stale");
}

#[test]
fn pause_resume_stays_contiguous_and_a_song_change_while_paused_holds_the_frozen_picture() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    {
        let feed = PacedFeed::attach(&h);
        let mut sink = feed.sink();
        let mut a = song(8, 1, 12);
        // Play b(1)..=b(2), pause b(3)..=b(4), resume b(5)..=b(6), pause b(7).
        for k in 1..=7 {
            rig.clk.set(b(k));
            let paused = matches!(k, 3 | 4 | 7);
            let out = if paused {
                rig.pacer.service_standby(Standby::FrozenLast, &mut sink)
            } else {
                if k == 5 {
                    rig.pacer.audio_resume_reset();
                }
                rig.pacer.service(|| a.pop_front(), &mut sink)
            };
            let want = if paused {
                ServiceOutcome::Repeated
            } else {
                ServiceOutcome::Emitted
            };
            assert_eq!(out, want, "k={k}");
            rig.drain();
        }
        assert_eq!(
            rig.counters().consumer_fill_pairs,
            0,
            "a pacer owns b(1..=7)"
        );
    }
    let frozen = rig.backend.last_async_video_slice();
    // Play B arrives while paused: 3 slots between the scopes.
    rig.between_scopes(8, 10);
    assert_eq!(
        rig.backend.last_async_video_slice(),
        frozen,
        "the fills hold the frozen picture"
    );
    rig.clk.set(b(10) + GRACE + 1_000);
    {
        let feed = PacedFeed::attach(&h);
        assert_eq!(feed.last_serviced_100ns(), Some(b(10)));
        rig.pacer.continue_grid_after(b(10));
        let mut sink = feed.sink();
        let clk = rig.clk.clone();
        let black = StandbyBlack {
            width: 4,
            height: 2,
            stride: 4,
            video: &rig.black,
        };
        rig.pacer
            .preroll(black, &mut sink, || Some(()), |_, until| clk.set(until));
        let mut bs = song(12, 11, 2);
        for k in 11..=12 {
            rig.clk.set(b(k));
            rig.pacer.service(|| bs.pop_front(), &mut sink);
            rig.drain();
        }
    }
    assert_eq!(
        rig.backend.video_timecodes(),
        (1..=12).map(b).collect::<Vec<_>>(),
        "play → pause → play → (paused) next song: Δ = one slot throughout"
    );
    let mut want = pairs(SONG_A, 10);
    want.extend(pairs(SONG_B, 2));
    assert_eq!(rig.sends(), want);
    let c = rig.counters();
    assert_eq!(c.consumer_fill_pairs, 3);
    assert_eq!(c.song_change_unserviced_slots, 0);
}

#[test]
fn idle_to_play_across_a_slow_open_holds_the_idle_black_and_stays_contiguous() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    {
        let feed = PacedFeed::attach(&h);
        let mut sink = feed.sink();
        for k in 1..=3 {
            rig.clk.set(b(k));
            let black = StandbyBlack {
                width: 4,
                height: 2,
                stride: 4,
                video: &rig.black,
            };
            let out = rig.pacer.service_standby(black.standby(), &mut sink);
            assert_eq!(out, ServiceOutcome::Emitted, "idle k={k}");
            rig.drain();
        }
    }
    rig.between_scopes(4, 6);
    assert_eq!(
        rig.backend.last_async_video_slice().map(|(ptr, _)| ptr),
        Some(rig.black.as_ptr() as usize),
        "the fills hold the idle black itself (a refcount bump)"
    );
    rig.clk.set(b(6) + GRACE + 1_000);
    {
        let feed = PacedFeed::attach(&h);
        rig.pacer.continue_grid_after(b(6));
        let mut sink = feed.sink();
        let clk = rig.clk.clone();
        let black = StandbyBlack {
            width: 4,
            height: 2,
            stride: 4,
            video: &rig.black,
        };
        rig.pacer
            .preroll(black, &mut sink, || Some(()), |_, until| clk.set(until));
        let mut bs = song(12, 7, 2);
        for k in 7..=8 {
            rig.clk.set(b(k));
            rig.pacer.service(|| bs.pop_front(), &mut sink);
            rig.drain();
        }
    }
    assert_eq!(
        rig.backend.video_timecodes(),
        (1..=8).map(b).collect::<Vec<_>>(),
        "idle → play: Δ = one slot"
    );
    let mut want = pairs(BLACK, 6);
    want.extend(pairs(SONG_B, 2));
    assert_eq!(rig.sends(), want);
    assert_eq!(rig.counters().consumer_fill_pairs, 3);
    assert_eq!(rig.counters().song_change_unserviced_slots, 0);
}

#[test]
fn a_consumer_starved_past_eight_slots_resyncs_and_counts_the_hole_honestly() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    {
        let feed = PacedFeed::attach(&h);
        let mut sink = feed.sink();
        let mut a = song(8, 1, 1);
        rig.clk.set(b(1));
        rig.pacer.service(|| a.pop_front(), &mut sink);
        rig.drain();
    }
    // The consumer is not scheduled for 12 slots: b(2) is 11 behind b(13).
    rig.clk.set(b(13) + GRACE);
    rig.drain();
    assert_eq!(rig.backend.video_timecodes(), vec![b(1), b(13)]);
    let c = rig.counters();
    assert_eq!(c.consumer_fill_pairs, 1);
    assert_eq!(
        c.song_change_unserviced_slots, 11,
        "b(2)..=b(12) unserviced"
    );
    // The next pacer continues right after the resynced stamp.
    let feed = PacedFeed::attach(&h);
    assert_eq!(feed.last_serviced_100ns(), Some(b(13)));
}

#[test]
fn a_scope_attaching_while_the_last_jobs_are_still_queued_continues_after_them() {
    // A natural song end: the EOS-tail boundary b(4) is offered, the song
    // detaches, and the idle scope attaches ~1 ms later while b(4) (behind a
    // slow submit) is still queued. The next pacer must continue at b(5):
    // re-emitting b(4) is a stale drop, and a second queued job would coalesce
    // into a real stamp hole.
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    {
        let _song = PacedFeed::attach(&h);
        h.offer(job(b(3), 8, Some(2)));
        rig.drain();
        h.offer(job(b(4), 8, Some(2)));
    }
    let idle = PacedFeed::attach(&h);
    assert_eq!(
        idle.last_serviced_100ns(),
        Some(b(4)),
        "the queued b(4) counts as serviced"
    );
    h.offer(job(b(5), 8, Some(2)));
    rig.drain();
    assert_eq!(rig.backend.video_timecodes(), vec![b(3), b(4), b(5)]);
    let c = rig.counters();
    assert_eq!(c.dropped, 0);
    assert_eq!(c.song_change_unserviced_slots, 0);
}

#[test]
fn the_old_pacers_queued_tail_does_not_hide_a_hole_before_the_next_pacers_first_stamp() {
    // As above, but the next pacer's first stamp lands 11 slots late (a hung
    // box): the queued b(4) is still the OLD pacer's, so the b(5)..=b(14) hole
    // after it is a song-change hole and must be counted.
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    {
        let _song = PacedFeed::attach(&h);
        h.offer(job(b(3), 8, Some(2)));
        rig.drain();
        h.offer(job(b(4), 8, Some(2)));
    }
    let _idle = PacedFeed::attach(&h);
    rig.drain();
    h.offer(job(b(15), 8, Some(2)));
    rig.drain();
    assert_eq!(rig.backend.video_timecodes(), vec![b(3), b(4), b(15)]);
    assert_eq!(rig.counters().song_change_unserviced_slots, 10);
}

#[test]
fn an_attached_pacer_owns_the_grid_even_when_it_is_late() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    let feed = PacedFeed::attach(&h);
    h.offer(job(b(1), 8, Some(2)));
    rig.drain();
    // The emit thread is late by 8 slots: the consumer never steals a boundary.
    rig.clk.set(b(9));
    assert!(matches!(h.step_now(b(9)), ConsumerStep::Wait(None)));
    rig.drain();
    assert_eq!(rig.backend.video_timecodes(), vec![b(1)]);
    drop(feed);
    // Detached: the fill is due a grace after the next boundary.
    assert!(matches!(
        h.step_now(b(2) + GRACE - 1),
        ConsumerStep::Wait(Some(d)) if d == b(2) + GRACE
    ));
}

#[test]
fn a_job_at_or_before_the_last_serviced_stamp_is_dropped_never_sent() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    h.offer(job(b(3), 8, Some(2)));
    rig.drain();
    h.offer(job(b(3), 8, Some(2)));
    h.offer(job(b(2), 8, Some(2)));
    rig.drain();
    assert_eq!(rig.backend.video_timecodes(), vec![b(3)]);
    assert_eq!(rig.counters().dropped, 2, "both stale jobs counted");
    assert_eq!(rig.counters().submitted, 1);
}

#[test]
fn stop_drains_the_queue_then_exits_without_filling() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    h.offer(job(b(1), 8, Some(2)));
    h.stop();
    rig.clk.set(b(5));
    let step = h.step_now(b(5));
    assert!(
        matches!(step, ConsumerStep::Submit(_)),
        "the queue drains first"
    );
    rig.consumer.serve(&h, step);
    assert!(
        matches!(h.step_now(b(5)), ConsumerStep::Exit),
        "no fill after stop"
    );
    rig.drain();
    assert_eq!(
        rig.backend.video_timecodes(),
        vec![b(1)],
        "the drained job went out"
    );
    assert_eq!(rig.counters().consumer_fill_pairs, 0);
}

#[test]
fn a_fill_follows_the_last_audio_layout_and_ignores_an_empty_one() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    h.offer(job(b(1), 8, Some(1)));
    rig.between_scopes(1, 2); // the mono job, then a fill of b(2)
    // A zero-channel block (sent as nothing) keeps the mono layout.
    h.offer(job(b(3), 8, Some(0)));
    rig.between_scopes(3, 4);
    // A job with no audio keeps it too.
    h.offer(job(b(5), 8, None));
    rig.between_scopes(5, 6);
    let mono = "send_audio(42,sr=48000,ch=1,spc=1600)";
    assert_eq!(
        rig.sends(),
        vec![
            mono, SONG_A, mono, SONG_A, SONG_A, mono, SONG_A, SONG_A, mono, SONG_A,
        ],
        "every fill after a mono song is a mono silent block"
    );
    assert_eq!(
        rig.backend.video_timecodes(),
        (1..=6).map(b).collect::<Vec<_>>()
    );
}

#[test]
fn the_consumer_polls_connections_on_its_first_submit_then_every_thirtieth() {
    let mut rig = Rig::new();
    let h = rig.handoff.clone();
    rig.backend.set_connection_count(3);
    // Each job is submitted 3 ms after its boundary.
    rig.clk.set(b(1) + 30_000);
    h.offer(job(b(1), 8, Some(2)));
    rig.drain();
    assert_eq!(h.snapshot().1, 3, "polled on the first submit");
    rig.backend.set_connection_count(5);
    for k in 2..=30 {
        rig.clk.set(b(k) + 30_000);
        h.offer(job(b(k), 8, Some(2)));
        rig.drain();
    }
    assert_eq!(h.snapshot().1, 3, "not polled on submits 2..=30");
    rig.clk.set(b(31) + 30_000);
    h.offer(job(b(31), 8, Some(2)));
    rig.drain();
    assert_eq!(h.snapshot().1, 5, "polled again on the 31st");
    let c = rig.counters();
    assert_eq!(c.submitted, 31);
    assert_eq!(c.late_frames, 31, "3 ms after the stamp is late (> 2 ms)");
    assert_eq!(c.max_late_us, 3_000);
    assert_eq!(c.last_submit_100ns, b(31) + 30_000);
    assert_eq!(c.submit_p99_us(), 0, "the fixed clock measures no SDK cost");
}

#[test]
fn the_wait_before_a_fill_is_the_time_left_to_its_deadline() {
    assert_eq!(
        wait_before_fill(b(4) + GRACE, b(4)),
        Duration::from_nanos(GRACE as u64 * 100)
    );
    assert_eq!(wait_before_fill(b(4), b(4)), Duration::ZERO);
    assert_eq!(wait_before_fill(b(4), b(4) + 1), Duration::ZERO, "overdue");
}

#[test]
fn continue_grid_after_latches_the_boundary_right_after_the_last_serviced_stamp() {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    assert_eq!(pacer.next_boundary_100ns(), b(1));
    pacer.continue_grid_after(b(5));
    assert_eq!(pacer.next_boundary_100ns(), b(6));
    // Genlock off: there is no grid to continue.
    let (wall, _clk) = WallClock::settable(0);
    let mut off = Pacer::with_wallclock(0, true, wall);
    off.continue_grid_after(b(5));
    assert_eq!(off.next_boundary_100ns(), 0);
}

#[test]
fn the_pipeline_submitter_spawns_one_paced_thread_and_joins_it_before_destroying_the_sender() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "PS", false, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);
    submitter.set_paced(true);
    let first = submitter.paced_handoff(7, 4, 2);
    let again = submitter.paced_handoff(7, 4, 2);
    assert!(Arc::ptr_eq(&first, &again), "one paced thread per pipeline");
    let feed = PacedFeed::attach(&first);
    first.offer(job(b(1), 8, Some(2)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while first.submitted() < 1 {
        assert!(
            Instant::now() < deadline,
            "the paced thread never submitted"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(feed);
    drop(first);
    drop(again);
    // Drop on a helper thread, so a hung join fails in bounded time.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(submitter);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("dropping the submitter stops + joins the paced thread");
    let calls = backend.calls();
    assert!(calls.contains(&SONG_A.to_string()), "the job went out");
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("send_destroy"))
            .count(),
        1,
        "the twin never destroys"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("send_destroy(42)"),
        "the owning sender is destroyed LAST, after the paced thread flushed"
    );
}

#[test]
fn a_paced_thread_that_is_gone_is_respawned_by_the_next_scope() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "PR", false, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);
    submitter.set_paced(true);
    let first = submitter.paced_handoff(7, 4, 2);
    // The thread exits (stop here; a panic in production).
    first.stop();
    let deadline = Instant::now() + Duration::from_secs(10);
    let second = loop {
        let h = submitter.paced_handoff(7, 4, 2);
        if !Arc::ptr_eq(&h, &first) {
            break h;
        }
        assert!(
            Instant::now() < deadline,
            "the gone thread was never respawned"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    // The respawned thread submits.
    let feed = PacedFeed::attach(&second);
    second.offer(job(b(1), 8, Some(2)));
    while second.submitted() < 1 {
        assert!(
            Instant::now() < deadline,
            "the respawned thread never submitted"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(feed);
    assert!(
        Arc::ptr_eq(&second, &submitter.paced_handoff(7, 4, 2)),
        "a live thread is kept"
    );
    drop(first);
    drop(second);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(submitter);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("dropping the submitter joins the respawned thread");
    let calls = backend.calls();
    assert!(calls.contains(&SONG_A.to_string()));
    assert_eq!(
        calls.last().map(String::as_str),
        Some("send_destroy(42)"),
        "one owner, destroyed last"
    );
}

#[test]
fn the_live_paced_thread_services_detached_boundaries_on_its_own() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "PL", false, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);
    submitter.set_paced(true);
    let handoff = submitter.paced_handoff(9, 4, 2);
    // One job on the current real boundary, then nobody feeds.
    let now = crate::playback::wallclock::utc_now_100ns();
    handoff.offer(job(
        sp_core::genlock::floor_boundary_100ns(now, 30),
        8,
        Some(2),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while handoff.submitted() < 4 {
        assert!(
            Instant::now() < deadline,
            "the detached thread never filled on its own"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // Real time on a shared runner: assert the shape, not the timing (the
    // exact contiguity is pinned by the deterministic tests above).
    let stamps = backend.video_timecodes();
    for pair in stamps.windows(2) {
        assert!(pair[1] > pair[0], "stamps only increase: {stamps:?}");
    }
    for s in &stamps {
        assert_eq!(
            sp_core::genlock::floor_boundary_100ns(*s, 30),
            *s,
            "every fill is stamped exactly on a grid boundary"
        );
    }
    drop(handoff);
    drop(submitter);
}
