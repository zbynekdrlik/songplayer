//! #257: the audio past the video's end goes out on held steps.
//!
//! A song ends when its AUDIO has played. When the picture ends first, the
//! decoder keeps handing the audio out in steps on the video's own frame
//! cadence (`next_held`): each step is the time the held last frame shows at,
//! with the audio up to that time plus the lead, exactly as a frame takes its
//! own. The steps stop once the audio ended or at `AUDIO_PAST_VIDEO_MAX_MS`
//! past their base; a seek starts them anew. A seek to or past the last frame
//! delivers no frame, and the last frame its fast-forward decoded is the
//! picture the audio plays on (`take_frame_before_end`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::stream::MediaStream;
use crate::types::PixelFormat;

/// A video reader of frames at the given ms, at 10 fps, that lands a seek on
/// the last keyframe at or before the position (every `key_every`-th frame,
/// like Media Foundation) and counts its reads.
struct Video {
    ms: Vec<u64>,
    key_every: usize,
    next: usize,
    reads: Arc<AtomicU64>,
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0)
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let at_or_before = self.ms.iter().take_while(|&&m| m <= position_ms).count();
        self.next = at_or_before.saturating_sub(1) / self.key_every * self.key_every;
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let Some(&ms) = self.ms.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        Ok(Some(DecodedVideoFrame {
            data: vec![0u8; 6],
            width: 2,
            height: 2,
            stride: 2,
            timestamp_ms: ms,
            pixel_format: PixelFormat::Nv12,
            gpu: None,
        }))
    }
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (10, 1)
    }
}

/// An audio reader of chunks at the given ms that lands a seek on the first
/// chunk at or after the position (sample-accurate, like the FLAC reader).
struct Audio {
    ms: Vec<u64>,
    next: usize,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0)
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.next = self.ms.iter().take_while(|&&m| m < position_ms).count();
        Ok(())
    }
}

impl AudioStream for Audio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let Some(&ms) = self.ms.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        Ok(Some(DecodedAudioFrame {
            data: vec![0.0; 4],
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

/// A decoder (40 ms pairing) over frames and chunks at the given ms, a
/// keyframe every `key_every` frames, and the video reader's read count.
fn decoder(video: &[u64], key_every: usize, audio: &[u64]) -> (SplitSyncedDecoder, Arc<AtomicU64>) {
    let reads = Arc::new(AtomicU64::new(0));
    let dec = SplitSyncedDecoder::new(
        Box::new(Video {
            ms: video.to_vec(),
            key_every,
            next: 0,
            reads: Arc::clone(&reads),
        }),
        Box::new(Audio {
            ms: audio.to_vec(),
            next: 0,
        }),
    )
    .expect("valid mock readers");
    (dec, reads)
}

/// `from..=to` in steps of `step` ms.
fn every(from: u64, to: u64, step: u64) -> Vec<u64> {
    (from..=to).step_by(step as usize).collect()
}

/// The frames' timestamps until the video ends.
fn play_frames(dec: &mut SplitSyncedDecoder) -> Vec<u64> {
    std::iter::from_fn(|| {
        dec.next_synced()
            .expect("mocks never fail")
            .map(|(v, _)| v.timestamp_ms)
    })
    .collect()
}

/// One held step: its time, the timestamps of its chunks, and whether the
/// audio ended with it.
type Step = (u64, Vec<u64>, bool);

/// Every held step until the decoder hands out none (bounded: a mutant that
/// never stops fails the bound instead of hanging).
fn held_steps(dec: &mut SplitSyncedDecoder) -> Vec<Step> {
    let mut steps = Vec::new();
    for _ in 0..1_000 {
        let Some((at, audio)) = dec.next_held().expect("mocks never fail") else {
            return steps;
        };
        let chunks = audio.iter().map(|a| a.timestamp_ms).collect();
        steps.push((at, chunks, dec.audio_ended()));
    }
    panic!("the held steps never stopped: {} steps", steps.len());
}

#[test]
fn the_audio_past_the_video_goes_out_on_held_steps_at_the_frame_cadence() {
    // Frames 0, 100, 200 at 10 fps; chunks every 100 ms to 700. The last
    // frame takes chunk 200 (deadline 240) and leaves 300 waiting. Then one
    // step per frame period from the last frame: 300 … 700, each with its
    // chunk; the 700 step reads the reader's end, so the audio ended with it.
    let (mut dec, _) = decoder(&[0, 100, 200], 1, &every(0, 700, 100));
    assert_eq!(play_frames(&mut dec), vec![0, 100, 200]);
    assert!(!dec.audio_ended(), "chunk 300 waits past the last frame");
    assert_eq!(
        held_steps(&mut dec),
        vec![
            (300, vec![300], false),
            (400, vec![400], false),
            (500, vec![500], false),
            (600, vec![600], false),
            (700, vec![700], true),
        ]
    );
    assert!(dec.audio_ended());
    assert!(
        dec.next_held().unwrap().is_none(),
        "nothing past the audio's end"
    );
}

#[test]
fn the_held_steps_stop_at_the_bound_and_never_read_the_audio_past_it() {
    // One frame at 0; 20 s of chunks every 100 ms. The steps run every
    // 100 ms up to EXACTLY 15 000 ms past the frame (the bound itself is
    // served), then stop: chunks past it are never read, so the audio has
    // not ended (the pacer counts that song's end as one `underrun_end`).
    let (mut dec, _) = decoder(&[0], 1, &every(0, 20_000, 100));
    assert_eq!(play_frames(&mut dec), vec![0]);
    let steps = held_steps(&mut dec);
    assert_eq!(steps.len(), 150, "a step per 100 ms up to 15 s");
    assert_eq!(steps.last().map(|s| s.0), Some(15_000));
    let handed: Vec<u64> = steps.iter().flat_map(|s| s.1.clone()).collect();
    assert_eq!(handed, every(100, 15_000, 100), "each step its own chunk");
    assert!(steps.iter().all(|s| !s.2), "the audio never ended");
    assert!(!dec.audio_ended());
    assert!(
        dec.next_held().unwrap().is_none(),
        "past the bound, never again"
    );
}

#[test]
fn no_held_step_while_the_video_plays() {
    // Before the video ended there is no held step, even with audio left;
    // after the audio ended there is none either.
    let (mut dec, _) = decoder(&[0, 100], 1, &every(0, 300, 100));
    assert!(dec.next_held().unwrap().is_none(), "nothing read yet");
    assert!(dec.next_synced().unwrap().is_some());
    assert!(dec.next_held().unwrap().is_none(), "frame 100 is still due");
    assert_eq!(play_frames(&mut dec), vec![100]);
    assert_eq!(
        held_steps(&mut dec),
        vec![(200, vec![200], false), (300, vec![300], true)]
    );
}

#[test]
fn a_seek_past_the_last_frame_holds_that_frame_from_the_seek_position() {
    // Frames 0 … 900, a keyframe every third frame (0, 300, 600, 900);
    // chunks every 50 ms to 2000. A seek to 1250 lands the video on keyframe
    // 900, which the fast-forward decodes and drops; the video then ends with
    // no frame delivered. Frame 900 is the picture the audio plays on, and
    // the steps start ON the seek's position, one frame period apart.
    let (mut dec, _) = decoder(&every(0, 900, 100), 3, &every(0, 2_000, 50));
    dec.seek(1_250).expect("mocks seek");
    assert!(dec.next_synced().unwrap().is_none(), "no frame past 1250");
    let held = dec.take_frame_before_end().expect("the last frame decoded");
    assert_eq!(held.timestamp_ms, 900);
    assert!(dec.take_frame_before_end().is_none(), "handed out once");
    let steps = held_steps(&mut dec);
    assert_eq!(
        steps[..3],
        [
            (1_250, vec![1_250], false),
            (1_350, vec![1_300, 1_350], false),
            (1_450, vec![1_400, 1_450], false),
        ]
    );
    // Chunk 2000 lies past the 1950 step's deadline (1990); the 2050 step
    // takes it and reads the reader's end.
    assert_eq!(steps.len(), 9, "1250 … 2050");
    assert_eq!(steps.last(), Some(&(2_050, vec![2_000], true)));
}

#[test]
fn a_frame_delivered_after_a_seek_leaves_no_frame_before_end() {
    // A seek to 450 lands on keyframe 300 and fast-forwards to 500: a frame
    // is delivered, so there is no picture kept from the seek, neither then
    // nor at the video's natural end. The steps then follow frame 900.
    let (mut dec, _) = decoder(&every(0, 900, 100), 3, &every(0, 1_200, 100));
    dec.seek(450).expect("mocks seek");
    assert_eq!(play_frames(&mut dec), every(500, 900, 100));
    assert!(dec.take_frame_before_end().is_none());
    let steps = held_steps(&mut dec);
    assert_eq!(steps.first().map(|s| s.0), Some(1_000));
    assert_eq!(steps.last(), Some(&(1_200, vec![1_200], true)));
}

#[test]
fn the_video_is_not_read_again_after_its_end_until_a_seek() {
    // Media Foundation is never asked for a frame past its end: after the
    // end-of-stream read, `next_synced` answers from the decoder until a
    // seek starts the video again.
    let (mut dec, reads) = decoder(&[0, 100], 1, &every(0, 100, 100));
    assert_eq!(play_frames(&mut dec), vec![0, 100]);
    assert_eq!(reads.load(Ordering::SeqCst), 3, "two frames + the end");
    for _ in 0..3 {
        assert!(dec.next_synced().unwrap().is_none());
    }
    assert_eq!(reads.load(Ordering::SeqCst), 3, "no read past the end");
    dec.seek(0).expect("mocks seek");
    assert_eq!(play_frames(&mut dec), vec![0, 100]);
    assert_eq!(reads.load(Ordering::SeqCst), 6);
}

#[test]
fn a_seek_back_starts_the_held_steps_anew_from_the_new_last_frame() {
    // Played to the end and two steps into the tail; a seek back to 100
    // plays frames 100 and 200 again, and the steps start again one frame
    // after 200 (not where the earlier tail stopped).
    let (mut dec, _) = decoder(&[0, 100, 200], 1, &every(0, 1_000, 100));
    assert_eq!(play_frames(&mut dec), vec![0, 100, 200]);
    assert_eq!(dec.next_held().unwrap().map(|s| s.0), Some(300));
    assert_eq!(dec.next_held().unwrap().map(|s| s.0), Some(400));
    dec.seek(100).expect("mocks seek");
    assert_eq!(play_frames(&mut dec), vec![100, 200]);
    let (at, audio) = dec.next_held().unwrap().expect("a step");
    assert_eq!(at, 300);
    let chunks: Vec<u64> = audio.iter().map(|a| a.timestamp_ms).collect();
    assert_eq!(chunks, vec![300]);
}

#[test]
fn held_offsets_follow_the_frame_cadence_exactly() {
    assert_eq!(held_offset_ms(3, (10, 1)), 300);
    // 23.976 fps: 3 · 1000 · 1001 / 24000 = 125.125, floored.
    assert_eq!(held_offset_ms(3, (24_000, 1_001)), 125);
    // 29.97 fps, from the base every time: no drift over a long tail.
    assert_eq!(held_offset_ms(1, (30_000, 1_001)), 33);
    assert_eq!(held_offset_ms(450, (30_000, 1_001)), 15_015);
    assert_eq!(held_offset_ms(0, (25, 1)), 0);
    // A rate with a zero term holds at 30 fps.
    assert_eq!(held_offset_ms(3, (0, 0)), 100);
    assert_eq!(held_offset_ms(3, (0, 1)), 100);
    assert_eq!(held_offset_ms(3, (30, 0)), 100);
}

#[test]
fn a_rate_outside_10_to_120_fps_holds_at_30_fps() {
    // #257 review: Media Foundation can report a broken rate. 1000/1 would
    // push 15 000 steps into the 15 s bound; under 0.6 fps one step would
    // carry over 2 s of audio, past the grid's 2 s cap. The edges hold their
    // own cadence, whatever the terms.
    assert_eq!(held_offset_ms(3, (10, 1)), 300, "10 fps itself");
    assert_eq!(held_offset_ms(3, (100, 10)), 300, "10 fps as 100/10");
    assert_eq!(held_offset_ms(3, (9, 1)), 100, "9 fps: 30 fps");
    assert_eq!(held_offset_ms(3, (99, 10)), 100, "9.9 fps: 30 fps");
    assert_eq!(held_offset_ms(3, (120, 1)), 25, "120 fps itself");
    assert_eq!(held_offset_ms(3, (1_200, 10)), 25, "120 fps as 1200/10");
    assert_eq!(held_offset_ms(3, (1_201, 10)), 100, "120.1 fps: 30 fps");
    assert_eq!(held_offset_ms(3, (121, 1)), 100, "121 fps: 30 fps");
    assert_eq!(held_offset_ms(3, (1_000, 1)), 100, "1000 fps: 30 fps");
    assert_eq!(held_offset_ms(3, (1, 2)), 100, "0.5 fps: 30 fps");
}

/// How a Media Foundation-like reader answers a seek past its last frame
/// (#257 review): it may refuse the position, or land at end-of-stream.
#[derive(Clone, Copy, Debug)]
enum PastEnd {
    Refuse,
    Eof,
}

/// A video reader like Media Foundation's: frames every 100 ms from 0 to
/// `last_ms` (10 fps), a keyframe every 300 ms, the container's duration
/// `duration_ms`, and a seek past the last frame answered as `past`. It
/// records every position it is asked to seek to.
struct MfVideo {
    last_ms: u64,
    duration_ms: u64,
    next: Option<u64>,
    past: PastEnd,
    seeks: Arc<Mutex<Vec<u64>>>,
}

impl MediaStream for MfVideo {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.seeks.lock().unwrap().push(position_ms);
        if position_ms <= self.last_ms {
            self.next = Some(position_ms / 300 * 300);
            return Ok(());
        }
        match self.past {
            PastEnd::Refuse => Err(DecoderError::Seek("past the end".into())),
            PastEnd::Eof => {
                self.next = None;
                Ok(())
            }
        }
    }
}

impl VideoStream for MfVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let Some(ms) = self.next.filter(|&ms| ms <= self.last_ms) else {
            return Ok(None);
        };
        self.next = Some(ms + 100);
        Ok(Some(DecodedVideoFrame {
            data: vec![0u8; 6],
            width: 2,
            height: 2,
            stride: 2,
            timestamp_ms: ms,
            pixel_format: PixelFormat::Nv12,
            gpu: None,
        }))
    }
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (10, 1)
    }
}

/// A decoder over the Media Foundation-like video (frames 0 … 900, its
/// duration `duration_ms`, a seek past 900 answered as `past`) and chunks
/// every 50 ms to 2000, and the positions the video was asked to seek to.
fn mf_decoder(duration_ms: u64, past: PastEnd) -> (SplitSyncedDecoder, Arc<Mutex<Vec<u64>>>) {
    let seeks = Arc::new(Mutex::new(Vec::new()));
    let dec = SplitSyncedDecoder::new(
        Box::new(MfVideo {
            last_ms: 900,
            duration_ms,
            next: Some(0),
            past,
            seeks: Arc::clone(&seeks),
        }),
        Box::new(Audio {
            ms: every(0, 2_000, 50),
            next: 0,
        }),
    )
    .expect("valid mock readers");
    (dec, seeks)
}

#[test]
fn a_seek_past_the_videos_end_asks_the_video_for_one_frame_before_it() {
    // Review: a Pause in the held tail stores a position past the last
    // frame, and ▶ resumes there. Asked for 1250 itself, Media Foundation
    // may refuse (the song would play on from where it was) or land at its
    // end with no frame (no picture to hold: the tail would never play). The
    // decoder asks the VIDEO for its duration less one frame (1000 − 100),
    // and keeps the fast-forward and the audio on 1250: frame 900 is held
    // from 1250 on, exactly as when the reader lands on a keyframe itself.
    for past in [PastEnd::Refuse, PastEnd::Eof] {
        let (mut dec, seeks) = mf_decoder(1_000, past);
        dec.seek(1_250)
            .expect("the video is asked a position it has");
        assert_eq!(*seeks.lock().unwrap(), vec![900], "{past:?}");
        assert!(dec.next_synced().unwrap().is_none(), "no frame past 1250");
        let held = dec.take_frame_before_end().expect("the last frame decoded");
        assert_eq!(held.timestamp_ms, 900, "{past:?}");
        let steps = held_steps(&mut dec);
        assert_eq!(steps.len(), 9, "1250 … 2050, {past:?}");
        assert_eq!(steps.first(), Some(&(1_250, vec![1_250], false)));
        assert_eq!(steps.last(), Some(&(2_050, vec![2_000], true)));
    }
}

#[test]
fn the_video_is_asked_at_most_its_duration_less_one_frame() {
    // A position inside the video goes through as asked; from one frame
    // before the duration on, the video is asked for that frame.
    let cases = [
        (0, 0),
        (500, 500),
        (899, 899),
        (900, 900),
        (901, 900),
        (1_000, 900),
        (5_000, 900),
    ];
    for (asked, video) in cases {
        let (mut dec, seeks) = mf_decoder(1_000, PastEnd::Refuse);
        dec.seek(asked)
            .expect("the video is asked a position it has");
        assert_eq!(*seeks.lock().unwrap(), vec![video], "asked {asked}");
    }
}

#[test]
fn a_video_of_unknown_duration_is_asked_the_position_itself() {
    // A reader that reports no duration gives nothing to clamp to: the
    // position goes through as asked, and the reader's refusal is the
    // seek's.
    let (mut dec, seeks) = mf_decoder(0, PastEnd::Refuse);
    assert!(dec.seek(1_250).is_err(), "the video refused 1250");
    assert_eq!(*seeks.lock().unwrap(), vec![1_250]);
}
