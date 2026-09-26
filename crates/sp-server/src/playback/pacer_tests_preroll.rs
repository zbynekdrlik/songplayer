//! #147 song-start pre-roll (ROZHODNUTÉ comment 5842127369): while the decoder
//! opens and decodes its first frame, every boundary still carries the standby
//! pair (black + one silent block); the song's grid anchors only once it is
//! ready, so its first frame lands on the very next boundary. Drives the pure
//! [`Pacer::preroll`] over a settable clock with a slow-opening "decoder" poll.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::cell::Cell;
use std::sync::Arc;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

fn anchored_pacer() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

/// The 4x2 standby black.
fn black_frame() -> SharedFrame {
    SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2])
}

fn black(video: &SharedFrame) -> StandbyBlack<'_> {
    StandbyBlack {
        width: 4,
        height: 2,
        stride: 4,
        video,
    }
}

/// An 8x2 song frame (a different size than the 4x2 black, so the SDK call
/// strings tell them apart) at `pts_ns`, with one boundary of stereo audio.
fn song_frame(pts_ns: i64) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 8,
        height: 2,
        stride: 8,
        video: SharedFrame::new(vec![0u8; 8 * 2 * 3 / 2]),
        audio: vec![AudioFrame {
            data: vec![0.25; 1600 * 2],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// A "decoder" that is ready only on its `ready_on`-th readiness poll (1-based).
fn slow_decoder(ready_on: u32) -> (Cell<u32>, u32) {
    (Cell::new(0), ready_on)
}

/// One recorded boundary: video stamp, whether the video is the standby black,
/// and the audio block's `(channels, samples per channel)` (`None` = no audio).
#[derive(Default)]
struct Rec {
    black: Option<SharedFrame>,
    boundaries: Vec<(i64, bool, Option<(u32, usize)>)>,
}

impl PacedSink for Rec {
    fn emit(&mut self, video: &PacedFrame, audio: &[AudioFrame], vtc: i64, _atc: i64) {
        assert!(audio.len() <= 1, "at most ONE audio block per boundary");
        let is_black = self.black.as_ref().is_some_and(|b| b.ptr_eq(&video.video));
        let block = audio
            .first()
            .map(|a| (a.channels, a.data.len() / a.channels.max(1) as usize));
        self.boundaries.push((vtc, is_black, block));
    }
}

const SIDE_CALLS: [&str; 2] = [
    "send_audio(42,sr=48000,ch=2,spc=1600)",
    "send_video_async(42,NV12,4x2,stride=4,30/1)",
];
const SONG_CALLS: [&str; 2] = [
    "send_audio(42,sr=48000,ch=2,spc=1600)",
    "send_video_async(42,NV12,8x2,stride=8,30/1)",
];

#[test]
fn a_slow_decoder_open_is_filled_with_standby_pairs_and_the_song_starts_on_the_next_boundary() {
    // Idle at b(1)..b(3), then Play: the decoder is ready only on its 4th
    // readiness poll (≈ 3 boundaries of open + first decode). Every boundary
    // b(1)..b(9) must carry exactly one audio+video pair, the stamps must be
    // contiguous, and the first song frame must land on b(7): the boundary
    // right after readiness was seen (before b(7)).
    let (mut pacer, clk) = anchored_pacer();
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "PR", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    sub.set_paced(true);
    let blk = black_frame();

    for k in 1..=3 {
        clk.set(b(k));
        pacer.service_standby(black(&blk).standby(), &mut sub);
    }

    let (polls, ready_on) = slow_decoder(4);
    pacer.preroll(
        black(&blk),
        &mut sub,
        || {
            polls.set(polls.get() + 1);
            (polls.get() >= ready_on).then_some(())
        },
        |_, until| clk.set(until),
    );
    assert_eq!(polls.get(), 4, "readiness asked once per slot");

    let mut song: std::collections::VecDeque<PacedFrame> = (0..3)
        .map(|j| song_frame((b(7 + j) - b(7)) * 100))
        .collect();
    for k in 7..=9 {
        clk.set(b(k));
        assert_eq!(
            pacer.service(|| song.pop_front(), &mut sub),
            ServiceOutcome::Emitted,
            "k={k}"
        );
    }

    let calls = backend.calls();
    let mut expected: Vec<String> = Vec::new();
    for _ in 0..6 {
        expected.extend(SIDE_CALLS.iter().map(|c| c.to_string()));
    }
    for _ in 0..3 {
        expected.extend(SONG_CALLS.iter().map(|c| c.to_string()));
    }
    assert_eq!(
        calls[1..],
        expected[..],
        "b(1)..b(6) standby pairs (3 idle + 3 pre-roll), then the song from b(7)"
    );
    assert_eq!(
        backend.video_timecodes(),
        (1..=9).map(b).collect::<Vec<_>>(),
        "every boundary b(1)..b(9) exactly once: none skipped, none doubled"
    );
    assert_eq!(backend.audio_timecodes().len(), 9, "one audio block each");
}

#[test]
fn a_decoder_ready_at_once_starts_on_the_very_next_boundary() {
    // Ready on the first poll: no extra standby boundary, the song's first frame
    // takes the boundary right after the last idle one.
    let (mut pacer, clk) = anchored_pacer();
    let blk = black_frame();
    let mut rec = Rec {
        black: Some(blk.clone()),
        ..Rec::default()
    };
    for k in 1..=3 {
        clk.set(b(k));
        pacer.service_standby(black(&blk).standby(), &mut rec);
    }
    let waits = Cell::new(0);
    pacer.preroll(
        black(&blk),
        &mut rec,
        || Some(()),
        |_, until| {
            waits.set(waits.get() + 1);
            clk.set(until);
        },
    );
    assert_eq!(waits.get(), 0, "ready at once: no pre-roll boundary");
    let mut first = Some(song_frame(0));
    clk.set(b(4));
    assert_eq!(
        pacer.service(|| first.take(), &mut rec),
        ServiceOutcome::Emitted
    );
    let stamps: Vec<i64> = rec.boundaries.iter().map(|x| x.0).collect();
    assert_eq!(stamps, (1..=4).map(b).collect::<Vec<_>>());
    assert!(!rec.boundaries[3].1, "b(4) is the song frame, not black");
}

#[test]
fn song_end_to_next_song_keeps_one_pair_per_boundary_through_the_preroll() {
    // Song A (b(1), b(2)) ends; its tail rides one frozen boundary b(3); the
    // next song's decoder takes 2 more slots to be ready. b(4), b(5) are
    // pre-roll black, song B starts on b(6). One block on every boundary.
    let (mut pacer, clk) = anchored_pacer();
    let blk = black_frame();
    let mut rec = Rec {
        black: Some(blk.clone()),
        ..Rec::default()
    };
    let mut song_a: std::collections::VecDeque<PacedFrame> = (0..2)
        .map(|j| song_frame((b(1 + j) - b(1)) * 100))
        .collect();
    for k in 1..=2 {
        clk.set(b(k));
        pacer.service(|| song_a.pop_front(), &mut rec);
    }
    pacer.hold_eos_tail_for_standby();
    clk.set(b(3));
    assert_eq!(
        pacer.service_standby(Standby::FrozenLast, &mut rec),
        ServiceOutcome::Repeated
    );

    let (polls, ready_on) = slow_decoder(3);
    pacer.preroll(
        black(&blk),
        &mut rec,
        || {
            polls.set(polls.get() + 1);
            (polls.get() >= ready_on).then_some(())
        },
        |_, until| clk.set(until),
    );
    let mut song_b = Some(song_frame(0));
    clk.set(b(6));
    assert_eq!(
        pacer.service(|| song_b.take(), &mut rec),
        ServiceOutcome::Emitted
    );

    let stamps: Vec<i64> = rec.boundaries.iter().map(|x| x.0).collect();
    assert_eq!(stamps, (1..=6).map(b).collect::<Vec<_>>(), "contiguous");
    let is_black: Vec<bool> = rec.boundaries.iter().map(|x| x.1).collect();
    assert_eq!(
        is_black,
        vec![false, false, false, true, true, false],
        "song A, its frozen tail boundary, 2 pre-roll blacks, song B"
    );
    assert!(
        rec.boundaries.iter().all(|x| x.2 == Some((2, 1600))),
        "one 1600-sample block on every boundary: {:?}",
        rec.boundaries
    );
}

#[test]
fn preroll_gate_reads_the_open_result_once_and_waits_for_the_first_frame() {
    let mut gate: PrerollGate<u32, String> = PrerollGate::pending();
    let opens = Cell::new(0);
    let open = |r: Option<Result<u32, String>>| {
        let opens = &opens;
        move || {
            opens.set(opens.get() + 1);
            r
        }
    };
    // Still opening: not ready, and "primed" is never consulted.
    assert_eq!(
        gate.poll(open(None), || panic!("primed read before the open")),
        None
    );
    // Opened, first frame not buffered yet: not ready.
    assert_eq!(gate.poll(open(Some(Ok(7))), || false), None);
    // Primed: ready with the open result, and the open channel was NOT read again.
    assert_eq!(
        gate.poll(|| panic!("open read twice"), || true),
        Some(Ok(7))
    );
    assert_eq!(opens.get(), 2, "one read while opening, one that opened");
}

#[test]
fn preroll_gate_ends_at_once_on_a_failed_open() {
    let mut gate: PrerollGate<u32, String> = PrerollGate::pending();
    assert_eq!(
        gate.poll(|| Some(Err("no file".to_string())), || false),
        Some(Err("no file".to_string())),
        "a failed open ends the pre-roll without waiting for a frame"
    );
}
