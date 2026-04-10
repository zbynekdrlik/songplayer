//! Windows Media Foundation video/audio decoder for SongPlayer.
//!
//! On Windows this crate provides [`MediaReader`] (low-level MF source reader)
//! and [`SyncedDecoder`] (A/V synchronised frame iterator).
//!
//! On non-Windows platforms only the error and frame types are exported so that
//! consuming crates can still compile cross-platform.

mod error;
mod types;

#[cfg(windows)]
mod reader;
#[cfg(windows)]
mod sync;

pub use error::DecoderError;
pub use types::{DecodedAudioFrame, DecodedVideoFrame, PixelFormat};

#[cfg(windows)]
pub use reader::MediaReader;
#[cfg(windows)]
pub use sync::SyncedDecoder;
