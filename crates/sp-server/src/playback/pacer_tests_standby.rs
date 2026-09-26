//! #147 standby same-path (design comment 5841796900): an idle (black) or
//! paused (frozen) boundary leaves the pacer exactly like a playing boundary —
//! one audio block of `samples_per_boundary` first, then the NV12 video async,
//! stamped by `resolve_emit_boundary` — and idle→play keeps the grid cadence
//! contiguous. Drives the pure [`Pacer`] over a settable clock, with either a
//! recording sink or the real [`FrameSubmitter`] over `MockNdiBackend`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::submitter::FrameSubmitter;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::sync::Arc;

/// The k-th exact-rational grid boundary at 30 fps, in 100-ns units.
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// The one boundary's worth of SDK calls a paced output makes for a 4x2 frame:
/// one 1600-sample stereo audio block, then the async NV12 video.
const AUDIO_CALL: &str = "send_audio(42,sr=48000,ch=2,spc=1600)";
const VIDEO_CALL: &str = "send_video_async(42,NV12,4x2,stride=4,30/1)";

/// A pacer over a settable clock, anchored at clock 0 (first boundary `b(1)`).
fn anchored_pacer() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

fn black() -> SharedFrame {
    SharedFrame::new(vec![16u8; 4 * 2 * 3 / 2])
}

fn idle(video: &SharedFrame) -> Standby<'_> {
    Standby::Black {
        width: 4,
        height: 2,
        stride: 4,
        video,
    }
}

/// A 4x2 frame at `pts_ns` carrying one boundary (1600 samples per channel) of
/// untimed, non-silent audio in `channels` channels.
fn frame_with_block(pts_ns: i64, channels: u32) -> PacedFrame {
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 12]),
        audio: vec![AudioFrame {
            data: vec![0.25; 1600 * channels as usize],
            channels,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// One recorded boundary: the video stamp, the audio stamp, and the audio
/// block's `(channels, samples per channel, all zero)` — `None` = no audio.
type Boundary = (i64, i64, Option<(u32, usize, bool)>);

#[derive(Default)]
struct Rec {
    boundaries: Vec<Boundary>,
}

impl Rec {
    fn video_tcs(&self) -> Vec<i64> {
        self.boundaries.iter().map(|b| b.0).collect()
    }
}

impl PacedSink for Rec {
    fn emit(&mut self, _video: &PacedFrame, audio: &[AudioFrame], vtc: i64, atc: i64) {
        assert!(audio.len() <= 1, "at most ONE audio block per boundary");
        let block = audio.first().map(|a| {
            let spc = a.data.len() / a.channels.max(1) as usize;
            (a.channels, spc, a.data.iter().all(|&s| s == 0.0))
        });
        self.boundaries.push((vtc, atc, block));
    }
}

/// A paced `FrameSubmitter` over a recording mock (`clock_video=false`, as the
/// paced pipeline creates it).
fn paced_submitter() -> (Arc<MockNdiBackend>, FrameSubmitter<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "SB", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    sub.set_paced(true);
    (backend, sub)
}

#[test]
fn an_idle_boundary_carries_one_silent_stereo_block_stamped_like_playing_audio() {
    let (mut pacer, clk) = anchored_pacer();
    let blk = black();
    let mut rec = Rec::default();
    // 2 ms late: the audio stamp is the emit instant, the video stamp the boundary.
    clk.set(b(1) + 20_000);
    assert_eq!(
        pacer.service_standby(idle(&blk), &mut rec),
        ServiceOutcome::Emitted
    );
    assert_eq!(
        rec.boundaries,
        vec![(b(1), b(1) + 20_000, Some((2, 1600, true)))],
        "idle = one silent stereo 1600-sample block + the on-grid video stamp"
    );
}

#[test]
fn a_paused_boundary_carries_silence_in_the_songs_channel_layout() {
    // A MONO song, then paused: the standby silence follows the song's layout
    // (the grid buffer knows it), so the receiver's audio format never flips.
    let (mut pacer, clk) = anchored_pacer();
    let mut rec = Rec::default();
    let mut first = Some(frame_with_block(0, 1));
    clk.set(b(1));
    assert_eq!(
        pacer.service(|| first.take(), &mut rec),
        ServiceOutcome::Emitted
    );
    clk.set(b(2));
    assert_eq!(
        pacer.service_standby(Standby::FrozenLast, &mut rec),
        ServiceOutcome::Repeated
    );
    assert_eq!(
        rec.boundaries[0].2,
        Some((1, 1600, false)),
        "the song block"
    );
    assert_eq!(
        rec.boundaries[1].2,
        Some((1, 1600, true)),
        "paused = one silent MONO block (the song's layout)"
    );
}

#[test]
fn idle_paused_and_playing_boundaries_make_the_same_sdk_call_sequence() {
    let (mut pacer, clk) = anchored_pacer();
    let (backend, mut sub) = paced_submitter();
    let blk = black();

    // Idle (no song): the black standby.
    clk.set(b(1));
    assert_eq!(
        pacer.service_standby(idle(&blk), &mut sub),
        ServiceOutcome::Emitted
    );
    // Play arrives inside the slot: anchor → the song's first frame is due b(2).
    clk.set(b(1) + 10_000);
    pacer.anchor();
    let mut song = Some(frame_with_block(0, 2));
    clk.set(b(2));
    assert_eq!(
        pacer.service(|| song.take(), &mut sub),
        ServiceOutcome::Emitted
    );
    // Pause: the frozen frame.
    clk.set(b(3));
    assert_eq!(
        pacer.service_standby(Standby::FrozenLast, &mut sub),
        ServiceOutcome::Repeated
    );

    let calls = backend.calls();
    let expected: Vec<String> = std::iter::repeat_n([AUDIO_CALL, VIDEO_CALL], 3)
        .flatten()
        .map(str::to_string)
        .collect();
    assert_eq!(
        calls[1..],
        expected[..],
        "idle, playing and paused boundaries: the SAME audio-then-NV12-async pair"
    );
    assert_eq!(backend.video_timecodes(), vec![b(1), b(2), b(3)]);
    assert_eq!(backend.audio_timecodes(), vec![b(1), b(2), b(3)]);
    // The paused boundary's block (the last audio sent) is pure silence.
    let last = backend.last_audio_planar();
    assert_eq!(last.len(), 2 * 1600);
    assert!(last.iter().all(|&s| s == 0.0), "paused audio is silence");
}

#[test]
fn the_paced_output_never_sends_a_sync_frame_or_bgra_and_keeps_the_grid_contiguous() {
    // A whole paced output lifetime through the real FrameSubmitter: start-up
    // standby, idle, a song, a pause, the song end, idle again.
    let (mut pacer, clk) = anchored_pacer();
    let (backend, mut sub) = paced_submitter();
    let blk = black();

    sub.send_standby_black(1920, 1080); // pipeline start
    for k in 1..=3 {
        clk.set(b(k));
        pacer.service_standby(idle(&blk), &mut sub);
    }
    clk.set(b(3) + 5_000); // Play
    pacer.anchor();
    let mut song: std::collections::VecDeque<PacedFrame> = (0..3)
        .map(|j| frame_with_block((b(4 + j) - b(4)) * 100, 2))
        .collect();
    for k in 4..=6 {
        clk.set(b(k));
        pacer.service(|| song.pop_front(), &mut sub);
    }
    for k in 7..=8 {
        clk.set(b(k)); // Pause
        pacer.service_standby(Standby::FrozenLast, &mut sub);
    }
    sub.send_standby_black(1920, 1080); // song end / Stop
    for k in 9..=10 {
        clk.set(b(k));
        pacer.service_standby(idle(&blk), &mut sub);
    }

    let calls = backend.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("send_video(")),
        "no synchronous send_video on the paced path: {calls:#?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("BGRA")),
        "no BGRA frame on the paced path: {calls:#?}"
    );
    let expected: Vec<String> = std::iter::repeat_n([AUDIO_CALL, VIDEO_CALL], 10)
        .flatten()
        .map(str::to_string)
        .collect();
    assert_eq!(
        calls[1..],
        expected[..],
        "one audio+video pair per boundary"
    );
    assert_eq!(
        backend.video_timecodes(),
        (1..=10).map(b).collect::<Vec<_>>(),
        "every boundary b(1)..b(10) exactly once: none skipped, none doubled"
    );
}

#[test]
fn standby_stamps_are_resolve_emit_boundarys_stamp_on_a_normal_slot_and_a_resync() {
    // `resolve_emit_boundary` decides the stamp for BOTH `service` and
    // `service_standby`. A mirror pacer `r` driven step by step through it must
    // produce exactly the standby pacer's stamps: the normal one-slot advance AND
    // a > 8-slot stall with nothing queued (a resync → floor(now)).
    let (mut s, clk_s) = anchored_pacer();
    let (mut r, clk_r) = anchored_pacer();
    let blk = black();
    let mut rec = Rec::default();

    for now in [b(1), b(2), b(3), b(43) + 1_234] {
        clk_s.set(now);
        s.service_standby(idle(&blk), &mut rec);

        clk_r.set(now);
        let boundary = latched_boundary_100ns(now, r.next_boundary_100ns, 30);
        let (stamp, next) = r.resolve_emit_boundary(now, boundary, false);
        r.next_boundary_100ns = next;
        assert_eq!(
            *rec.video_tcs().last().unwrap(),
            stamp,
            "standby stamp == resolve_emit_boundary stamp at now={now}"
        );
    }
    assert_eq!(rec.video_tcs(), vec![b(1), b(2), b(3), b(43)]);
    assert_eq!(s.stats().resyncs, 1, "the stall resynced once");
    assert_eq!(r.stats().resyncs, 1);
}

#[test]
fn a_standby_boundary_matches_a_playing_repeat_stamp_and_block() {
    // The same history, one pacer PLAYING (the decoder has nothing new: a
    // repeat), the other PAUSED (FrozenLast): identical stamps (normal slot and
    // a resync) and an identical silent 1600-sample block at every boundary.
    let (mut play, clk_p) = anchored_pacer();
    let (mut pause, clk_q) = anchored_pacer();
    let mut rec_play = Rec::default();
    let mut rec_pause = Rec::default();
    let mut f1 = Some(frame_with_block(0, 2));
    let mut f2 = Some(frame_with_block(0, 2));
    clk_p.set(b(1));
    clk_q.set(b(1));
    play.service(|| f1.take(), &mut rec_play);
    pause.service(|| f2.take(), &mut rec_pause);

    for now in [b(2), b(3), b(43) + 1_234] {
        clk_p.set(now);
        clk_q.set(now);
        assert_eq!(
            play.service(|| None, &mut rec_play),
            ServiceOutcome::Repeated
        );
        assert_eq!(
            pause.service_standby(Standby::FrozenLast, &mut rec_pause),
            ServiceOutcome::Repeated
        );
    }
    assert_eq!(
        rec_pause.boundaries, rec_play.boundaries,
        "standby = a playing repeat: same stamps, same silent block"
    );
    assert_eq!(rec_pause.video_tcs(), vec![b(1), b(2), b(3), b(43)]);
    assert!(
        rec_pause.boundaries[1..]
            .iter()
            .all(|b| b.2 == Some((2, 1600, true))),
        "every standby boundary: one silent stereo 1600-sample block"
    );
}

#[test]
fn idle_to_play_keeps_the_stamp_and_audio_cadence_contiguous() {
    // Play lands (a) exactly on the last idle slot's instant and (b) 5 ms into
    // it. Either way the song's first frame takes the VERY NEXT boundary: no
    // boundary is skipped or doubled, and every boundary carries one block.
    for play_at in [b(5), b(5) + 50_000] {
        let (mut pacer, clk) = anchored_pacer();
        let blk = black();
        let mut rec = Rec::default();
        for k in 1..=5 {
            clk.set(b(k));
            pacer.service_standby(idle(&blk), &mut rec);
        }
        clk.set(play_at);
        pacer.anchor();
        let mut song: std::collections::VecDeque<PacedFrame> = (0..5)
            .map(|j| frame_with_block((b(6 + j) - b(6)) * 100, 2))
            .collect();
        for k in 6..=10 {
            clk.set(b(k));
            assert_eq!(
                pacer.service(|| song.pop_front(), &mut rec),
                ServiceOutcome::Emitted,
                "play_at={play_at} k={k}"
            );
        }
        assert_eq!(
            rec.video_tcs(),
            (1..=10).map(b).collect::<Vec<_>>(),
            "play_at={play_at}: b(1)..b(10), none skipped, none doubled"
        );
        let blocks: Vec<_> = rec.boundaries.iter().map(|b| b.2).collect();
        assert!(
            blocks[..5].iter().all(|b| *b == Some((2, 1600, true))),
            "idle: one silent block per boundary: {blocks:?}"
        );
        assert!(
            blocks[5..].iter().all(|b| *b == Some((2, 1600, false))),
            "play: one song block per boundary: {blocks:?}"
        );
    }
}

#[test]
fn the_paced_outer_loop_enters_the_idle_fill_at_once_legacy_keeps_5_s() {
    use crate::playback::pacer_sink::idle_poll;
    assert_eq!(
        idle_poll(true),
        std::time::Duration::ZERO,
        "paced: no 5 s hole before the idle fill after start / song end / stop"
    );
    assert_eq!(
        idle_poll(false),
        std::time::Duration::from_secs(5),
        "SDK-clocked: the unchanged 5 s heartbeat poll"
    );
}
