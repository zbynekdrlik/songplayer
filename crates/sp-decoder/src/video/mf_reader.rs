//! Media Foundation video-only reader.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use tracing::debug;

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_API_VERSION,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::core::PCWSTR;

use crate::error::DecoderError;
use crate::stream::{MediaStream, VideoStream};
use crate::types::{DecodedVideoFrame, PixelFormat};

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Video-only Media Foundation source reader.
pub struct MediaFoundationVideoReader {
    reader: IMFSourceReader,
    duration_ms: u64,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
}

// SAFETY: IMFSourceReader is a COM interface that windows-rs marks as !Send.
// MFCreateSourceReaderFromURL initialises MF in STA mode (COINIT_APARTMENTTHREADED).
// Once opened the reader is driven from a single worker thread in the playback
// pipeline; ownership transfer across threads happens only when the owning
// thread is done with the reader.  We therefore assert Send manually, matching
// the same pattern used by all video/audio readers in this crate.
unsafe impl Send for MediaFoundationVideoReader {}

impl MediaFoundationVideoReader {
    #[cfg_attr(test, mutants::skip)]
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            debug!(hr = ?hr, "CoInitializeEx result");
            MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecoderError::ComInit(format!("MFStartup: {e}")))?;
        }

        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

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

        let reader: IMFSourceReader = unsafe {
            MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), Some(&attrs))
                .map_err(|e| DecoderError::SourceReader(e.to_string()))?
        };

        // Negotiate NV12 output.
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

        let negotiated_video: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(format!("GetCurrentMediaType video: {e}")))?
        };
        let (width, height) = unsafe {
            let size = negotiated_video.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };
        let (frame_rate_num, frame_rate_den) = unsafe {
            match negotiated_video.GetUINT64(&MF_MT_FRAME_RATE) {
                Ok(packed) => ((packed >> 32) as u32, packed as u32),
                Err(e) => {
                    tracing::warn!(
                        "MF_MT_FRAME_RATE unavailable: {e}; falling back to 30000/1001 (29.97 fps)"
                    );
                    (30000, 1001)
                }
            }
        };

        let duration_ms: u64 = unsafe {
            match reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            {
                Ok(pv) => u64::try_from(&pv).unwrap_or(0) / 10_000,
                Err(_) => 0,
            }
        };

        Ok(Self {
            reader,
            duration_ms,
            width,
            height,
            frame_rate_num,
            frame_rate_den,
        })
    }

    fn make_video_output_type() -> Result<IMFMediaType, DecoderError> {
        let media_type: IMFMediaType = unsafe {
            MFCreateMediaType()
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?
        };
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| DecoderError::NoStream(Box::leak(e.to_string().into_boxed_str())))?;
        }
        Ok(media_type)
    }

    fn lock_video_buffer(
        buffer: &IMFMediaBuffer,
        reader: &IMFSourceReader,
    ) -> Result<(Vec<u8>, u32, u32, u32), DecoderError> {
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut current_len: u32 = 0;

        unsafe {
            buffer
                .Lock(&mut data_ptr, Some(&mut max_len), Some(&mut current_len))
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
        }

        let nv12: Vec<u8> =
            unsafe { std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec() };

        unsafe {
            buffer
                .Unlock()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
        }

        let media_type: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(e.to_string()))?
        };
        let (width, height) = unsafe {
            let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            ((size >> 32) as u32, size as u32)
        };
        let stride = unsafe {
            media_type
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as u32)
                .unwrap_or(width)
        };

        Ok((nv12, width, height, stride))
    }
}

impl MediaStream for MediaFoundationVideoReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, _position_ms: u64) -> Result<(), DecoderError> {
        // Seek not yet wired for the video reader — the playback pipeline
        // does not currently expose scrubbing, so no caller exercises this
        // path. When dashboard scrubbing is added, this will need a real
        // MF SetCurrentPosition implementation via PROPVARIANT.
        Err(DecoderError::Seek(
            "MediaFoundationVideoReader::seek not yet implemented".into(),
        ))
    }
}

impl VideoStream for MediaFoundationVideoReader {
    #[cfg_attr(test, mutants::skip)]
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
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

        let buffer: IMFMediaBuffer = unsafe {
            sample
                .ConvertToContiguousBuffer()
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?
        };

        let (nv12_data, width, height, stride) = Self::lock_video_buffer(&buffer, &self.reader)?;
        let timestamp_ms = (timestamp_100ns / 10_000) as u64;

        if timestamp_ms > self.duration_ms {
            self.duration_ms = timestamp_ms;
        }

        Ok(Some(DecodedVideoFrame {
            data: nv12_data,
            width,
            height,
            stride,
            timestamp_ms,
            pixel_format: PixelFormat::Nv12,
        }))
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn frame_rate(&self) -> (u32, u32) {
        (self.frame_rate_num, self.frame_rate_den)
    }
}
