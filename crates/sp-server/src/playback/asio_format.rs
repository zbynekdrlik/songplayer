//! #233: an ASIO driver's sample types (ASIOSampleType, little-endian only:
//! the big-endian ones exist on old Macs, Float64 and DSD on no driver we
//! use) and the program's L/R written into them. Integers use a SYMMETRIC
//! full scale (±(2^(bits−1) − 1)), like VBAN's INT24; a value is clamped to
//! ±1.0 and a non-finite one is silence (`vban_packet::clean_f32`). Every
//! channel but the two configured ones is zeroed; frames an underrun did not
//! deliver are zeroed. Pure and allocation-free: the driver's buffer-switch
//! callback (`asio_win.rs`) calls [`fill_channel`] once per output channel;
//! the encode follows iemmixer's model (`iem-audio-io/src/format.rs`). The
//! codes are azo-sys 0.3.2's `SampleType` constants (`PCM_I16_LSB` 16 …
//! `PCM_I32_LSB_24` 27).

use crate::playback::vban_packet::clean_f32;

/// The sample types an ASIO output writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsioSample {
    Int16,
    Int24,
    Int32,
    Float32,
    /// A 32-bit container holding a sign-extended 16/18/20/24-bit value.
    Int32In16,
    Int32In18,
    Int32In20,
    Int32In24,
}

impl AsioSample {
    /// The type of an ASIOSampleType code; an unsupported code is returned.
    pub fn from_code(code: i32) -> Result<Self, i32> {
        Ok(match code {
            16 => Self::Int16,
            17 => Self::Int24,
            18 => Self::Int32,
            19 => Self::Float32,
            24 => Self::Int32In16,
            25 => Self::Int32In18,
            26 => Self::Int32In20,
            27 => Self::Int32In24,
            other => return Err(other),
        })
    }

    /// The type's ASIO name (the status shows it).
    pub fn name(self) -> &'static str {
        match self {
            Self::Int16 => "Int16LSB",
            Self::Int24 => "Int24LSB",
            Self::Int32 => "Int32LSB",
            Self::Float32 => "Float32LSB",
            Self::Int32In16 => "Int32LSB16",
            Self::Int32In18 => "Int32LSB18",
            Self::Int32In20 => "Int32LSB20",
            Self::Int32In24 => "Int32LSB24",
        }
    }

    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            _ => 4,
        }
    }

    /// The symmetric integer full scale; `None` for Float32.
    fn full_scale(self) -> Option<f64> {
        match self {
            Self::Int16 | Self::Int32In16 => Some(32_767.0),
            Self::Int32In18 => Some(131_071.0),
            Self::Int32In20 => Some(524_287.0),
            Self::Int24 | Self::Int32In24 => Some(8_388_607.0),
            Self::Int32 => Some(2_147_483_647.0),
            Self::Float32 => None,
        }
    }

    /// Channel `ch` (0 = L, 1 = R) of interleaved stereo `stereo` into
    /// `dst`, as many whole samples as both hold.
    pub fn encode(self, stereo: &[f32], ch: usize, dst: &mut [u8]) {
        let full = self.full_scale();
        for (out, frame) in dst
            .chunks_exact_mut(self.bytes())
            .zip(stereo.chunks_exact(2))
        {
            let x = clean_f32(frame[ch]);
            match (self, full) {
                (_, None) => out.copy_from_slice(&x.to_le_bytes()),
                (Self::Int16, Some(full)) => {
                    out.copy_from_slice(&((f64::from(x) * full).round() as i16).to_le_bytes())
                }
                (Self::Int24, Some(full)) => {
                    let b = ((f64::from(x) * full).round() as i32).to_le_bytes();
                    out.copy_from_slice(&b[..3]);
                }
                (_, Some(full)) => {
                    out.copy_from_slice(&((f64::from(x) * full).round() as i32).to_le_bytes())
                }
            }
        }
    }
}

/// Why a driver whose sample type is `code` is refused.
pub fn unsupported_sample_text(code: i32) -> String {
    format!(
        "the driver's sample type {code} is not supported (Int16/24/32LSB, Float32LSB, Int32LSB16-24)"
    )
}

/// Which program channel output channel `channel` plays: L, R or none.
pub fn source_of(channel: usize, left: usize, right: usize) -> Option<usize> {
    if channel == left {
        Some(0)
    } else if channel == right {
        Some(1)
    } else {
        None
    }
}

/// One output channel's half-buffer: `source` of `stereo` (the frames the
/// ring delivered) encoded, everything after them — or all of it for an
/// unused channel — zeroed.
pub fn fill_channel(sample: AsioSample, stereo: &[f32], source: Option<usize>, dst: &mut [u8]) {
    let written = match source {
        Some(ch) => {
            sample.encode(stereo, ch, dst);
            (stereo.len() / 2 * sample.bytes()).min(dst.len())
        }
        None => 0,
    };
    dst[written..].fill(0);
}

#[cfg(test)]
#[path = "asio_format_tests.rs"]
mod tests;
