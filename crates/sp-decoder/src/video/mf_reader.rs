//! Media Foundation video-only reader.
//!
//! #223 S3b: a reader opens its file in a [`DecodeMode`]
//! ([`MediaFoundationVideoReader::open_with`]). `Software` (the default, and
//! [`MediaFoundationVideoReader::open`]) is the path every song took before:
//! Media Foundation's software decoders, each picture locked out of a
//! system-memory buffer. `Hardware` gives the source reader a Direct3D 11
//! device on the GPU (`hw_session.rs`: `sp_gpu::VideoDevice` behind a DXGI
//! device manager, `MF_SOURCE_READER_D3D_MANAGER`), so the decoder MFT
//! decodes with DXVA and hands each picture over as a DXGI surface, which is
//! copied back in the software path's NV12 layout (`dxgi_frame.rs`). The path
//! really used is read from every picture ([`DecodePath::of_picture`]), and
//! the hardware path never kills a song: a failed open, or a decode error on
//! the D3D path, falls back to software for that file with a WARN. Every
//! decision is `crate::hw_decode`'s (Linux-tested); this file only calls
//! Media Foundation.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_API_VERSION,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFStartup,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::core::PCWSTR;

use super::dxgi_frame::DxgiSurface;
use super::hw_session::{HwDevice, HwSession};
use crate::error::DecoderError;
use crate::hw_decode::{
    DecodeMode, DecodePath, FallbackGate, FallbackStage, HwFallback, OnDecodeError, PathTracker,
    Resume, hw_counters,
};
use crate::stream::{MediaStream, VideoStream};
use crate::types::{DecodedVideoFrame, PixelFormat};

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Video-only Media Foundation source reader.
pub struct MediaFoundationVideoReader {
    reader: IMFSourceReader,
    /// The file, for a software reopen (#223 S3b).
    path: PathBuf,
    duration_ms: u64,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    /// The negotiated type carried `MF_MT_FRAME_RATE`; `false` = the
    /// 29.97 fps fallback.
    frame_rate_known: bool,
    /// What the caller asked for.
    mode: DecodeMode,
    /// The device manager while the source reader runs on the D3D path;
    /// `None` in software and after a fall back.
    hw: Option<HwSession>,
    /// The adapter the D3D path opened on (kept after a fall back).
    adapter: Option<String>,
    /// The path of the pictures handed over.
    paths: PathTracker,
    /// Why this file left the hardware path, if it did.
    fallback: Option<HwFallback>,
    /// Whether a decode error may still reopen the file in software (once).
    gate: FallbackGate,
    resume: Resume,
    /// A test's injected decode failure for the next read.
    inject_failure: bool,
}

// SAFETY: IMFSourceReader is a COM interface that windows-rs marks as !Send.
// MFCreateSourceReaderFromURL initialises MF in STA mode (COINIT_APARTMENTTHREADED).
// Once opened the reader is driven from a single worker thread in the playback
// pipeline; ownership transfer across threads happens only when the owning
// thread is done with the reader.  We therefore assert Send manually, matching
// the same pattern used by all video/audio readers in this crate.
unsafe impl Send for MediaFoundationVideoReader {}

/// What a source reader negotiated at open.
struct Opened {
    reader: IMFSourceReader,
    duration_ms: u64,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    frame_rate_known: bool,
}

impl MediaFoundationVideoReader {
    /// Open `path` in software (`open_with(path, DecodeMode::Software)`).
    #[cfg_attr(test, mutants::skip)]
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        Self::open_with(path, DecodeMode::Software)
    }

    /// Open `path` in `mode` (#223 S3b). A `Hardware` open that cannot set up
    /// the D3D path opens the file in software (WARN, [`Self::hw_fallback`]);
    /// only a file that does not open in software either is an error.
    #[cfg_attr(test, mutants::skip)]
    pub fn open_with(path: &Path, mode: DecodeMode) -> Result<Self, DecoderError> {
        com_startup()?;
        match mode {
            DecodeMode::Software => {
                let opened = Self::create(path, None)?;
                Ok(Self::from_opened(path, mode, opened, None))
            }
            DecodeMode::Hardware => Self::open_hardware(path, HwDevice::Picked),
        }
    }

    /// `Hardware` mode on a WARP device instead of the picked GPU: for the
    /// Windows CI tests (`windows-latest` has no GPU). WARP has no decoder
    /// profiles, so Media Foundation's decoder decodes in software on it.
    #[doc(hidden)]
    #[cfg_attr(test, mutants::skip)]
    pub fn open_hardware_on_warp(path: &Path) -> Result<Self, DecoderError> {
        com_startup()?;
        Self::open_hardware(path, HwDevice::Warp)
    }

    fn open_hardware(path: &Path, device: HwDevice) -> Result<Self, DecoderError> {
        hw_counters().requested();
        let attempt = HwSession::new(device).and_then(|session| {
            let opened = Self::create(path, Some(&session))
                .map_err(|e| format!("the source reader refused the D3D11 device: {e}"))?;
            Ok((opened, session))
        });
        match attempt {
            Ok((opened, session)) => {
                info!(
                    file = %path.display(),
                    adapter = session.adapter_name(),
                    "mf_reader: opened on the D3D11 path (hardware decode requested)"
                );
                let adapter = session.adapter_name().to_string();
                let mut reader =
                    Self::from_opened(path, DecodeMode::Hardware, opened, Some(session));
                reader.adapter = Some(adapter);
                reader.gate.arm();
                Ok(reader)
            }
            Err(reason) => {
                let fallback = HwFallback {
                    stage: FallbackStage::Open,
                    reason,
                };
                warn!(
                    file = %path.display(),
                    fallback = %fallback.describe(),
                    "mf_reader: hardware decode did not open; this file decodes in software"
                );
                hw_counters().fell_back(&fallback);
                let opened = Self::create(path, None)?;
                let mut reader = Self::from_opened(path, DecodeMode::Hardware, opened, None);
                reader.fallback = Some(fallback);
                Ok(reader)
            }
        }
    }

    fn from_opened(path: &Path, mode: DecodeMode, opened: Opened, hw: Option<HwSession>) -> Self {
        Self {
            reader: opened.reader,
            path: path.to_path_buf(),
            duration_ms: opened.duration_ms,
            width: opened.width,
            height: opened.height,
            frame_rate_num: opened.frame_rate_num,
            frame_rate_den: opened.frame_rate_den,
            frame_rate_known: opened.frame_rate_known,
            mode,
            hw,
            adapter: None,
            paths: PathTracker::default(),
            fallback: None,
            gate: FallbackGate::default(),
            resume: Resume::default(),
            inject_failure: false,
        }
    }

    /// A source reader on `path` with NV12 output, on `hw`'s device when
    /// given (the D3D path), else in software (no device: the reader before
    /// #223 S3b, unchanged).
    fn create(path: &Path, hw: Option<&HwSession>) -> Result<Opened, DecoderError> {
        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut attrs: Option<IMFAttributes> = None;
        unsafe {
            MFCreateAttributes(&mut attrs, 2)
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
        if let Some(hw) = hw {
            unsafe { attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, hw.manager()) }.map_err(
                |e| DecoderError::ComInit(format!("SetUnknown SOURCE_READER_D3D_MANAGER: {e}")),
            )?;
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
                    DecoderError::NoStream(format!("video: SetCurrentMediaType failed: {e}"))
                })?;
        }

        let negotiated_video: IMFMediaType = unsafe {
            reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(|e| DecoderError::ReadSample(format!("GetCurrentMediaType video: {e}")))?
        };
        let (width, height) = frame_size(&negotiated_video);
        let (frame_rate_num, frame_rate_den, frame_rate_known) = unsafe {
            match negotiated_video.GetUINT64(&MF_MT_FRAME_RATE) {
                Ok(packed) => ((packed >> 32) as u32, packed as u32, true),
                Err(e) => {
                    tracing::warn!(
                        "MF_MT_FRAME_RATE unavailable: {e}; falling back to 30000/1001 (29.97 fps)"
                    );
                    (30000, 1001, false)
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

        Ok(Opened {
            reader,
            duration_ms,
            width,
            height,
            frame_rate_num,
            frame_rate_den,
            frame_rate_known,
        })
    }

    /// The codec the file's video stream is compressed with, as the native
    /// media type's subtype text (`AV01`, `VP90`, `H264`, `HEVC`;
    /// [`crate::subtype::subtype_name`]). `None` when MF does not answer.
    /// The decode bench reports it (#223 S0), so a measured sample is known to
    /// be the codec it is named after.
    #[cfg_attr(test, mutants::skip)]
    pub fn codec(&self) -> Option<String> {
        let native: IMFMediaType =
            unsafe { self.reader.GetNativeMediaType(VIDEO_STREAM, 0) }.ok()?;
        let guid = unsafe { native.GetGUID(&MF_MT_SUBTYPE) }.ok()?;
        Some(crate::subtype::subtype_name(
            guid.data1, guid.data2, guid.data3, guid.data4,
        ))
    }

    /// Whether [`VideoStream::frame_rate`] is the stream's own rate, not the
    /// 29.97 fps fallback `open` takes when MF reports none. The decode bench
    /// (#223 S0) judges D2's budget only against a known rate.
    pub fn frame_rate_known(&self) -> bool {
        self.frame_rate_known
    }

    /// What the caller asked for (#223 S3b).
    pub fn decode_mode(&self) -> DecodeMode {
        self.mode
    }

    /// The path the last picture handed over really came out of (#223 S3b):
    /// `Hardware` = a DXGI surface from the GPU decoder. `None` before the
    /// first picture.
    pub fn decode_path(&self) -> Option<DecodePath> {
        self.paths.last()
    }

    /// The adapter the D3D path opened on, if it did.
    pub fn hw_adapter(&self) -> Option<&str> {
        self.adapter.as_deref()
    }

    /// Why this file left the hardware path, if it did.
    pub fn hw_fallback(&self) -> Option<&HwFallback> {
        self.fallback.as_ref()
    }

    /// Make the next read fail as a decode error on the D3D path would, so
    /// the reader reopens the file in software once, even when it opened in
    /// software: the only way CI (no GPU) runs the mid-stream fall back.
    /// For tests, not production.
    #[doc(hidden)]
    pub fn fail_next_read_for_test(&mut self) {
        self.inject_failure = true;
        self.gate.arm();
    }

    fn make_video_output_type() -> Result<IMFMediaType, DecoderError> {
        let media_type: IMFMediaType =
            unsafe { MFCreateMediaType().map_err(|e| DecoderError::NoStream(e.to_string()))? };
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecoderError::NoStream(e.to_string()))?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| DecoderError::NoStream(e.to_string()))?;
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

        let nv12: Vec<u8> = if current_len == 0 {
            Vec::new()
        } else {
            let len = current_len as usize;
            // Fill a RECYCLED buffer (capacity >= len, cleared) via
            // `extend_from_slice` into retained capacity — no demand-zero page
            // fault after the first frame of a resolution (#203 2b). The
            // SDK-clocked path wraps this Vec in `SharedFrame::new` at
            // `submit_nv12` and the paced path in `to_paced_frame`, so its
            // last-owner drop returns the allocation to `frame_pool` for reuse.
            // #207: FALLIBLE — a host OOM (out of commit) returns a typed
            // `FrameAlloc` error the pipeline maps to a dropped frame instead of
            // aborting the whole process (`handle_alloc_error`, the #156 class).
            match crate::frame_pool::try_take(len) {
                Ok(mut buf) => {
                    // SAFETY: `data_ptr .. data_ptr + len` is the MF-locked
                    // buffer, valid until `Unlock` below; `extend_from_slice`
                    // copies out of it.
                    unsafe {
                        buf.extend_from_slice(std::slice::from_raw_parts(data_ptr, len));
                    }
                    buf
                }
                Err(e) => {
                    // Unlock before propagating so the MF buffer is never left
                    // locked on the drop path.
                    unsafe {
                        let _ = buffer.Unlock();
                    }
                    return Err(DecoderError::FrameAlloc(e.bytes));
                }
            }
        };

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
        let (width, height) = frame_size(&media_type);
        let stride = unsafe {
            media_type
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as u32)
                .unwrap_or(width)
        };

        Ok((nv12, width, height, stride))
    }

    /// The next sample and its timestamp (100 ns), `None` at the end.
    fn read_sample(&mut self) -> Result<Option<(IMFSample, i64)>, DecoderError> {
        // When hardware transforms are enabled, `ReadSample` is permitted to
        // return `S_OK` with a null sample while the decoder is still
        // draining pre-roll frames — the caller must keep calling until a
        // sample comes out or the end-of-stream flag is set. Cap the retry
        // count so a broken source can't spin forever.
        const MAX_NULL_RETRIES: usize = 64;

        let mut null_retries = 0_usize;
        loop {
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

            if let Some(s) = sample {
                return Ok(Some((s, timestamp_100ns)));
            }

            null_retries += 1;
            if null_retries >= MAX_NULL_RETRIES {
                return Err(DecoderError::ReadSample(format!(
                    "ReadSample returned null without EOS {MAX_NULL_RETRIES} times"
                )));
            }
        }
    }

    /// The next decoded picture the caller does not have yet, from a DXGI
    /// surface on the D3D path or from a system-memory buffer (software).
    /// After a software reopen, the pictures `Resume` drops are dropped
    /// before any readback (each still costs its decode).
    fn read_picture(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let (sample, timestamp_ms) = loop {
            let Some((sample, timestamp_100ns)) = self.read_sample()? else {
                return Ok(None);
            };
            let timestamp_ms = (timestamp_100ns.max(0) / 10_000) as u64;
            if !self.resume.skips(timestamp_ms) {
                break (sample, timestamp_ms);
            }
        };
        let surface = if self.hw.is_some() {
            DxgiSurface::of(&sample)?
        } else {
            None
        };
        let (data, width, height, stride, path) = match surface {
            Some(surface) => {
                let media_type: IMFMediaType = unsafe {
                    self.reader
                        .GetCurrentMediaType(VIDEO_STREAM)
                        .map_err(|e| DecoderError::ReadSample(e.to_string()))?
                };
                let (width, height) = frame_size(&media_type);
                let picture = surface.read(width, height)?;
                (picture.data, width, height, picture.stride, picture.path)
            }
            None => {
                let buffer: IMFMediaBuffer = unsafe {
                    sample
                        .ConvertToContiguousBuffer()
                        .map_err(|e| DecoderError::BufferLock(e.to_string()))?
                };
                let (data, width, height, stride) = Self::lock_video_buffer(&buffer, &self.reader)?;
                (data, width, height, stride, DecodePath::of_picture(None))
            }
        };
        self.observe(path);

        if timestamp_ms > self.duration_ms {
            self.duration_ms = timestamp_ms;
        }

        Ok(Some(DecodedVideoFrame {
            data,
            width,
            height,
            stride,
            timestamp_ms,
            pixel_format: PixelFormat::Nv12,
        }))
    }

    /// Record the path of a picture; the first one on the D3D path is
    /// counted and logged (`PathTracker`), so a silent software decode is
    /// seen.
    fn observe(&mut self, path: DecodePath) {
        if self.paths.observe(path, self.hw.is_some()) {
            hw_counters().first_picture(path);
            let adapter = self.adapter.as_deref().unwrap_or("?");
            match path {
                DecodePath::Hardware => info!(
                    file = %self.path.display(),
                    adapter,
                    "mf_reader: hardware decode active (decoder surfaces, D3D11_BIND_DECODER)"
                ),
                DecodePath::Software => warn!(
                    file = %self.path.display(),
                    adapter,
                    "mf_reader: the D3D11 path is set up, but Media Foundation decodes this file in software"
                ),
            }
        }
    }

    /// Reopen the file in software after `error` on the D3D path, where the
    /// last picture handed over was (#223 S3b, once per file).
    fn reopen_in_software(&mut self, error: &DecoderError) -> Result<(), DecoderError> {
        let fallback = HwFallback {
            stage: FallbackStage::MidStream,
            reason: error.to_string(),
        };
        warn!(
            file = %self.path.display(),
            fallback = %fallback.describe(),
            "mf_reader: a decode error on the D3D11 path; this file goes on in software"
        );
        hw_counters().fell_back(&fallback);
        self.fallback = Some(fallback);
        let opened = Self::create(&self.path, None)?;
        self.reader = opened.reader;
        self.hw = None;
        self.width = opened.width;
        self.height = opened.height;
        self.duration_ms = self.duration_ms.max(opened.duration_ms);
        if let Some(position_ms) = self.resume.reopen() {
            self.set_position(position_ms)?;
        }
        Ok(())
    }

    /// `IMFSourceReader::SetCurrentPosition` to `position_ms`.
    fn set_position(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        // MSDN IMFSourceReader::SetCurrentPosition:
        //   `guidtimeformat` must point to a GUID that identifies the time
        //   format. Use a pointer to GUID_NULL for 100-ns units — the call
        //   dereferences the GUID, so a null pointer causes an access
        //   violation on release-mode MF.
        //
        //   `varPosition` must be a PROPVARIANT of type VT_I8 (or VT_UI8).
        //
        // We construct the PROPVARIANT manually as a 24-byte stack buffer
        // (vt=VT_I8 at offset 0, hVal at offset 8) rather than via
        // `windows::core::PROPVARIANT::from(i64)`. The wrapper type has a
        // `Drop` impl that calls `PropVariantClear` from ole32.dll. For
        // VT_I8 there is nothing to free and avoiding the wrapper keeps
        // the release-mode LTO path clean (prior retry of this fix left
        // MF in a state where `ReadSample` returned EOS immediately on
        // fresh decoders — see the 2026-04-22 worship-training deploy).
        // u64 → i64 → checked × 10_000. Overflow is theoretical (i64 range is
        // ~290 years in milliseconds; playback positions are minutes-to-hours)
        // but the explicit guards surface a clean Seek error rather than silently
        // wrapping to a negative position that MF would reject mid-decode.
        let position_signed = i64::try_from(position_ms).map_err(|_| {
            DecoderError::Seek(format!("position_ms {position_ms} exceeds i64::MAX"))
        })?;
        let position_100ns: i64 = position_signed.checked_mul(10_000).ok_or_else(|| {
            DecoderError::Seek(format!(
                "position_ms {position_ms} ms × 10_000 overflows i64 (100-ns units)"
            ))
        })?;
        const VT_I8: u16 = 20;
        let mut raw: [u64; 3] = [0; 3]; // 24 bytes, 8-byte aligned for i64
        let raw_ptr = raw.as_mut_ptr().cast::<u8>();
        unsafe {
            raw_ptr.cast::<u16>().write(VT_I8);
            raw_ptr.add(8).cast::<i64>().write(position_100ns);
        }
        let var_ptr = raw.as_ptr().cast::<windows::core::PROPVARIANT>();
        // GUID_NULL is the all-zero GUID; stack-allocated so the pointer is
        // valid for the duration of the call.
        let guid_null = windows::core::GUID::from_u128(0);
        unsafe {
            self.reader
                .SetCurrentPosition(&guid_null, var_ptr)
                .map_err(|e| DecoderError::Seek(format!("SetCurrentPosition: {e}")))?;
        }
        debug!(position_ms, "mf_reader: seek complete");
        Ok(())
    }
}

/// `CoInitializeEx` (STA) and `MFStartup` on the calling thread.
pub(super) fn com_startup() -> Result<(), DecoderError> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            return Err(DecoderError::ComInit(format!("CoInitializeEx: {hr}")));
        }
        MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)
            .map_err(|e| DecoderError::ComInit(format!("MFStartup: {e}")))?;
    }
    Ok(())
}

/// A media type's `MF_MT_FRAME_SIZE` (0×0 when it has none).
fn frame_size(media_type: &IMFMediaType) -> (u32, u32) {
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
    ((size >> 32) as u32, size as u32)
}

impl MediaStream for MediaFoundationVideoReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.resume.on_seek(position_ms);
        self.set_position(position_ms)
    }
}

impl VideoStream for MediaFoundationVideoReader {
    #[cfg_attr(test, mutants::skip)]
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        loop {
            let read = if std::mem::take(&mut self.inject_failure) {
                Err(DecoderError::ReadSample(
                    "a decode failure injected by a test".into(),
                ))
            } else {
                self.read_picture()
            };
            match read {
                Ok(Some(frame)) => {
                    self.resume.delivered(frame.timestamp_ms);
                    return Ok(Some(frame));
                }
                Ok(None) => return Ok(None),
                Err(e) => match self.gate.on_error(&e) {
                    OnDecodeError::ReopenSoftware => self.reopen_in_software(&e)?,
                    OnDecodeError::Propagate => return Err(e),
                },
            }
        }
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
