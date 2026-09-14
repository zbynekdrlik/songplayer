//! Cross-platform audio decoder (Symphonia-backed).

pub mod karaoke;
pub mod symphonia_reader;

pub use karaoke::{KaraokeAudioReader, gain_to_bits, shared_gain};
pub use symphonia_reader::SymphoniaAudioReader;
