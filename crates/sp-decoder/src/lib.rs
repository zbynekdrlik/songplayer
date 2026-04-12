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
pub mod split_sync;
pub mod stream;

#[cfg(windows)]
pub mod video;

pub use audio::SymphoniaAudioReader;
pub use error::DecoderError;
pub use split_sync::SplitSyncedDecoder;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
