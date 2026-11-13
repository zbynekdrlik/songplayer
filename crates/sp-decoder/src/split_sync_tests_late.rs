//! #318: the decoder while the next picture is late — `next_synced_within`
//! answers `NotReady` and consumes nothing, `next_late` hands out steps on
//! the source cadence with the audio up to each step plus the lead, the
//! frames at or before the last step are dropped when pictures come back, a
//! seek ends the run, a picture missing past `AUDIO_PAST_VIDEO_MAX_MS` fails
//! the song, and a seek's fast-forward survives a picture that is not ready.

use std::collections::VecDeque;
use std::time::Duration;

use super::{AUDIO_PAST_VIDEO_MAX_MS, SplitSyncedDecoder, Synced};
use crate::error::DecoderError;
use crate::stream::{AudioStream, FramePoll, MediaStream, VideoStream};
use crate::types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat};

/// Frames at `k · 100` ms (10 fps), `frames` of them. The polls answer from
/// `polls` first: `false` = not ready (the frame stays the next one). A seek
/// lands 200 ms before the position (a keyframe), clamped at 0.
struct Video {
    frames: u64,
    next: u64,
    polls: VecDeque<bool>,
    rate: (u32, u32),
}

impl Video {
    fn new(frames: u64) -> Self {
        Self {
            frames,
            next: 0,
            polls: VecDeque::new(),
            rate: (10, 1),
        }
    }
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.frames * 100
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.next = position_ms.saturating_sub(200) / 100;
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let k = self.next;
        self.next += 1;
        Ok(Some(DecodedVideoFrame {
            data: vec![k as u8; 12],
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: k * 100,
            pixel_format: PixelFormat::Nv12,
            gpu: None,
        }))
    }
    fn poll_frame(&mut self, _wait: Duration) -> Result<FramePoll, DecoderError> {
        if self.polls.pop_front() == Some(false) {
            return Ok(FramePoll::NotReady);
        }
        Ok(FramePoll::Ready(self.next_frame()?))
    }
    fn width(&self) -> u32 {
        4
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        self.rate
    }
}

/// An audio chunk every 10 ms up to `end_ms` (exclusive).
struct Audio {
    end_ms: u64,
    next_ms: u64,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        self.end_ms
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.next_ms = position_ms;
        Ok(())
    }
}

impl AudioStream for Audio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        if self.next_ms >= self.end_ms {
            return Ok(None);
        }
        let ms = self.next_ms;
        self.next_ms += 10;
        Ok(Some(DecodedAudioFrame {
            data: vec![0.0; 960],
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: ms,
        }))
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

/// A decoder (lead 40 ms) over `video` and audio up to `audio_end_ms`.
fn decoder(video: Video, audio_end_ms: u64) -> SplitSyncedDecoder {
    let audio = Audio {
        end_ms: audio_end_ms,
        next_ms: 0,
    };
    SplitSyncedDecoder::new(Box::new(video), Box::new(audio)).expect("valid mocks")
}

/// The next frame's time, waiting with a zero wait.
fn frame_ms(dec: &mut SplitSyncedDecoder) -> u64 {
    match dec.next_synced_within(Some(Duration::ZERO)).unwrap() {
        Synced::Frame(v, _) => v.timestamp_ms,
        other => panic!("a frame: {other:?}"),
    }
}

/// The audio times of `audio`.
fn times(audio: &[DecodedAudioFrame]) -> Vec<u64> {
    audio.iter().map(|a| a.timestamp_ms).collect()
}

#[test]
fn a_picture_not_ready_consumes_nothing() {
    let mut video = Video::new(5);
    video.polls = VecDeque::from([true, false, false]);
    let mut dec = decoder(video, 1000);
    assert_eq!(frame_ms(&mut dec), 0);
    for _ in 0..2 {
        let polled = dec.next_synced_within(Some(Duration::ZERO)).unwrap();
        assert!(matches!(polled, Synced::NotReady), "{polled:?}");
    }
    match dec.next_synced_within(Some(Duration::ZERO)).unwrap() {
        Synced::Frame(v, audio) => {
            assert_eq!(v.timestamp_ms, 100);
            assert_eq!(times(&audio), [50, 60, 70, 80, 90, 100, 110, 120, 130, 140]);
        }
        other => panic!("frame 1: {other:?}"),
    }
    assert!(!dec.in_late_run());
    assert_eq!(dec.late_counts(), (0, 0));
}

#[test]
fn late_steps_carry_the_audio_and_the_late_frames_are_dropped() {
    let mut video = Video::new(6);
    video.polls = VecDeque::from([true, false]);
    let mut dec = decoder(video, 2000);
    assert_eq!(frame_ms(&mut dec), 0);
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::NotReady
    ));
    let (at, audio) = dec.next_late().unwrap().expect("a late step");
    assert_eq!(at, 100);
    assert_eq!(times(&audio), [50, 60, 70, 80, 90, 100, 110, 120, 130, 140]);
    assert!(dec.in_late_run());
    let (at, audio) = dec.next_late().unwrap().expect("a second step");
    assert_eq!(at, 200);
    assert_eq!(
        times(&audio),
        [150, 160, 170, 180, 190, 200, 210, 220, 230, 240]
    );
    // Frames 1 (100) and 2 (200) lie on the late line: dropped; frame 3 is
    // the first past it, with the audio from where the steps left off.
    match dec.next_synced_within(Some(Duration::ZERO)).unwrap() {
        Synced::Frame(v, audio) => {
            assert_eq!(v.timestamp_ms, 300);
            assert_eq!(
                times(&audio),
                [250, 260, 270, 280, 290, 300, 310, 320, 330, 340]
            );
        }
        other => panic!("frame 3: {other:?}"),
    }
    assert_eq!(dec.late_counts(), (2, 2));
    assert!(!dec.in_late_run(), "the picture is back");
    // A new run counts from the frame the picture came back on.
    let (at, _) = dec.next_late().unwrap().expect("a step of a new run");
    assert_eq!(at, 400);
}

#[test]
fn no_late_step_without_a_picture_to_hold_or_after_either_end() {
    let mut dec = decoder(Video::new(2), 150);
    assert!(dec.next_late().unwrap().is_none(), "no frame yet");
    assert_eq!(frame_ms(&mut dec), 0);
    assert_eq!(frame_ms(&mut dec), 100);
    // The audio (150 ms) went out with frame 1 (up to 140): nothing to carry.
    assert!(dec.audio_ended());
    assert!(dec.next_late().unwrap().is_none(), "no audio left");
    let mut dec = decoder(Video::new(1), 5000);
    assert_eq!(frame_ms(&mut dec), 0);
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::Ended
    ));
    assert!(
        dec.next_late().unwrap().is_none(),
        "the video ended: the tail's job"
    );
}

#[test]
fn a_picture_missing_past_the_bound_fails_the_song() {
    let mut dec = decoder(Video::new(1), 60_000);
    assert_eq!(frame_ms(&mut dec), 0);
    // 10 fps: step k at k · 100 ms; 150 steps reach exactly the bound.
    for k in 1..=AUDIO_PAST_VIDEO_MAX_MS / 100 {
        let (at, _) = dec.next_late().unwrap().expect("within the bound");
        assert_eq!(at, k * 100);
    }
    match dec.next_late() {
        Err(DecoderError::Decode(text)) => {
            assert!(text.contains("15100 ms past 0 ms"), "{text}")
        }
        other => panic!("the song fails: {other:?}"),
    }
}

#[test]
fn a_seek_ends_the_late_run() {
    let mut dec = decoder(Video::new(20), 5000);
    assert_eq!(frame_ms(&mut dec), 0);
    dec.next_late().unwrap().expect("a late step at 100");
    dec.next_late().unwrap().expect("a late step at 200");
    // Back to 300: the video lands on 100, the fast-forward to 300; the old
    // late line (200) drops nothing any more.
    dec.seek(300).unwrap();
    assert!(!dec.in_late_run());
    assert_eq!(frame_ms(&mut dec), 300);
    assert!(dec.next_late().unwrap().is_some_and(|(at, _)| at == 400));
}

#[test]
fn a_seek_back_behind_the_late_line_shows_its_frames() {
    let mut video = Video::new(20);
    video.polls = VecDeque::from([true, true, true, true, true, false]);
    let mut dec = decoder(video, 5000);
    for k in 0..5 {
        assert_eq!(frame_ms(&mut dec), k * 100);
    }
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::NotReady
    ));
    dec.next_late().unwrap().expect("a late step at 500");
    dec.seek(200).unwrap();
    assert_eq!(frame_ms(&mut dec), 200, "never dropped by the old line");
}

#[test]
fn the_fast_forward_survives_a_picture_that_is_not_ready() {
    let mut video = Video::new(20);
    // The seek to 700 lands on 500: 500 (discarded), not ready, 600
    // (discarded), not ready, then 700.
    video.polls = VecDeque::from([true, false, true, false, true]);
    let mut dec = decoder(video, 5000);
    dec.seek(700).unwrap();
    let mut answers = Vec::new();
    loop {
        match dec.next_synced_within(Some(Duration::ZERO)).unwrap() {
            Synced::NotReady => answers.push(None),
            Synced::Frame(v, _) => {
                answers.push(Some(v.timestamp_ms));
                break;
            }
            Synced::Ended => panic!("the video has frames"),
        }
    }
    assert_eq!(answers, [None, None, Some(700)]);
    assert!(dec.take_frame_before_end().is_none());
}

#[test]
fn a_seek_past_the_end_keeps_the_last_fast_forward_frame_across_a_not_ready() {
    let mut video = Video::new(5);
    video.polls = VecDeque::from([true, false]);
    let mut dec = decoder(video, 5000);
    // 900 asks the video for 400 (one frame before its end), it lands on
    // 200: 200 discarded, not ready, 300 and 400 discarded, end-of-stream.
    dec.seek(900).unwrap();
    while !matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::Ended
    ) {}
    let last = dec.take_frame_before_end().expect("the last frame decoded");
    assert_eq!(last.timestamp_ms, 400);
}

#[test]
fn a_held_tail_after_a_late_run_goes_on_from_the_last_step() {
    let mut dec = decoder(Video::new(2), 5000);
    assert_eq!(frame_ms(&mut dec), 0);
    dec.next_late().unwrap().expect("a late step at 100");
    dec.next_late().unwrap().expect("a late step at 200");
    // Frame 1 (100) is late, dropped; the video ends: the tail from 300.
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::Ended
    ));
    assert_eq!(dec.late_counts(), (2, 1));
    let (at, _) = dec.next_held().unwrap().expect("a held step");
    assert_eq!(at, 300);
}

#[test]
fn the_frame_period_follows_the_source_rate() {
    let mut video = Video::new(1);
    video.rate = (24_000, 1001);
    assert_eq!(decoder(video, 100).frame_period_ms(), 41);
    let mut video = Video::new(1);
    video.rate = (0, 0);
    assert_eq!(decoder(video, 100).frame_period_ms(), 33, "30 fps fallback");
    assert_eq!(decoder(Video::new(1), 100).frame_period_ms(), 100);
}

#[test]
fn next_synced_with_no_wait_blocks_and_never_says_not_ready() {
    let mut video = Video::new(2);
    video.polls = VecDeque::from([false, false]);
    let mut dec = decoder(video, 1000);
    // No wait: the plain read (the polls are never asked).
    let (v, _) = dec.next_synced().unwrap().expect("frame 0");
    assert_eq!(v.timestamp_ms, 0);
    let (v, _) = dec.next_synced().unwrap().expect("frame 1");
    assert_eq!(v.timestamp_ms, 100);
    assert!(dec.next_synced().unwrap().is_none());
}

#[test]
fn a_picture_away_for_good_fails_the_song_with_nothing_held() {
    // #318 review round 2: at the song's start nothing is held, so no late
    // step runs into its bound; the stall itself is bounded.
    let mut video = Video::new(5);
    video.polls = VecDeque::from(vec![false; 10_000]);
    let mut dec = decoder(video, 1000);
    dec.set_stall_limit(Duration::from_millis(30));
    for _ in 0..2_000 {
        match dec.next_synced_within(Some(Duration::ZERO)) {
            Ok(Synced::NotReady) => std::thread::sleep(Duration::from_millis(5)),
            Err(DecoderError::Decode(text)) => {
                assert!(text.contains("gave no picture"), "{text}");
                return;
            }
            other => panic!("not ready, then the failure: {other:?}"),
        }
    }
    panic!("a picture away for good never failed the song");
}

#[test]
fn a_picture_that_comes_restarts_the_stall_clock() {
    let mut video = Video::new(5);
    video.polls = VecDeque::from([false, true, false]);
    let mut dec = decoder(video, 1000);
    dec.set_stall_limit(Duration::from_millis(30));
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::NotReady
    ));
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        frame_ms(&mut dec),
        0,
        "a picture, past the limit: no failure"
    );
    assert!(
        matches!(
            dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
            Synced::NotReady
        ),
        "a new stall starts its own clock"
    );
}

#[test]
fn a_video_that_ends_during_a_late_run_ends_the_run() {
    let mut dec = decoder(Video::new(2), 5000);
    assert_eq!(frame_ms(&mut dec), 0);
    dec.next_late().unwrap().expect("a late step at 100");
    assert!(dec.in_late_run());
    // Frame 1 (100) lies on the line and is dropped, then the video ends.
    assert!(matches!(
        dec.next_synced_within(Some(Duration::ZERO)).unwrap(),
        Synced::Ended
    ));
    assert!(!dec.in_late_run(), "the run ended with the video");
    let (at, _) = dec.next_held().unwrap().expect("the tail goes on");
    assert_eq!(at, 200, "from the last late step");
}

/// The stall bound's edge (#318): a picture away exactly as long as the bound
/// is still within it, one nanosecond more fails the song.
#[test]
fn a_stall_exactly_at_the_bound_is_not_over_it() {
    let limit = Duration::from_millis(AUDIO_PAST_VIDEO_MAX_MS);
    assert!(!super::stall_over(limit, limit));
    assert!(super::stall_over(limit + Duration::from_nanos(1), limit));
    assert!(!super::stall_over(Duration::ZERO, limit));
}
