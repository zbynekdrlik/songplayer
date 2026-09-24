//! Media decoder for SongPlayer.
//!
//! This crate provides stream-oriented readers that plug into the playback
//! pipeline through the shared [`stream`] traits:
//!
//! * [`audio::SymphoniaAudioReader`] — pure-Rust FLAC decoder (cross-platform)
//! * [`video::mf_reader::MediaFoundationVideoReader`] — Windows-only video
//!   reader backed by Media Foundation.
//!
//! [`split_sync::SplitSyncedDecoder`] drives them with audio-as-master-clock.

mod error;
mod types;

pub mod audio;
pub mod frame_pool;
pub mod level_probe;
pub mod split_sync;
pub mod stream;

#[cfg(windows)]
pub mod video;

pub use audio::{StemMixReader, SymphoniaAudioReader, gain_from_bits, gain_to_bits, shared_gain};
pub use error::DecoderError;
pub use frame_pool::PooledBuf;
pub use level_probe::{LevelProbe, LevelReading, PROBE_INTERVAL};
pub use split_sync::SplitSyncedDecoder;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
