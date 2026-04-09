//! Windows Media Foundation source reader.

use std::path::Path;

use tracing::debug;

use crate::error::DecoderError;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_API_VERSION,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MFAudioFormat_Float, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
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
}

impl MediaReader {
    /// Open a media file and configure output formats.
    ///
    /// Video is decoded to BGRA (`MFVideoFormat_RGB32`).
    /// Audio is decoded to interleaved f32 PCM (`MFAudioFormat_Float`).
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        // COM + MF init (idempotent)
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
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

        debug!(path = %path.display(), "calling MFCreateSourceReaderFromURL");
        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), None)
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

        Ok(Self {
            reader,
            duration_ms: 0,
        })
    }

    /// Duration of the media in milliseconds (0 if unknown, updated during decode).
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Update the known duration (called as frames are decoded).
    pub fn set_duration_ms(&mut self, ms: u64) {
        if ms > self.duration_ms {
            self.duration_ms = ms;
        }
    }

    /// Read the next decoded video frame, or `None` at end-of-stream.
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

        let (nv12_data, width, height) = Self::lock_video_buffer(&buffer, &self.reader)?;
        let timestamp_ms = (timestamp_100ns / 10_000) as u64;

        // Track duration from timestamps
        if timestamp_ms > self.duration_ms {
            self.duration_ms = timestamp_ms;
        }

        // Convert NV12 → BGRA for NDI output.
        let bgra = nv12_to_bgra(&nv12_data, width, height);
        let stride = width * 4;

        Ok(Some(DecodedVideoFrame {
            data: bgra,
            width,
            height,
            stride,
            timestamp_ms,
        }))
    }

    /// Read the next decoded audio chunk, or `None` at end-of-stream.
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
            // Request NV12 output — the native format for most hardware decoders.
            // We convert NV12→BGRA after reading the sample.
            // RGB32 is rejected by some decoders that only support NV12/YUY2.
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

    /// Lock a video buffer and return raw NV12 data + dimensions.
    fn lock_video_buffer(
        buffer: &IMFMediaBuffer,
        reader: &IMFSourceReader,
    ) -> Result<(Vec<u8>, u32, u32), DecoderError> {
        let (data, _len) = Self::lock_buffer_raw(buffer)?;

        // Read width/height from the negotiated output type.
        let mt: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };

        let (width, height) = unsafe {
            let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };

        Ok((data, width, height))
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

/// Convert NV12 pixel data to BGRA.
///
/// NV12 layout: `height` rows of Y (stride = width), then `height/2` rows of
/// interleaved UV (stride = width).  Total size = width * height * 3/2.
fn nv12_to_bgra(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut bgra = vec![0u8; w * h * 4];
    let y_plane = &nv12[..w * h];
    let uv_plane = &nv12[w * h..];

    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col] as f32;
            let uv_row = row / 2;
            let uv_col = (col / 2) * 2;
            let u = uv_plane[uv_row * w + uv_col] as f32 - 128.0;
            let v = uv_plane[uv_row * w + uv_col + 1] as f32 - 128.0;

            // BT.601 YUV→RGB
            let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;

            let idx = (row * w + col) * 4;
            bgra[idx] = b;
            bgra[idx + 1] = g;
            bgra[idx + 2] = r;
            bgra[idx + 3] = 255;
        }
    }
    bgra
}
