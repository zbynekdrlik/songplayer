//! #279 P2: ring frames (`Picture::Gpu`, P3a's hardware decode) through the
//! pacer, on `sp_gpu::FakeRing`. A fresh emit, a repeat, a blend's next
//! frame, the paused frozen repeat and a seek's held picture all carry the
//! SAME ring frame (an `Arc` bump, never a copy: the frame's id names it),
//! and every slot is free again once the pacer and its sink let go.
//! Pins from `pacer_blend_tests.rs`'s 25 fps run (the same frame times).

use std::collections::VecDeque;
use std::time::Duration;

use sp_gpu::{FakeRing, GpuFrame};
use sp_ndi::AudioFrame;

use super::{FrameBlend, PacedFrame, PacedSink, Pacer, ServiceOutcome, Standby, StandbyBlack};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::picture::Picture;
use crate::playback::wallclock::{SettableClock, WallClock};

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// A 4×2 frame at `pts_ns` whose picture is a new frame of `ring`.
fn ring_frame(ring: &FakeRing, pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: ring
            .frame(4, 2, Duration::ZERO)
            .expect("a free slot")
            .into(),
        audio: Vec::new(),
    }
}

/// A picture's ring frame id; `None` for a CPU picture.
fn id(picture: &Picture) -> Option<u64> {
    picture.gpu().map(GpuFrame::id)
}

/// A paced frame's ring frame id (the test built a ring frame).
fn id_of(frame: &PacedFrame) -> u64 {
    id(&frame.video).expect("a ring frame")
}

/// What each boundary showed: the picture's id and, blended, the next one's.
#[derive(Default)]
struct Rec {
    shown: Vec<(Option<u64>, Option<u64>)>,
    /// The pictures handed over, in order (identity checks).
    pictures: Vec<Picture>,
}

impl PacedSink for Rec {
    fn emit(&mut self, video: &PacedFrame, _audio: &[AudioFrame], _vtc: i64, _atc: i64) {
        self.shown.push((id(&video.video), None));
        self.pictures.push(video.video.clone());
    }

    fn emit_blended(
        &mut self,
        video: &PacedFrame,
        blend: FrameBlend,
        _audio: &[AudioFrame],
        _vtc: i64,
        _atc: i64,
    ) {
        self.shown.push((id(&video.video), id(&blend.video)));
        self.pictures.push(video.video.clone());
    }
}

/// The 4×2 standby black (a CPU picture).
fn black(video: &Picture) -> StandbyBlack<'_> {
    StandbyBlack {
        width: 4,
        height: 2,
        stride: 4,
        video,
    }
}

fn anchored() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

#[test]
fn ring_frames_are_shown_repeated_blended_and_frozen_by_arc() {
    // A 25 fps song of ring frames on the 30 fps grid: b(2) repeats frame 0
    // blended toward frame 1, … b(7) shows frame 5 alone, b(8) blends it
    // toward frame 6; then a pause freezes frame 5 on b(9).
    let ring = FakeRing::new(8);
    let frames: Vec<PacedFrame> = (0..8).map(|k| ring_frame(&ring, k * 40_000_000)).collect();
    let ids: Vec<u64> = frames.iter().map(id_of).collect();
    assert_eq!(ring.stats().in_use, 8, "each frame holds its slot");
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    let mut queue: VecDeque<PacedFrame> = frames.into();
    for k in 1..=8 {
        clk.set(b(k));
        pacer.service(|| queue.pop_front(), &mut rec);
    }
    clk.set(b(9));
    assert_eq!(
        pacer.service_standby(Standby::FrozenLast, &mut rec),
        ServiceOutcome::Repeated
    );
    let shown = |i: usize| Some(ids[i]);
    assert_eq!(
        rec.shown,
        [
            (shown(0), None),
            (shown(0), shown(1)),
            (shown(1), shown(2)),
            (shown(2), shown(3)),
            (shown(3), shown(4)),
            (shown(4), shown(5)),
            (shown(5), None),
            (shown(5), shown(6)),
            (shown(5), None),
        ],
        "each boundary names its ring frames by their own ids"
    );
    assert!(
        rec.pictures[1].ptr_eq(&rec.pictures[0]),
        "the repeat at b(2) is frame 0 itself"
    );
    assert!(
        rec.pictures[8].ptr_eq(&rec.pictures[6]),
        "the frozen b(9) is frame 5 itself"
    );
    assert_eq!(
        ring.stats().in_use,
        8,
        "nothing copied, nothing freed early"
    );
    drop((pacer, queue, rec));
    assert_eq!(
        ring.stats().in_use,
        0,
        "every slot is free once nothing holds its frame"
    );
}

#[test]
fn a_seek_refill_holds_the_last_ring_frame_until_the_pacer_lets_go() {
    // Idle b(1)..=b(3) on the CPU black, a pre-roll, ring frames on b(4) and
    // b(5), then a seek: the starved b(6) shows frame b(5) (the pacer's
    // held pre-seek picture, by `Arc`), which keeps its slot until the pacer
    // is gone.
    let ring = FakeRing::new(2);
    let blk = Picture::Cpu(SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2]));
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    for k in 1..=3 {
        clk.set(b(k));
        pacer.service_standby(black(&blk).standby(), &mut rec);
    }
    pacer.preroll(
        black(&blk),
        &mut rec,
        || Some(()),
        |_, until| clk.set(until),
    );
    let mut song: VecDeque<PacedFrame> = (0..2)
        .map(|j| ring_frame(&ring, (b(4 + j) - b(4)) * 100))
        .collect();
    let ids: Vec<u64> = song.iter().map(id_of).collect();
    for k in 4..=5 {
        clk.set(b(k));
        assert_eq!(
            pacer.service(|| song.pop_front(), &mut rec),
            ServiceOutcome::Emitted
        );
    }
    clk.set(b(5) + 1_000);
    pacer.anchor_seek();
    clk.set(b(6));
    assert_eq!(pacer.service(|| None, &mut rec), ServiceOutcome::Starved);
    assert_eq!(
        rec.shown,
        [
            (None, None),
            (None, None),
            (None, None),
            (Some(ids[0]), None),
            (Some(ids[1]), None),
            (Some(ids[1]), None),
        ],
        "the idle black is a CPU picture; the refill holds frame b(5)"
    );
    assert!(
        rec.pictures[5].ptr_eq(&rec.pictures[4]),
        "the refill is frame b(5) itself"
    );
    drop(rec);
    assert_eq!(
        ring.stats().in_use,
        1,
        "only the pacer's held picture keeps a slot"
    );
    drop(pacer);
    assert_eq!(ring.stats().in_use, 0);
}
