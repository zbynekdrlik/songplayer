//! #277: a video whose picture stops long before its own duration ends the
//! song as a FAILURE: no held tail, `early_end` says where and of how long.
//! The mock video reports the duration its container would, apart from its
//! frames (Media Foundation reported 250 965 ms for "Remembrance" and handed
//! out no picture).

use sp_decoder::{DecodedVideoFrame, DecoderError, MediaStream, SplitSyncedDecoder, VideoStream};

use super::{Audio, SeekAnswer, Video, every, items, shape, taps};
use crate::playback::early_end::EarlyEnd;
use crate::playback::paced_producer::DecodeProducer;

/// [`Video`] whose container says it is `duration_ms` long.
struct Long {
    frames: Video,
    duration_ms: u64,
}

impl MediaStream for Long {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.frames.seek(position_ms)
    }
}

impl VideoStream for Long {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.frames.next_frame()
    }
    fn width(&self) -> u32 {
        self.frames.width()
    }
    fn height(&self) -> u32 {
        self.frames.height()
    }
    fn frame_rate(&self) -> (u32, u32) {
        self.frames.frame_rate()
    }
}

/// A decoder over frames at `video` ms in a video `duration_ms` long, and
/// chunks at `audio` ms.
fn long(video: &[u64], duration_ms: u64, audio: &[u64]) -> SplitSyncedDecoder {
    SplitSyncedDecoder::new(
        Box::new(Long {
            frames: Video {
                ms: video.to_vec(),
                next: 0,
                seeks: SeekAnswer::Lands,
            },
            duration_ms,
        }),
        Box::new(Audio {
            ms: audio.to_vec(),
            next: 0,
        }),
    )
    .expect("valid mock readers")
}

#[test]
fn a_video_with_no_picture_of_250_s_ends_early_at_once() {
    // The observed case: Media Foundation opens the file, hands out no
    // picture and reports end-of-stream.
    let mut dec = long(&[], 250_965, &every(0, 300, 100));
    let mut p = DecodeProducer::new(0);
    assert!(p.next_item(&mut dec, &taps()).unwrap().is_none());
    assert_eq!(
        p.early_end(),
        Some(EarlyEnd {
            at_ms: 0,
            duration_ms: 250_965
        })
    );
}

#[test]
fn an_end_at_0_1_s_of_a_250_s_video_is_an_early_end_with_no_tail() {
    // Two pictures (0 and 100 ms), then end-of-stream in a 250 s video: the
    // song fails at its last picture; no held step carries the audio on.
    let mut dec = long(&[0, 100], 250_000, &every(0, 900, 100));
    let mut p = DecodeProducer::new(0);
    let shapes: Vec<_> = items(&mut p, &mut dec).iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![(0, 0, vec![0], false, 1), (100, 100, vec![100], false, 2)]
    );
    assert_eq!(
        p.early_end(),
        Some(EarlyEnd {
            at_ms: 100,
            duration_ms: 250_000
        })
    );
    assert!(
        p.next_item(&mut dec, &taps()).unwrap().is_none(),
        "still over"
    );
}

#[test]
fn an_end_exactly_ten_seconds_before_the_duration_is_the_natural_end() {
    // The last picture 10 s before the end: the natural end, so the held
    // tail plays the audio on (the step at 240 100 ms).
    let mut dec = long(&[0, 240_000], 250_000, &[0, 240_000, 240_100]);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..2 {
        p.next_item(&mut dec, &taps).unwrap().expect("a picture");
    }
    let (_, ms, _) = p.next_item(&mut dec, &taps).unwrap().expect("a held step");
    assert_eq!(ms, 240_100);
    assert_eq!(p.early_end(), None);
}

#[test]
fn an_end_a_millisecond_earlier_is_an_early_end() {
    let mut dec = long(&[0, 239_999], 250_000, &[0, 239_999, 240_100]);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..2 {
        p.next_item(&mut dec, &taps).unwrap().expect("a picture");
    }
    assert!(p.next_item(&mut dec, &taps).unwrap().is_none());
    assert_eq!(
        p.early_end(),
        Some(EarlyEnd {
            at_ms: 239_999,
            duration_ms: 250_000
        })
    );
}

#[test]
fn a_seek_into_a_broken_file_ends_early_at_the_last_frame_it_decoded() {
    // A seek to 150 s lands on keyframe 0; the fast-forward decodes 0, 100
    // and 200 ms, all before the target, then end-of-stream: the picture
    // stopped at 200 ms (the frame before the end), 250 s long.
    let mut dec = long(&[0, 100, 200], 250_000, &every(0, 300, 100));
    let mut p = DecodeProducer::new(0);
    p.next_item(&mut dec, &taps()).unwrap().expect("frame 0");
    assert_eq!(p.seek(&mut dec, 150_000, 1, 1), 150_000);
    assert!(p.next_item(&mut dec, &taps()).unwrap().is_none());
    assert_eq!(
        p.early_end(),
        Some(EarlyEnd {
            at_ms: 200,
            duration_ms: 250_000
        })
    );
}

#[test]
fn a_seek_clears_an_early_end_and_the_video_plays_again() {
    let mut dec = long(&[0, 100], 250_000, &every(0, 300, 100));
    let mut p = DecodeProducer::new(0);
    while p.next_item(&mut dec, &taps()).unwrap().is_some() {}
    assert!(p.early_end().is_some());
    assert_eq!(p.seek(&mut dec, 0, 2, 1), 0);
    assert_eq!(p.early_end(), None, "the video plays again from the seek");
    let (_, ms, _) = p.next_item(&mut dec, &taps()).unwrap().expect("frame 0");
    assert_eq!(ms, 0);
}
