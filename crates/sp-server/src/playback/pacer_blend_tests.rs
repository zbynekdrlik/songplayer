//! #251: a boundary between two source frames shows both, blended. Pins
//! from a scratch Python model of the pacer's frame choice and
//! [`blend_weight_q8`] (the grid `b(k) = ⌊k · 10⁷ / 30⌋`, the song anchored
//! on b(1)).

use std::collections::VecDeque;

use sp_ndi::AudioFrame;

use super::{BLEND_DEAD_BAND_100NS, FrameBlend, blend_weight_q8};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::{PacedFrame, PacedSink, Pacer};
use crate::playback::picture::Picture;
use crate::playback::wallclock::{SettableClock, WallClock};

/// One 30 fps grid slot (100 ns), the pacer's `interval_100ns(30)`.
const SLOT: i64 = 333_333;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// Frame `id` at `pts` (100 ns): a `width`×2 NV12 picture whose every byte
/// is `id`, with no audio.
fn frame(id: u8, pts_100ns: i64, width: u32) -> PacedFrame {
    PacedFrame {
        pts_ns: pts_100ns * 100,
        width,
        height: 2,
        stride: width,
        video: SharedFrame::new(vec![id; width as usize * 3]).into(),
        audio: Vec::new(),
    }
}

/// What one boundary showed: the frame's id, and the next frame's id + the
/// weight when it was blended.
#[derive(Debug, PartialEq)]
enum Shown {
    Plain(u8),
    Blended(u8, u8, u32),
}

#[derive(Default)]
struct Rec {
    shown: Vec<Shown>,
    stamps: Vec<i64>,
}

impl PacedSink for Rec {
    fn emit(&mut self, video: &PacedFrame, _audio: &[AudioFrame], vtc: i64, atc: i64) {
        assert_eq!(vtc, atc, "audio and video stamped on one boundary");
        self.shown.push(Shown::Plain(video.video.expect_cpu()[0]));
        self.stamps.push(vtc);
    }

    fn emit_blended(
        &mut self,
        video: &PacedFrame,
        blend: FrameBlend,
        _audio: &[AudioFrame],
        vtc: i64,
        atc: i64,
    ) {
        assert_eq!(vtc, atc, "audio and video stamped on one boundary");
        assert_eq!(blend.video.len(), video.video.len(), "same size");
        self.shown.push(Shown::Blended(
            video.video.expect_cpu()[0],
            blend.video.expect_cpu()[0],
            blend.weight_q8,
        ));
        self.stamps.push(vtc);
    }
}

/// A sink that only shows frames (the trait's default `emit_blended`).
#[derive(Default)]
struct PlainOnly {
    shown: Vec<u8>,
}

impl PacedSink for PlainOnly {
    fn emit(&mut self, video: &PacedFrame, _audio: &[AudioFrame], _vtc: i64, _atc: i64) {
        self.shown.push(video.video.expect_cpu()[0]);
    }
}

fn anchored() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

/// Service boundaries b(1)..=b(n) of `frames` through `sink`.
fn serve<S: PacedSink>(frames: Vec<PacedFrame>, n: i64, sink: &mut S) {
    let (mut pacer, clk) = anchored();
    let mut queue: VecDeque<PacedFrame> = frames.into();
    for k in 1..=n {
        clk.set(b(k));
        pacer.service(|| queue.pop_front(), sink);
    }
}

/// `count` frames, frame k at `pts(k)` (100 ns), `width` wide, ids k
/// (wrapping at 256).
fn stream(count: i64, pts: impl Fn(i64) -> i64, width: u32) -> Vec<PacedFrame> {
    (0..count)
        .map(|k| frame((k % 256) as u8, pts(k), width))
        .collect()
}

#[test]
fn the_weight_is_where_the_boundary_lies_between_the_two_frames() {
    assert_eq!(
        blend_weight_q8(200_000, 400_000, SLOT),
        Some(128),
        "halfway"
    );
    assert_eq!(
        blend_weight_q8(333_333, 400_000, SLOT),
        Some(213),
        "floored"
    );
    assert_eq!(
        blend_weight_q8(399_999, 400_000, SLOT),
        Some(255),
        "just before the next frame"
    );
    assert_eq!(
        blend_weight_q8(400_000, 400_000, SLOT),
        None,
        "on the next frame: it is shown, not blended"
    );
}

#[test]
fn a_boundary_on_the_frame_or_just_after_it_shows_the_frame_alone() {
    assert_eq!(BLEND_DEAD_BAND_100NS, 20_000, "2 ms");
    assert_eq!(blend_weight_q8(0, 400_000, SLOT), None);
    assert_eq!(blend_weight_q8(19_999, 400_000, SLOT), None);
    assert_eq!(blend_weight_q8(20_000, 400_000, SLOT), Some(12));
}

#[test]
fn only_a_gap_between_one_slot_and_two_is_blended() {
    assert_eq!(
        blend_weight_q8(200_000, 353_333, SLOT),
        None,
        "29.97 fps and faster"
    );
    assert_eq!(blend_weight_q8(200_000, 353_334, SLOT), Some(144));
    assert_eq!(blend_weight_q8(200_000, 646_665, SLOT), Some(79));
    assert_eq!(
        blend_weight_q8(200_000, 646_666, SLOT),
        None,
        "15 fps and slower"
    );
    assert_eq!(
        blend_weight_q8(200_000, 1_000_000, SLOT),
        None,
        "10 fps, a stall, a seek"
    );
}

#[test]
fn a_25_fps_song_blends_every_boundary_between_two_frames() {
    let mut rec = Rec::default();
    serve(stream(20, |k| k * 400_000, 4), 8, &mut rec);
    assert_eq!(
        rec.shown,
        [
            Shown::Plain(0),
            Shown::Blended(0, 1, 213),
            Shown::Blended(1, 2, 170),
            Shown::Blended(2, 3, 128),
            Shown::Blended(3, 4, 85),
            Shown::Blended(4, 5, 42),
            Shown::Plain(5),
            Shown::Blended(5, 6, 213),
        ],
        "the repeat at b(2) is a blend too: no frame is held a slot unchanged"
    );
    assert_eq!(rec.stamps, (1..=8).map(b).collect::<Vec<_>>());
}

#[test]
fn the_next_frame_is_the_same_allocation_never_a_copy() {
    struct Same(Option<bool>, Picture);
    impl PacedSink for Same {
        fn emit(&mut self, _: &PacedFrame, _: &[AudioFrame], _: i64, _: i64) {}
        fn emit_blended(
            &mut self,
            _: &PacedFrame,
            blend: FrameBlend,
            _: &[AudioFrame],
            _: i64,
            _: i64,
        ) {
            self.0.get_or_insert(blend.video.ptr_eq(&self.1));
        }
    }
    let frames = stream(4, |k| k * 400_000, 4);
    let mut sink = Same(None, frames[1].video.clone());
    serve(frames, 2, &mut sink);
    assert_eq!(sink.0, Some(true), "frame 1 by Arc, blended into b(2)");
}

#[test]
fn a_30_fps_song_with_millisecond_pts_is_never_blended() {
    let mut rec = Rec::default();
    serve(stream(20, |k| k * 1000 / 30 * 10_000, 4), 8, &mut rec);
    assert_eq!(rec.shown, (0..8).map(Shown::Plain).collect::<Vec<_>>());
}

#[test]
fn a_10_fps_song_is_shown_as_before() {
    let mut rec = Rec::default();
    serve(stream(20, |k| k * 1_000_000, 4), 8, &mut rec);
    assert_eq!(
        rec.shown,
        [0, 0, 0, 1, 1, 1, 2, 2].map(Shown::Plain),
        "a gap of three slots is a held frame, never a blend"
    );
}

#[test]
fn a_next_frame_of_another_size_is_not_blended() {
    let mut frames = stream(20, |k| k * 400_000, 4);
    frames[1] = frame(1, 400_000, 8);
    let mut rec = Rec::default();
    serve(frames, 4, &mut rec);
    assert_eq!(
        rec.shown,
        [
            Shown::Plain(0),
            Shown::Plain(0),
            Shown::Plain(1),
            Shown::Blended(2, 3, 128),
        ]
    );
}

#[test]
fn a_sink_that_only_shows_frames_gets_every_boundary() {
    let mut sink = PlainOnly::default();
    serve(stream(20, |k| k * 400_000, 4), 8, &mut sink);
    assert_eq!(sink.shown, [0, 0, 1, 2, 3, 4, 5, 5]);
}

/// How many of the boundaries b(1)..=b(n) of `frames` were blended.
fn blended_count(frames: Vec<PacedFrame>, n: i64) -> usize {
    let mut rec = Rec::default();
    serve(frames, n, &mut rec);
    assert_eq!(rec.shown.len() as i64, n, "every boundary shows a frame");
    rec.shown
        .iter()
        .filter(|s| matches!(s, Shown::Blended(..)))
        .count()
}

/// A source's frame k at its Media Foundation time `k · period` (100 ns),
/// truncated to whole milliseconds as `to_paced_frame` gets it.
fn ms_pts(period_100ns: i64) -> impl Fn(i64) -> i64 {
    move |k| k * period_100ns / 10_000 * 10_000
}

#[test]
fn a_source_at_or_above_the_grid_rate_is_never_blended() {
    // Over 40 s: a 29.97 fps source's ms pts drift through the whole gap.
    for (fps, period, count) in [
        ("29.97", 333_667, 1_300),
        ("50", 200_000, 2_600),
        ("59.94", 166_833, 2_600),
        ("15", 666_667, 700),
    ] {
        assert_eq!(
            blended_count(stream(count, ms_pts(period), 4), 1_200),
            0,
            "{fps} fps keeps its own frames"
        );
    }
}

#[test]
fn a_24_or_25_fps_source_is_blended_wherever_it_lies_between_frames() {
    assert_eq!(
        blended_count(stream(1_300, ms_pts(400_000), 4), 1_200),
        1_000
    );
    assert_eq!(blended_count(stream(1_300, ms_pts(416_667), 4), 1_200), 960);
}
