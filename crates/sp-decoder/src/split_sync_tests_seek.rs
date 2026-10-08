//! #217 review round 1: a seek the video reader refuses moves NOTHING — the
//! audio is not sought before it, so the pipeline's report (`real_seek_ms`:
//! a refused seek plays on from where the decoder was) holds for both
//! streams, never the audio at the new position and the video at the old.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::stream::MediaStream;

/// A video reader whose seek is refused (a broken index, an MF error).
struct RefusingVideo;

impl MediaStream for RefusingVideo {
    fn duration_ms(&self) -> u64 {
        1_000
    }
    fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
        Err(DecoderError::Seek("refused".into()))
    }
}

impl VideoStream for RefusingVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        Ok(None)
    }
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (30, 1)
    }
}

/// An audio reader that counts its seeks.
struct CountingAudio(Arc<AtomicU64>);

impl MediaStream for CountingAudio {
    fn duration_ms(&self) -> u64 {
        1_000
    }
    fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl AudioStream for CountingAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Ok(None)
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

#[test]
fn a_refused_video_seek_leaves_the_audio_where_it_was() {
    let audio_seeks = Arc::new(AtomicU64::new(0));
    let mut dec = SplitSyncedDecoder::new(
        Box::new(RefusingVideo),
        Box::new(CountingAudio(Arc::clone(&audio_seeks))),
    )
    .unwrap();
    dec.pending_audio = VecDeque::from([DecodedAudioFrame {
        data: vec![0.0; 4],
        channels: 2,
        sample_rate: 48_000,
        timestamp_ms: 10,
    }]);
    assert!(dec.seek(500).is_err(), "the video refused");
    assert_eq!(
        audio_seeks.load(Ordering::SeqCst),
        0,
        "the audio was not sought"
    );
    assert_eq!(dec.pending_audio.len(), 1, "nor its buffer dropped");
    assert_eq!(dec.pending_video_target_ms, None, "no fast-forward target");
}
