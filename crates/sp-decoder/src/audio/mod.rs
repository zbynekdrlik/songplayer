//! Cross-platform audio decoder (Symphonia-backed).

pub mod stem_mix;
pub mod symphonia_reader;

pub use stem_mix::{StemMixReader, gain_from_bits, gain_to_bits, shared_gain};
pub use symphonia_reader::SymphoniaAudioReader;
