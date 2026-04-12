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
use symphonia::core::units::{Time, TimeBase};

use crate::error::DecoderError;
use crate::stream::{AudioStream, MediaStream};
use crate::types::DecodedAudioFrame;

/// Cross-platform audio decoder backed by [symphonia](https://crates.io/crates/symphonia).
///
/// Opens a FLAC file, reports its full duration immediately from the
/// STREAMINFO header, and yields interleaved f32 PCM samples one packet at
/// a time. Seeks are sample-accurate.
pub struct SymphoniaAudioReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    time_base: TimeBase,
    /// After a seek, the first returned packet uses this timestamp instead of
    /// the block-boundary timestamp, giving the caller a sample-accurate view
    /// of the requested position.
    pending_seek_ts_ms: Option<u64>,
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
            Some(n_frames) => {
                let t = time_base.calc_time(n_frames);
                t.seconds * 1_000 + ((t.frac * 1_000.0) as u64)
            }
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

            let ts = packet.ts();
            let timestamp_ms = self.pending_seek_ts_ms.take().unwrap_or_else(|| {
                let t = self.time_base.calc_time(ts);
                t.seconds * 1_000 + (t.frac * 1_000.0) as u64
            });

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
        let target = Time::from(std::time::Duration::from_millis(position_ms));
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: target,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| DecoderError::Seek(e.to_string()))?;
        self.decoder.reset();
        // Remember the exact requested position so the first decoded packet
        // reports this timestamp rather than the FLAC block boundary.
        self.pending_seek_ts_ms = Some(position_ms);
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
