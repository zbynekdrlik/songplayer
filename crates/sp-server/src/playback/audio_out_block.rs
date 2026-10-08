//! #233: one program boundary's audio as every output receives it (#210's
//! `VbanBlock`, renamed and shared). `ProgramOutput::serve` makes ONE copy of
//! the limited block (`copied`: the NDI submit still borrows the pair after
//! the hand-off) and the fan-out (`audio_out.rs`) hands each output an `Arc`
//! of it. Anything that is not one 48 kHz stereo 1600-frame block is sent as
//! silence and marked `substituted`.

use std::sync::Arc;

use sp_ndi::AudioFrame;

use crate::playback::vban_packet::{VBAN_BLOCK_SAMPLES, VBAN_CHANNELS, VBAN_SAMPLE_RATE_HZ};

/// One program boundary's audio for the outputs.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgramBlock {
    /// The boundary the block belongs to (the pair's video stamp, 100 ns).
    pub due_100ns: i64,
    /// 3200 interleaved stereo samples; `None` = silence.
    pub samples: Option<Arc<[f32]>>,
    /// The pair's audio was not one program block and is sent as silence.
    pub substituted: bool,
}

impl ProgramBlock {
    /// The program's standby silence for `due_100ns`.
    pub fn silence(due_100ns: i64) -> Self {
        Self {
            due_100ns,
            samples: None,
            substituted: false,
        }
    }

    /// A pair's audio, COPIED once into a shared block: the program hands it
    /// over BEFORE the pair's NDI submit, which still borrows the frames
    /// (#210). Exactly one 48 kHz stereo 1600-frame frame is kept; anything
    /// else becomes silence, marked `substituted`.
    pub fn copied(due_100ns: i64, frames: &[AudioFrame]) -> Self {
        let samples: Option<Arc<[f32]>> = match frames {
            [frame] if is_program_block(frame) => Some(Arc::from(frame.data.as_slice())),
            _ => None,
        };
        Self {
            due_100ns,
            substituted: samples.is_none(),
            samples,
        }
    }
}

/// `frame` is one program audio block: 48 kHz, stereo, 1600 frames.
pub fn is_program_block(frame: &AudioFrame) -> bool {
    frame.channels as usize == VBAN_CHANNELS
        && i64::from(frame.sample_rate) == VBAN_SAMPLE_RATE_HZ
        && frame.data.len() == VBAN_BLOCK_SAMPLES
}
