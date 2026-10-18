//! #257: `audio_ended` — every audio sample of the song has been handed out
//! with a frame. The paced producer passes it on with each frame, and the
//! pacer then knows that a block cut short is the song's own end, not an
//! underrun.

use super::*;
use crate::stream::MediaStream;
use crate::types::PixelFormat;

/// A video reader over integer-ms timestamps that rewinds on a seek.
struct Video {
    ms: Vec<u64>,
    next: usize,
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0)
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.next = self.ms.iter().take_while(|&&m| m < position_ms).count();
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
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

/// An audio reader of chunks at the given timestamps that rewinds on a seek.
struct Audio {
    ms: Vec<u64>,
    next: usize,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0) + 100
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

/// A decoder (40 ms pairing) over frames and chunks at the given ms.
fn decoder(video: &[u64], audio: &[u64]) -> SplitSyncedDecoder {
    SplitSyncedDecoder::new(
        Box::new(Video {
            ms: video.to_vec(),
            next: 0,
        }),
        Box::new(Audio {
            ms: audio.to_vec(),
            next: 0,
        }),
    )
    .expect("valid mock readers")
}

/// The timestamp of the next frame and whether the audio ended after it.
fn step(dec: &mut SplitSyncedDecoder) -> Option<(u64, bool)> {
    let (video, _audio) = dec.next_synced().expect("mocks never fail")?;
    Some((video.timestamp_ms, dec.audio_ended()))
}

#[test]
fn the_audio_ends_on_the_frame_that_takes_its_last_chunk() {
    // Chunks at 0, 100, 200, 300; frames every 100 ms with a 40 ms deadline.
    // Frame 300 takes the last chunk (300) and the reader then returns
    // end-of-stream: from that frame on the audio has ended.
    let mut dec = decoder(&[0, 100, 200, 300, 400], &[0, 100, 200, 300]);
    assert!(
        !dec.audio_ended(),
        "nothing read yet: an empty queue is no end"
    );
    let steps: Vec<(u64, bool)> = std::iter::from_fn(|| step(&mut dec)).collect();
    assert_eq!(
        steps,
        vec![
            (0, false),
            (100, false),
            (200, false),
            (300, true),
            (400, true)
        ]
    );
    assert!(dec.audio_ended(), "still ended after the video's end");
}

#[test]
fn the_audio_has_not_ended_when_the_video_ends_first() {
    // The video stops at 100 ms; the audio still holds chunks past the last
    // frame's deadline, which are never read.
    let mut dec = decoder(&[0, 100], &[0, 100, 200, 300, 400]);
    let steps: Vec<(u64, bool)> = std::iter::from_fn(|| step(&mut dec)).collect();
    assert_eq!(steps, vec![(0, false), (100, false)]);
    assert!(
        !dec.audio_ended(),
        "chunk 200 waits, 300 and 400 are unread"
    );
}

#[test]
fn a_seek_back_clears_the_audio_end_until_the_audio_ends_again() {
    let mut dec = decoder(&[0, 100, 200], &[0, 100, 200]);
    while step(&mut dec).is_some() {}
    assert!(dec.audio_ended());
    dec.seek(100).expect("mocks seek");
    assert!(!dec.audio_ended(), "the seek re-reads the audio from 100");
    assert_eq!(step(&mut dec), Some((100, false)));
    assert_eq!(step(&mut dec), Some((200, true)));
}

#[test]
fn the_video_duration_is_the_video_readers_own() {
    // #277: an early end of the picture is measured against the video's own
    // length, never the audio's (the master clock): the video here is 700 ms
    // long, the audio 200 ms.
    let dec = decoder(&[0, 100, 700], &[0, 100]);
    assert_eq!(dec.video_duration_ms(), 700);
    assert_eq!(dec.duration_ms(), 200, "the master clock is the audio's");
}
