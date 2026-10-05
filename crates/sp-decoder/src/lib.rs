//! Media decoder for SongPlayer.
//!
//! This crate provides stream-oriented readers that plug into the playback
//! pipeline through the shared [`stream`] traits:
//!
//! * [`audio::SymphoniaAudioReader`] — pure-Rust FLAC decoder (cross-platform)
//! * [`video::mf_reader::MediaFoundationVideoReader`] — Windows-only video
//!   reader backed by Media Foundation, in software or (#223 S3b, opt-in) on
//!   the GPU ([`hw_decode`]).
//!
//! [`split_sync::SplitSyncedDecoder`] drives them with audio-as-master-clock.

mod error;
mod types;

pub mod audio;
pub mod frame_pool;
pub mod hw_decode; // #223 S3b: the hardware decode decisions (pure, Linux-tested)
pub mod level_probe;
mod peak_limiter; // #184: the stem mix's peak limiter (pure, mutation-tested), #210: the program's too
pub mod split_sync;
pub mod stream;
pub mod subtype; // #223 S0: an MF video subtype GUID as codec text (AV01, VP90, H264)

#[cfg(windows)]
pub mod video;

pub use audio::{
    StemMixReader, SymphoniaAudioReader, format_gains, gain_from_bits, gain_to_bits, gains_id,
    shared_gain,
};
pub use error::DecoderError;
pub use frame_pool::PooledBuf;
pub use hw_decode::{
    DecodeMode, DecodePath, FallbackStage, HwDecodeStats, HwFallback, hw_counters,
};
pub use level_probe::{LevelProbe, LevelReading, PROBE_INTERVAL};
pub use peak_limiter::{LIMIT_CEILING, PeakLimiter};
pub use split_sync::SplitSyncedDecoder;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
