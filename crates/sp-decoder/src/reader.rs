//! Windows Media Foundation source reader.

use std::path::Path;

use tracing::{debug, warn};

use crate::error::DecoderError;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFSourceReader, MF_API_VERSION, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MFAudioFormat_Float, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::PCWSTR;

/// The first video stream sentinel as `u32` for `ReadSample`.
const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// The first audio stream sentinel as `u32` for `ReadSample`.
const AUDIO_STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

/// Media Foundation source reader that decodes video and audio from a file.
pub struct MediaReader {
    reader: IMFSourceReader,
    duration_100ns: i64,
    video_stream_index: u32,
    audio_stream_index: u32,
}

impl MediaReader {
    /// Open a media file and configure output formats.
    ///
    /// Video is decoded to BGRA (`MFVideoFormat_RGB32`).
    /// Audio is decoded to interleaved f32 PCM (`MFAudioFormat_Float`).
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        // --- COM + MF init (idempotent) ---
        unsafe {
            // COM init — may already be initialised on this thread; that is fine.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecoderError::ComInit(format!("MFStartup: {e}")))?;
        }

        // Build a wide-string path for MF.
        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), None)
                .map_err(|e| DecoderError::SourceReader(e.to_string()))?
        };

        // --- Configure video output to BGRA ---
        let video_type = Self::make_video_output_type()?;
        unsafe {
            reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &video_type)
                .map_err(|e| {
                    DecoderError::NoStream(
                        // static str — we pick a message at compile time
                        if e.code().is_err() { "video" } else { "video" },
                    )
                })?;
        }

        // --- Configure audio output to f32 PCM ---
        let audio_type = Self::make_audio_output_type()?;
        unsafe {
            reader
                .SetCurrentMediaType(AUDIO_STREAM, None, &audio_type)
                .map_err(|_| DecoderError::NoStream("audio"))?;
        }

        // --- Read duration from the presentation descriptor ---
        let duration_100ns = unsafe {
            reader
                .GetPresentationAttribute(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    &MF_PD_DURATION,
                )
                .ok()
                .and_then(|prop| {
                    let val: i64 = prop.Anonymous.Anonymous.Anonymous.hVal.QuadPart;
                    Some(val)
                })
                .unwrap_or(0)
        };

        debug!(
            path = %path.display(),
            duration_ms = duration_100ns / 10_000,
            "Opened media file"
        );

        Ok(Self {
            reader,
            duration_100ns,
            video_stream_index: VIDEO_STREAM,
            audio_stream_index: AUDIO_STREAM,
        })
    }

    /// Duration of the media in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        (self.duration_100ns / 10_000) as u64
    }

    /// Seek to a position given in milliseconds.
    pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

        let hns = (position_ms as i64) * 10_000; // ms → 100-ns units
        let mut pv: PROPVARIANT = Default::default();
        // PROPVARIANT is a union; we set it to VT_I8 (signed 64-bit).
        unsafe {
            pv.Anonymous.Anonymous.vt = windows::Win32::System::Variant::VT_I8;
            pv.Anonymous.Anonymous.Anonymous.hVal =
                std::mem::transmute::<i64, windows::Win32::Foundation::LARGE_INTEGER>(hns);
        }
        unsafe {
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .map_err(|e| DecoderError::Seek(e.to_string()))?;
        }
        debug!(position_ms, "Seeked");
        Ok(())
    }

    /// Read the next decoded video frame, or `None` at end-of-stream.
    pub fn next_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let mut flags = 0u32;
        let mut timestamp_100ns = 0i64;

        let sample = unsafe {
            self.reader
                .ReadSample(
                    self.video_stream_index,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp_100ns),
                )
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }

        let sample = match sample {
            Some(s) => s,
            None => return Ok(None),
        };

        // Get the buffer from the sample.
        let buffer: IMFMediaBuffer = unsafe {
            sample
                .ConvertToContiguousBuffer()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?
        };

        let (data, width, height, stride) = Self::lock_video_buffer(&buffer, &self.reader)?;

        Ok(Some(DecodedVideoFrame {
            data,
            width,
            height,
            stride,
            timestamp_ms: (timestamp_100ns / 10_000) as u64,
        }))
    }

    /// Read the next decoded audio chunk, or `None` at end-of-stream.
    pub fn next_audio_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let mut flags = 0u32;
        let mut timestamp_100ns = 0i64;

        let sample = unsafe {
            self.reader
                .ReadSample(
                    self.audio_stream_index,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp_100ns),
                )
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }

        let sample = match sample {
            Some(s) => s,
            None => return Ok(None),
        };

        let buffer: IMFMediaBuffer = unsafe {
            sample
                .ConvertToContiguousBuffer()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?
        };

        let (data_bytes, _len) = Self::lock_buffer_raw(&buffer)?;

        // Reinterpret as f32 samples.
        let f32_count = data_bytes.len() / 4;
        let mut pcm = Vec::with_capacity(f32_count);
        for chunk in data_bytes.chunks_exact(4) {
            pcm.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        // Read channel count and sample rate from the current output type.
        let (channels, sample_rate) = Self::audio_format_info(&self.reader)?;

        Ok(Some(DecodedAudioFrame {
            data: pcm,
            channels,
            sample_rate,
            timestamp_ms: (timestamp_100ns / 10_000) as u64,
        }))
    }

    // ---- private helpers -----------------------------------------------

    fn make_video_output_type() -> Result<IMFMediaType, DecoderError> {
        unsafe {
            let mt: IMFMediaType =
                MFCreateMediaType().map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            Ok(mt)
        }
    }

    fn make_audio_output_type() -> Result<IMFMediaType, DecoderError> {
        unsafe {
            let mt: IMFMediaType =
                MFCreateMediaType().map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .map_err(|e| DecoderError::ComInit(e.to_string()))?;
            Ok(mt)
        }
    }

    /// Lock an `IMFMediaBuffer`, copy its contents, and unlock.
    fn lock_buffer_raw(buffer: &IMFMediaBuffer) -> Result<(Vec<u8>, u32), DecoderError> {
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut length))
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;

            let slice = std::slice::from_raw_parts(ptr, length as usize);
            let data = slice.to_vec();

            buffer
                .Unlock()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;

            Ok((data, length))
        }
    }

    /// Lock a video buffer and also return width/height/stride from the
    /// current output media type.
    fn lock_video_buffer(
        buffer: &IMFMediaBuffer,
        reader: &IMFSourceReader,
    ) -> Result<(Vec<u8>, u32, u32, u32), DecoderError> {
        let (data, _len) = Self::lock_buffer_raw(buffer)?;

        // Read width/height from the negotiated output type.
        let mt: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };

        let (width, height) = unsafe {
            let mut size = 0u64;
            use windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE;
            mt.GetUINT64(&MF_MT_FRAME_SIZE)
                .map(|v| {
                    size = v;
                })
                .ok();
            ((size >> 32) as u32, size as u32)
        };

        let stride = width * 4; // BGRA = 4 bytes per pixel

        Ok((data, width, height, stride))
    }

    /// Read channels + sample_rate from the current audio output type.
    fn audio_format_info(reader: &IMFSourceReader) -> Result<(u32, u32), DecoderError> {
        let mt: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(AUDIO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };
        unsafe {
            let channels = mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2);
            let sample_rate = mt
                .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                .unwrap_or(48_000);
            Ok((channels, sample_rate))
        }
    }
}

use std::os::windows::ffi::OsStrExt;
