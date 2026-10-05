//! Hardware video decode (#223 S3b): every decision of
//! `MediaFoundationVideoReader`'s opt-in Direct3D 11 path, pure and tested
//! on Linux. The Windows glue (`video/`, outside the mutation gate) only
//! calls Media Foundation and Direct3D.
//!
//! A reader opens a file in a [`DecodeMode`]. `Software` (the default) is the
//! path every song took before S3b. `Hardware` hands Media Foundation a
//! Direct3D 11 device (`sp_gpu::VideoDevice`) through a DXGI device manager,
//! so the decoder MFT decodes on the GPU (DXVA) and hands each picture over
//! as a DXGI surface, which the reader copies back into system memory in the
//! same NV12 layout as the software path ([`SurfaceLayout`]).
//!
//! The path really used is read per picture, never assumed: a picture in a
//! DXGI surface was decoded on the GPU, one in system memory in software
//! ([`DecodePath::of_picture`]). Media Foundation's decoder falls back to
//! software on its own when the GPU has no decoder for the stream
//! (Microsoft's "Supporting Direct3D 11 Video Decoding in Media Foundation",
//! "Fallback to Software Decoding"), so a `Hardware` reader can decode in
//! software without any error.
//!
//! The hardware path never kills a song:
//!
//! - a `Hardware` open that fails (no hardware adapter, no device, Media
//!   Foundation refusing the device manager) opens the file in software, with
//!   a WARN ([`FallbackStage::Open`]);
//! - a decode error on the D3D path (a lost device, a surface the readback
//!   refuses) reopens the file in software where the last picture handed over
//!   was, once per file, with a WARN ([`on_decode_error`], [`Resume`],
//!   [`FallbackStage::MidStream`]).
//!
//! [`HwCounters`] counts what the hardware path did across the process; the
//! decode bench reports one reader's, `GET /api/v1/status` the process's.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use sp_core::nv12::{nv12_chroma_row, nv12_len};
use sp_gpu::{MAX_PICTURE_SIDE, mapped_len};

use crate::error::DecoderError;

/// What a caller asks the reader for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecodeMode {
    /// Media Foundation's software decoders (no Direct3D device).
    #[default]
    Software,
    /// A Direct3D 11 device on the GPU for the decoder MFT (DXVA), with a
    /// software fallback.
    Hardware,
}

impl DecodeMode {
    /// `Hardware` when `hw` is set (the setting `video_hw_decode`, the
    /// decode bench's `hw`), else `Software`.
    pub fn from_hw_flag(hw: bool) -> Self {
        if hw {
            DecodeMode::Hardware
        } else {
            DecodeMode::Software
        }
    }
}

/// What really decoded a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePath {
    Software,
    Hardware,
}

impl DecodePath {
    /// The path of a picture Media Foundation handed over: a DXGI surface was
    /// decoded on the GPU, a buffer in system memory in software (also on the
    /// D3D path, when the decoder MFT fell back on its own).
    pub fn of_picture(dxgi_surface: bool) -> Self {
        if dxgi_surface {
            DecodePath::Hardware
        } else {
            DecodePath::Software
        }
    }

    /// `"hardware"` / `"software"` (the decode bench's `decode_path`).
    pub fn as_str(self) -> &'static str {
        match self {
            DecodePath::Software => "software",
            DecodePath::Hardware => "hardware",
        }
    }
}

/// Where the hardware path gave up for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackStage {
    /// The `Hardware` open failed; the file was opened in software.
    Open,
    /// A decode error on the D3D path; the file was reopened in software.
    MidStream,
}

impl FallbackStage {
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackStage::Open => "open",
            FallbackStage::MidStream => "mid-stream",
        }
    }
}

/// One file's fall back to software, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwFallback {
    pub stage: FallbackStage,
    pub reason: String,
}

impl HwFallback {
    /// `"<stage>: <reason>"`, as the bench and the status report it.
    pub fn describe(&self) -> String {
        format!("{}: {}", self.stage.as_str(), self.reason)
    }
}

/// What the reader does after a decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDecodeError {
    /// Reopen the file in software and go on from the last picture handed
    /// over.
    ReopenSoftware,
    /// Hand the error to the caller (as before S3b).
    Propagate,
}

/// What to do with `error`. A reader that may still fall back (it decodes on
/// the D3D path and has not fallen back yet) reopens in software; a host
/// out of memory for the picture's buffer (`FrameAlloc`) is not the
/// decoder's fault, so it goes to the caller like any error of a software
/// reader (the pipeline drops that one picture).
pub fn on_decode_error(fallback_allowed: bool, error: &DecoderError) -> OnDecodeError {
    if fallback_allowed && !matches!(error, DecoderError::FrameAlloc(_)) {
        OnDecodeError::ReopenSoftware
    } else {
        OnDecodeError::Propagate
    }
}

/// Where the reader is in its file, so a software reopen goes on where the
/// D3D path stopped: no picture handed over twice, none lost (the seek lands
/// on the keyframe before the position, so the pictures up to the last one
/// handed over are decoded again and dropped).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resume {
    /// The last seek's target, until a picture is handed over after it.
    seek_ms: Option<u64>,
    /// The last picture handed over since the last seek.
    delivered_ms: Option<u64>,
    /// After a reopen: pictures up to this one are dropped.
    skip_through_ms: Option<u64>,
}

impl Resume {
    /// The caller seeked to `position_ms`.
    pub fn on_seek(&mut self, position_ms: u64) {
        *self = Resume {
            seek_ms: Some(position_ms),
            delivered_ms: None,
            skip_through_ms: None,
        };
    }

    /// Whether the decoded picture at `timestamp_ms` is handed over (and,
    /// when it is, record it). After a reopen, the pictures up to and
    /// including the last one handed over are dropped.
    pub fn take(&mut self, timestamp_ms: u64) -> bool {
        if self
            .skip_through_ms
            .is_some_and(|last| timestamp_ms <= last)
        {
            return false;
        }
        self.delivered_ms = Some(timestamp_ms);
        true
    }

    /// The reader reopens the file: where to seek (`None`: the file's start,
    /// no seek), and from now on drop the pictures through the last one
    /// handed over. With none handed over since the last seek, the reopen
    /// seeks to that seek's target and drops nothing.
    pub fn reopen(&mut self) -> Option<u64> {
        match self.delivered_ms {
            Some(last) => {
                self.skip_through_ms = Some(last);
                Some(last)
            }
            None => self.seek_ms,
        }
    }
}

/// `DXGI_FORMAT_NV12`: the only surface format the readback takes.
pub const DXGI_FORMAT_NV12: u32 = 103;

/// A decoded picture as Media Foundation mapped its DXGI surface
/// (`IMF2DBuffer2::Lock2DSize`): luma rows `pitch` bytes apart for the
/// texture's `surface_rows` rows, then the UV plane (Direct3D's NV12
/// layout), and the picture's size (the negotiated type's `MF_MT_FRAME_SIZE`,
/// as the software path reads it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceLayout {
    /// The mapped surface's row pitch, bytes.
    pub pitch: usize,
    /// The texture's height (`D3D11_TEXTURE2D_DESC::Height`): the UV plane
    /// starts `pitch × surface_rows` bytes after row 0.
    pub surface_rows: usize,
    pub width: u32,
    pub height: u32,
}

/// Why a mapped surface cannot be read back (the reader then reopens the
/// file in software).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    #[error("the surface is DXGI format {0}, not NV12 (103)")]
    Format(u32),
    #[error("a {width}x{height} picture is empty or over 16384 on a side")]
    Size { width: u32, height: u32 },
    #[error("a pitch of {pitch} bytes cannot hold a row of {row} bytes")]
    Pitch { pitch: usize, row: usize },
    #[error("a surface of {surface_rows} rows cannot hold a {height}-row picture")]
    Rows { surface_rows: usize, height: u32 },
    #[error("the mapped surface holds {len} bytes, the picture needs {needed}")]
    Short { len: usize, needed: usize },
}

/// How a checked surface is copied into the packed picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceCopy {
    /// The packed picture's row stride: one chroma row (the width rounded up
    /// to even, `sp_core::nv12::nv12_chroma_row`), as the software path's
    /// NV12 rows.
    pub stride: u32,
    /// The packed picture's bytes: `nv12_len(stride, height)`.
    pub len: usize,
    pitch: usize,
    luma_rows: usize,
    chroma_rows: usize,
    /// Where the UV plane starts in the mapped bytes.
    chroma_offset: usize,
}

impl SurfaceLayout {
    /// Check the surface before anything is read: the format is NV12, the
    /// picture is not empty nor over Direct3D's 16384 side, a pitch holds a
    /// packed row, the surface holds the picture's rows, and the `mapped`
    /// bytes (from scanline 0 to the end of the mapping) hold every row the
    /// copy reads, up to the last UV row's last byte.
    pub fn check(&self, format: u32, mapped: usize) -> Result<SurfaceCopy, SurfaceError> {
        if format != DXGI_FORMAT_NV12 {
            return Err(SurfaceError::Format(format));
        }
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 || width > MAX_PICTURE_SIDE || height > MAX_PICTURE_SIDE {
            return Err(SurfaceError::Size { width, height });
        }
        let row = nv12_chroma_row(width);
        if self.pitch < row {
            return Err(SurfaceError::Pitch {
                pitch: self.pitch,
                row,
            });
        }
        let luma_rows = height as usize;
        if self.surface_rows < luma_rows {
            return Err(SurfaceError::Rows {
                surface_rows: self.surface_rows,
                height,
            });
        }
        let chroma_rows = luma_rows.div_ceil(2);
        // Overflow means no mapping could hold it: as short as can be.
        let short = SurfaceError::Short {
            len: mapped,
            needed: usize::MAX,
        };
        let chroma_offset = self.pitch.checked_mul(self.surface_rows).ok_or(short)?;
        let chroma = mapped_len(self.pitch, row, chroma_rows).ok_or(short)?;
        let needed = chroma_offset.checked_add(chroma).ok_or(short)?;
        if mapped < needed {
            return Err(SurfaceError::Short {
                len: mapped,
                needed,
            });
        }
        // `row` ≤ 16384, so it fits a `u32`.
        let stride = row as u32;
        Ok(SurfaceCopy {
            stride,
            len: nv12_len(stride, height),
            pitch: self.pitch,
            luma_rows,
            chroma_rows,
            chroma_offset,
        })
    }
}

impl SurfaceCopy {
    /// Append the packed picture to `dst`: the first `stride` bytes of each
    /// luma row, then of each UV row. `mapped` is the checked mapping (at
    /// least the `mapped` bytes [`SurfaceLayout::check`] was given).
    pub fn copy(&self, mapped: &[u8], dst: &mut Vec<u8>) {
        let row = self.stride as usize;
        let (luma, chroma) = mapped.split_at(self.chroma_offset);
        for line in luma.chunks(self.pitch).take(self.luma_rows) {
            dst.extend_from_slice(&line[..row]);
        }
        for line in chroma.chunks(self.pitch).take(self.chroma_rows) {
            dst.extend_from_slice(&line[..row]);
        }
    }
}

/// The bytes from scanline 0 to the end of a mapping that starts at `start`
/// and is `len` bytes long (`Lock2DSize`'s bounds). `None` when scanline 0
/// lies outside it.
pub fn mapped_from_scanline0(start: usize, scanline0: usize, len: usize) -> Option<usize> {
    let offset = scanline0.checked_sub(start)?;
    len.checked_sub(offset)
}

/// What the hardware path did across the process, every reader opened in
/// `Hardware` mode (the decode bench's included).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HwDecodeStats {
    /// Readers opened in `Hardware` mode.
    pub requested: u64,
    /// Of those, the ones whose first picture came out of the GPU decoder.
    pub hardware: u64,
    /// Of those, the ones on the D3D path whose first picture Media
    /// Foundation's decoder made in software (it found no decoder on the
    /// GPU for the stream).
    pub mf_software: u64,
    /// Of those, the ones whose `Hardware` open failed (opened in software).
    pub open_fallbacks: u64,
    /// Reopens in software after a decode error on the D3D path.
    pub mid_stream_fallbacks: u64,
    /// The last fall back, [`HwFallback::describe`].
    pub last_fallback: Option<String>,
}

/// The process's [`HwDecodeStats`], counted as the readers go.
#[derive(Debug, Default)]
pub struct HwCounters {
    requested: AtomicU64,
    hardware: AtomicU64,
    mf_software: AtomicU64,
    open_fallbacks: AtomicU64,
    mid_stream_fallbacks: AtomicU64,
    last_fallback: Mutex<Option<String>>,
}

impl HwCounters {
    const fn zero() -> Self {
        HwCounters {
            requested: AtomicU64::new(0),
            hardware: AtomicU64::new(0),
            mf_software: AtomicU64::new(0),
            open_fallbacks: AtomicU64::new(0),
            mid_stream_fallbacks: AtomicU64::new(0),
            last_fallback: Mutex::new(None),
        }
    }

    /// A reader was opened in `Hardware` mode.
    pub fn requested(&self) {
        self.requested.fetch_add(1, Ordering::Relaxed);
    }

    /// The first picture of a reader on the D3D path came out of `path`.
    pub fn first_picture(&self, path: DecodePath) {
        let counter = match path {
            DecodePath::Hardware => &self.hardware,
            DecodePath::Software => &self.mf_software,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A reader fell back to software.
    pub fn fell_back(&self, fallback: &HwFallback) {
        let counter = match fallback.stage {
            FallbackStage::Open => &self.open_fallbacks,
            FallbackStage::MidStream => &self.mid_stream_fallbacks,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let mut last = self
            .last_fallback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *last = Some(fallback.describe());
    }

    /// What was counted so far.
    pub fn snapshot(&self) -> HwDecodeStats {
        let last_fallback = self
            .last_fallback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        HwDecodeStats {
            requested: self.requested.load(Ordering::Relaxed),
            hardware: self.hardware.load(Ordering::Relaxed),
            mf_software: self.mf_software.load(Ordering::Relaxed),
            open_fallbacks: self.open_fallbacks.load(Ordering::Relaxed),
            mid_stream_fallbacks: self.mid_stream_fallbacks.load(Ordering::Relaxed),
            last_fallback,
        }
    }
}

static COUNTERS: HwCounters = HwCounters::zero();

/// The process's hardware decode counters (every reader records into them).
pub fn hw_counters() -> &'static HwCounters {
    &COUNTERS
}

#[cfg(test)]
#[path = "hw_decode_tests.rs"]
mod tests;
