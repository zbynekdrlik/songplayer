//! Trait-based decoder abstraction.
//!
//! The split-file pipeline opens the video and audio sidecars through two
//! separate readers. Each reader implements one of these traits, and
//! [`crate::split_sync::SplitSyncedDecoder`] drives both generically — which
//! makes mock-based unit tests possible on non-Windows platforms.

use crate::error::DecoderError;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Behaviour shared by every media stream reader.
pub trait MediaStream {
    /// Total duration of the stream in milliseconds.
    fn duration_ms(&self) -> u64;

    /// Seek to the given position (in ms). Precision is format-dependent.
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError>;
}

/// What [`VideoStream::poll_frame`] found (#318).
#[derive(Debug)]
pub enum FramePoll {
    /// The next frame, or `None` at end-of-stream ([`VideoStream::next_frame`]'s answer).
    Ready(Option<DecodedVideoFrame>),
    /// No frame within the wait: the read is still running (a reader on its
    /// own thread, [`crate::video_thread::ThreadedVideo`]).
    NotReady,
}

/// A reader that produces decoded video frames.
pub trait VideoStream: MediaStream + Send {
    /// Pull the next decoded frame. Returns `Ok(None)` at end-of-stream.
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError>;

    /// The next frame if it is ready within `wait` (#318). A reader that
    /// decodes on the caller's thread is always ready: it reads the frame
    /// ([`next_frame`](Self::next_frame)), however long that takes. Only a
    /// reader on its own thread answers [`FramePoll::NotReady`].
    fn poll_frame(&mut self, wait: std::time::Duration) -> Result<FramePoll, DecoderError> {
        let _ = wait;
        Ok(FramePoll::Ready(self.next_frame()?))
    }

    /// Frame width in pixels.
    fn width(&self) -> u32;

    /// Frame height in pixels.
    fn height(&self) -> u32;

    /// Frame rate as (numerator, denominator).
    fn frame_rate(&self) -> (u32, u32);
}

/// A reader that produces decoded audio samples.
pub trait AudioStream: MediaStream + Send {
    /// Pull the next chunk of decoded samples. Returns `Ok(None)` at EOS.
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError>;

    /// Sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Channel count (1 = mono, 2 = stereo).
    fn channels(&self) -> u16;
}
