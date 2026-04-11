//! Windows Media Foundation source reader.

use std::path::Path;

use tracing::debug;

use crate::error::DecoderError;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_API_VERSION,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MFAudioFormat_Float, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::core::PCWSTR;

use std::os::windows::ffi::OsStrExt;

/// The first video stream sentinel as `u32` for `ReadSample`.
const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// The first audio stream sentinel as `u32` for `ReadSample`.
const AUDIO_STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

/// Media Foundation source reader that decodes video and audio from a file.
pub struct MediaReader {
    reader: IMFSourceReader,
    duration_ms: u64,
    video_width: u32,
    video_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
}

impl MediaReader {
    // cargo-mutants: skip — this entire impl block uses Windows Media Foundation
    // APIs that are only available at runtime on Windows. On the Linux mutation
    // runner these functions are excluded from compilation, so mutants would
    // survive with no observable behaviour. The sp-decoder crate is
    // cfg(windows)-only; mutation coverage is implicitly provided on Windows CI.

    /// Open a media file and configure output formats.
    ///
    /// Video is decoded to NV12 (`MFVideoFormat_NV12`) and passed through to
    /// callers without intermediate color conversion.
    /// Audio is decoded to interleaved f32 PCM (`MFAudioFormat_Float`).
    #[cfg_attr(test, mutants::skip)]
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        // COM + MF init (idempotent)
        unsafe {
            // STA required for hardware-accelerated decoders (AV1, VP9).
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            debug!(hr = ?hr, "CoInitializeEx result");
            MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecoderError::ComInit(format!("MFStartup: {e}")))?;
        }
        debug!("MFStartup succeeded");

        // Build a wide-string path for MF.
        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // Create attributes to enable hardware-accelerated transforms (AV1, VP9).
        let mut attrs: Option<IMFAttributes> = None;
        unsafe {
            MFCreateAttributes(&mut attrs, 1)
                .map_err(|e| DecoderError::ComInit(format!("MFCreateAttributes: {e}")))?;
        }
        let attrs = attrs
            .ok_or_else(|| DecoderError::ComInit("MFCreateAttributes returned null".into()))?;
        unsafe {
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(|e| {
                    DecoderError::ComInit(format!("SetUINT32 ENABLE_HARDWARE_TRANSFORMS: {e}"))
                })?;
        }

        debug!(path = %path.display(), "calling MFCreateSourceReaderFromURL");
        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), Some(&attrs))
                .map_err(|e| DecoderError::SourceReader(e.to_string()))?
        };
        debug!("MFCreateSourceReaderFromURL succeeded");

        // Configure video output to NV12
        debug!("setting video output type to NV12");
        let video_type = Self::make_video_output_type()?;
        unsafe {
            reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &video_type)
                .map_err(|e| {
                    DecoderError::NoStream(Box::leak(
                        format!("video: SetCurrentMediaType failed: {e}").into_boxed_str(),
                    ))
                })?;
        }
        debug!("video output type set successfully");

        // Configure audio output to f32 PCM
        debug!("setting audio output type");
        let audio_type = Self::make_audio_output_type()?;
        unsafe {
            reader
                .SetCurrentMediaType(AUDIO_STREAM, None, &audio_type)
                .map_err(|e| {
                    DecoderError::NoStream(Box::leak(
                        format!("audio: SetCurrentMediaType failed: {e}").into_boxed_str(),
                    ))
                })?;
        }
        debug!("audio output type set successfully");

        debug!(path = %path.display(), "media file opened successfully");

        // Read back the negotiated video type so we know the real frame size
        // and frame rate for the NDI sender.
        let negotiated_video: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(format!("GetCurrentMediaType video: {e}")))?
        };
        let (video_width, video_height) = unsafe {
            let size = negotiated_video.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };
        let (frame_rate_num, frame_rate_den) = unsafe {
            match negotiated_video.GetUINT64(&MF_MT_FRAME_RATE) {
                Ok(packed) => ((packed >> 32) as u32, packed as u32),
                Err(_) => {
                    tracing::warn!("MF_MT_FRAME_RATE unavailable; falling back to 30000/1001");
                    (30000, 1001)
                }
            }
        };
        debug!(
            video_width,
            video_height, frame_rate_num, frame_rate_den, "negotiated video media type"
        );

        Ok(Self {
            reader,
            duration_ms: 0,
            video_width,
            video_height,
            frame_rate_num,
            frame_rate_den,
        })
    }

    /// Duration of the media in milliseconds (0 if unknown, updated during decode).
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Video stream metadata from the negotiated media type.
    #[cfg_attr(test, mutants::skip)]
    pub fn video_info(&self) -> crate::types::VideoStreamInfo {
        crate::types::VideoStreamInfo {
            width: self.video_width,
            height: self.video_height,
            pixel_format: crate::types::PixelFormat::Nv12,
            frame_rate_num: self.frame_rate_num,
            frame_rate_den: self.frame_rate_den,
        }
    }

    /// Update the known duration (called as frames are decoded).
    pub fn set_duration_ms(&mut self, ms: u64) {
        if ms > self.duration_ms {
            self.duration_ms = ms;
        }
    }

    /// Read the next decoded video frame, or `None` at end-of-stream.
    #[cfg_attr(test, mutants::skip)]
    pub fn next_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let mut flags: u32 = 0;
        let mut timestamp_100ns: i64 = 0;
        let mut actual_stream_index: u32 = 0;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            self.reader
                .ReadSample(
                    VIDEO_STREAM,
                    0,
                    Some(&mut actual_stream_index as *mut _),
                    Some(&mut flags as *mut _),
                    Some(&mut timestamp_100ns as *mut _),
                    Some(&mut sample as *mut _),
                )
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?;
        }

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

        let (nv12_data, width, height, stride) = Self::lock_video_buffer(&buffer, &self.reader)?;
        let timestamp_ms = (timestamp_100ns / 10_000) as u64;

        // Track duration from timestamps
        if timestamp_ms > self.duration_ms {
            self.duration_ms = timestamp_ms;
        }

        // NV12 passthrough: the raw buffer is Y plane (height × stride bytes)
        // plus interleaved UV plane (height/2 × stride bytes). NDI accepts NV12
        // natively via NDIlib_FourCC_video_type_NV12. Stride is the Y-plane row
        // stride as negotiated with Media Foundation — hardware decoders often
        // pad to 16/32/64-byte alignment, which is why we read MF_MT_DEFAULT_STRIDE
        // rather than assuming stride == width.

        Ok(Some(DecodedVideoFrame {
            data: nv12_data,
            width,
            height,
            stride,
            timestamp_ms,
            pixel_format: crate::types::PixelFormat::Nv12,
        }))
    }

    /// Read the next decoded audio chunk, or `None` at end-of-stream.
    #[cfg_attr(test, mutants::skip)]
    pub fn next_audio_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let mut flags: u32 = 0;
        let mut timestamp_100ns: i64 = 0;
        let mut actual_stream_index: u32 = 0;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            self.reader
                .ReadSample(
                    AUDIO_STREAM,
                    0,
                    Some(&mut actual_stream_index as *mut _),
                    Some(&mut flags as *mut _),
                    Some(&mut timestamp_100ns as *mut _),
                    Some(&mut sample as *mut _),
                )
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?;
        }

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
            // Request NV12 output — the native format for most hardware decoders,
            // and the FourCC NDI accepts directly. No color conversion happens
            // after the sample is read.
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
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

    /// Lock a video buffer and return raw NV12 data + dimensions + Y-plane stride.
    fn lock_video_buffer(
        buffer: &IMFMediaBuffer,
        reader: &IMFSourceReader,
    ) -> Result<(Vec<u8>, u32, u32, u32), DecoderError> {
        let (data, _len) = Self::lock_buffer_raw(buffer)?;

        // Read width/height/stride from the negotiated output type.
        let mt: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };

        let (width, height) = unsafe {
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };

        let stride = unsafe {
            let stride_raw: i32 = match mt.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(s) => s as i32,
                Err(_) => {
                    tracing::warn!(
                        "MF_MT_DEFAULT_STRIDE unavailable; falling back to width={width}"
                    );
                    width as i32
                }
            };
            // Y-plane stride is always positive for NV12 MF output; a negative
            // value would mean a bottom-up image which NV12 from MF never is.
            // Abs-guard defensively and fall back to width if MF reports 0 or a
            // value smaller than width.
            if stride_raw > 0 && (stride_raw as u32) >= width {
                stride_raw as u32
            } else {
                width
            }
        };

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
