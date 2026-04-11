//! Media decoder for SongPlayer.
//!
//! This crate provides two stream-oriented readers that plug into the
//! playback pipeline through the shared [`stream`] traits:
//!
//! * [`audio::SymphoniaAudioReader`] — pure-Rust FLAC decoder (cross-platform)
//! * [`video::mf_reader::MediaFoundationVideoReader`] — Windows-only video
//!   reader backed by Media Foundation (added in a later task).
//!
//! [`split_sync::SplitSyncedDecoder`] drives them with audio-as-master-clock.
//!
//! The legacy [`MediaReader`] and [`SyncedDecoder`] types remain available
//! until the downloader and playback pipeline have fully migrated; they are
//! removed in task 10 of the FLAC migration plan.

mod error;
mod types;

pub mod audio;
pub mod split_sync;
pub mod stream;

#[cfg(windows)]
mod reader;
#[cfg(windows)]
mod sync;
#[cfg(windows)]
pub mod video;

pub use audio::SymphoniaAudioReader;
pub use error::DecoderError;
pub use split_sync::SplitSyncedDecoder;
pub use stream::{AudioStream, MediaStream, VideoStream};
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat, VideoStreamInfo};

#[cfg(windows)]
pub use reader::MediaReader;
#[cfg(windows)]
pub use sync::SyncedDecoder;
#[cfg(windows)]
pub use video::MediaFoundationVideoReader;
