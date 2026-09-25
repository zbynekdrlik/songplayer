//! Pure-Rust FLAC audio reader backed by Symphonia.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::TimeBase;

use tracing::debug;

use crate::error::DecoderError;
use crate::stream::{AudioStream, MediaStream};
use crate::types::DecodedAudioFrame;

/// Cross-platform audio decoder backed by [symphonia](https://crates.io/crates/symphonia).
///
/// Opens a FLAC file, reports its full duration immediately from the
/// STREAMINFO header, and yields interleaved f32 PCM samples one packet at
/// a time. Seeks are sample-accurate: symphonia's Accurate seek lands on the
/// packet CONTAINING the target, so the reader drops the pre-target frames
/// (`required_ts - actual_ts`, possibly across several packets) and the first
/// sample it emits after `seek(t)` IS the sample at `t` (#148 v3).
pub struct SymphoniaAudioReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    time_base: TimeBase,
    /// After a seek, the first returned chunk is labelled with this timestamp
    /// instead of its packet's block-boundary timestamp. It is TRUE because
    /// `skip_frames` trimmed everything before it.
    pending_seek_ts_ms: Option<u64>,
    /// Decoded frames still to drop after a seek before anything is emitted
    /// (symphonia's `required_ts - actual_ts`). May span several packets.
    skip_frames: u64,
}

impl SymphoniaAudioReader {
    /// Open a FLAC file and build the decoder.
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        let file = File::open(path).map_err(|e| DecoderError::Io(e.to_string()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| DecoderError::SourceReader(e.to_string()))?;

        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(DecoderError::NoStream("audio".into()))?;

        let track_id = track.id;
        let codec_params = &track.codec_params;

        let sample_rate = codec_params
            .sample_rate
            .ok_or_else(|| DecoderError::Decode("missing sample rate".into()))?;
        let channels = codec_params
            .channels
            .ok_or_else(|| DecoderError::Decode("missing channels".into()))?
            .count() as u16;

        let time_base = codec_params
            .time_base
            .unwrap_or(TimeBase::new(1, sample_rate));

        let duration_ms = match codec_params.n_frames {
            Some(n_frames) => ts_to_ms(time_base, n_frames),
            _ => 0,
        };

        let decoder = symphonia::default::get_codecs()
            .make(codec_params, &DecoderOptions::default())
            .map_err(|e| DecoderError::Decode(e.to_string()))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            duration_ms,
            time_base,
            pending_seek_ts_ms: None,
            skip_frames: 0,
        })
    }

    /// Decode one packet and return it as interleaved f32 PCM.
    /// Returns `Ok(None)` on end-of-stream.
    fn decode_packet(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    return Err(DecoderError::Decode("reset required".into()));
                }
                Err(e) => return Err(DecoderError::Decode(e.to_string())),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = self
                .decoder
                .decode(&packet)
                .map_err(|e| DecoderError::Decode(e.to_string()))?;

            let spec = *decoded.spec();
            let sample_rate = spec.rate;
            let channels = spec.channels.count() as u32;

            // Convert whatever sample format Symphonia produced into
            // interleaved f32.
            let mut interleaved: Vec<f32> =
                Vec::with_capacity(decoded.frames() * channels as usize);
            match decoded {
                AudioBufferRef::F32(buf) => {
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame]);
                        }
                    }
                }
                AudioBufferRef::S32(buf) => {
                    let scale = 1.0 / (i32::MAX as f32);
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame] as f32 * scale);
                        }
                    }
                }
                AudioBufferRef::S16(buf) => {
                    let scale = 1.0 / (i16::MAX as f32);
                    for frame in 0..buf.frames() {
                        for ch in 0..channels as usize {
                            interleaved.push(buf.chan(ch)[frame] as f32 * scale);
                        }
                    }
                }
                _ => {
                    return Err(DecoderError::Decode(
                        "unsupported symphonia sample format".into(),
                    ));
                }
            }

            // After a seek, drop the frames before the target. A packet wholly
            // before it (or an empty one) carries nothing to emit.
            trim_leading_frames(&mut interleaved, channels as usize, &mut self.skip_frames);
            if interleaved.is_empty() {
                continue;
            }

            let ts = packet.ts();
            let timestamp_ms = self
                .pending_seek_ts_ms
                .take()
                .unwrap_or_else(|| ts_to_ms(self.time_base, ts));

            return Ok(Some(DecodedAudioFrame {
                data: interleaved,
                channels,
                sample_rate,
                timestamp_ms,
            }));
        }
    }
}

impl MediaStream for SymphoniaAudioReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        // Seek by the integer stream timestamp, never a float `Time`: 0.288 s
        // as f64 times 48 000 is 13 823.99... and symphonia truncates it one
        // frame early. `ms_to_ts` matches `StemMixReader`'s integer re-anchor.
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: ms_to_ts(self.time_base, position_ms),
                    track_id: self.track_id,
                },
            )
            .map_err(|e| DecoderError::Seek(e.to_string()))?;
        self.decoder.reset();
        // Accurate seek lands on the packet CONTAINING the target: arm the
        // pre-target trim so the first emitted sample is the target itself, and
        // label that first chunk with its true media time.
        let (skip_frames, first_ms) = seek_start(
            seeked.required_ts,
            seeked.actual_ts,
            position_ms,
            self.time_base,
        );
        debug!(
            position_ms,
            required_ts = seeked.required_ts,
            actual_ts = seeked.actual_ts,
            skip_frames,
            first_ms,
            "audio seek: trimming pre-target frames"
        );
        self.skip_frames = skip_frames;
        self.pending_seek_ts_ms = Some(first_ms);
        Ok(())
    }
}

impl AudioStream for SymphoniaAudioReader {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        self.decode_packet()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

/// Media time in whole milliseconds of stream timestamp `ts`.
fn ts_to_ms(time_base: TimeBase, ts: u64) -> u64 {
    let t = time_base.calc_time(ts);
    t.seconds * 1_000 + (t.frac * 1_000.0) as u64
}

/// Stream timestamp of `position_ms`, in exact integer arithmetic (floor):
/// `position_ms * denom / (1000 * numer)`. For FLAC's `1/sample_rate` time base
/// that is `position_ms * rate / 1000`, the same frame `StemMixReader`
/// re-anchors on.
fn ms_to_ts(time_base: TimeBase, position_ms: u64) -> u64 {
    let ticks = u128::from(position_ms) * u128::from(time_base.denom);
    let per = 1_000 * u128::from(time_base.numer);
    u64::try_from(ticks / per).unwrap_or(u64::MAX)
}

/// What an Accurate seek leaves to do, from symphonia's `SeekedTo`: the frames
/// to drop before the target (`required_ts - actual_ts`) and the media time (ms)
/// of the first sample then emitted. Normally that is the requested
/// `position_ms` itself. If the demuxer overshot (`actual_ts > required_ts`,
/// which symphonia's FLAC reader allows on a corrupt/odd stream) nothing can be
/// trimmed and the first sample really is at `actual_ts`, so it is labelled so.
fn seek_start(
    required_ts: u64,
    actual_ts: u64,
    position_ms: u64,
    time_base: TimeBase,
) -> (u64, u64) {
    let skip_frames = required_ts.saturating_sub(actual_ts);
    let first_ms = if actual_ts > required_ts {
        ts_to_ms(time_base, actual_ts)
    } else {
        position_ms
    };
    (skip_frames, first_ms)
}

/// Drop up to `*skip` leading frames (`channels` interleaved samples each) from
/// `samples`, decrementing `*skip` by the frames dropped. A trim longer than the
/// packet empties it and carries the rest to the next packet.
fn trim_leading_frames(samples: &mut Vec<f32>, channels: usize, skip: &mut u64) {
    // A 0-channel spec decodes to no samples; `max(1)` only avoids a div by 0.
    let channels = channels.max(1);
    let frames = (samples.len() / channels) as u64;
    let dropped = (*skip).min(frames);
    samples.drain(..dropped as usize * channels);
    *skip -= dropped;
}

#[cfg(test)]
#[path = "symphonia_reader_tests.rs"]
mod tests;
