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
//! The path really used is read per picture, not from the mode: a picture in
//! a decoder texture (a DXGI surface whose texture carries
//! `D3D11_BIND_DECODER`) is taken to have come out of the GPU decoder; one
//! in system memory, or in a texture a software decoder uploaded into, did
//! not ([`DecodePath::of_picture`]). It is an inference from the texture
//! (Microsoft's DXVA decoders write into such textures); the box's decode
//! bench timings are its proof. Media Foundation's decoder falls back to
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
use sp_gpu::{MAX_PICTURE_SIDE, mapped_len, unpad_rows_into};

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

/// `D3D11_BIND_DECODER`: a texture a DXVA decoder decodes into
/// (Microsoft: its output texture array's "binding flags should include the
/// D3D11_BIND_DECODER flag").
pub const D3D11_BIND_DECODER: u32 = 0x200;

impl DecodePath {
    /// The path of a picture Media Foundation handed over, from the bind
    /// flags of the texture its DXGI surface lies in (`None`: a buffer in
    /// system memory). Only a decoder texture's picture came out of the GPU
    /// decoder; a system-memory one (also on the D3D path, when the decoder
    /// MFT fell back on its own) or one a software decoder uploaded into a
    /// texture did not.
    pub fn of_picture(surface_bind_flags: Option<u32>) -> Self {
        match surface_bind_flags {
            Some(flags) if flags & D3D11_BIND_DECODER != 0 => DecodePath::Hardware,
            _ => DecodePath::Software,
        }
    }

    /// The path of a picture handed over in a DXGI surface whose texture
    /// carries `surface_bind_flags`. A file opened at the decode cap
    /// (`scaled`, #282) lies in the texture of Media Foundation's video
    /// processor, which scaled it on the GPU after the decoder: that texture
    /// carries no `D3D11_BIND_DECODER`, so the decoder's own path cannot be
    /// read from it, and the picture counts as `Hardware`: it lies on the
    /// GPU, which scaled it. Whether the GPU also DECODED it is not visible
    /// behind the processor (a decoder MFT's own fall back to software, a
    /// GPU with no decoder for the codec); the box's bench timings tell it.
    /// Unscaled, [`DecodePath::of_picture`].
    pub fn of_surface(surface_bind_flags: u32, scaled: bool) -> Self {
        if scaled {
            DecodePath::Hardware
        } else {
            DecodePath::of_picture(Some(surface_bind_flags))
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

/// Whether a decode error may still reopen the file in software: once per
/// file, for a reader armed when it opened on the D3D path (or by a test's
/// injected failure).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FallbackGate {
    allowed: bool,
}

impl FallbackGate {
    /// The reader may fall back (once).
    pub fn arm(&mut self) {
        self.allowed = true;
    }

    /// What to do with `error` ([`on_decode_error`]); a reopen uses the
    /// allowance up, so a later error goes to the caller.
    pub fn on_error(&mut self, error: &DecoderError) -> OnDecodeError {
        let action = on_decode_error(self.allowed, error);
        if action == OnDecodeError::ReopenSoftware {
            self.allowed = false;
        }
        action
    }
}

/// What a handed-over picture's path means for the counters and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathNote {
    /// Nothing new.
    Nothing,
    /// The first picture of a reader on the D3D path: counted
    /// ([`HwCounters::first_picture`]) and logged, so a silent software
    /// decode is seen.
    First,
    /// A later picture on the D3D path came out of another path than the one
    /// before (Media Foundation's decoder changed its mind mid-file, with no
    /// error): counted ([`HwCounters::path_changed`]) and logged.
    Changed { from: DecodePath },
}

/// The path of the pictures a reader handed over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathTracker {
    last: Option<DecodePath>,
    /// How often a picture's path differed from the one before it.
    changes: u32,
}

impl PathTracker {
    /// Record a handed-over picture's `path`, and say what is new about it
    /// while the reader runs on the D3D path (`on_d3d`). Off it (software
    /// from the start, or after a fall back, which has its own WARN) nothing
    /// is new. Every change of path is counted ([`PathTracker::changes`]),
    /// a fall back's too.
    pub fn observe(&mut self, path: DecodePath, on_d3d: bool) -> PathNote {
        let before = self.last.replace(path);
        if before.is_some_and(|from| from != path) {
            self.changes = self.changes.saturating_add(1);
        }
        match before {
            None if on_d3d => PathNote::First,
            Some(from) if on_d3d && from != path => PathNote::Changed { from },
            _ => PathNote::Nothing,
        }
    }

    /// How often a picture's path differed from the one before it, for any
    /// reason (a fall back, or Media Foundation's decoder changing its
    /// mind): 0 = every picture came out of one path. The decode bench
    /// reports it, so a run whose timings mix two paths is seen.
    pub fn changes(&self) -> u32 {
        self.changes
    }

    /// The last picture's path (`None` before the first).
    pub fn last(&self) -> Option<DecodePath> {
        self.last
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

    /// Whether the decoded picture at `timestamp_ms` is dropped: after a
    /// reopen, the pictures up to and including the last one handed over.
    /// Asked before the picture is read back, so a dropped one costs no copy.
    pub fn skips(&self, timestamp_ms: u64) -> bool {
        self.skip_through_ms
            .is_some_and(|last| timestamp_ms <= last)
    }

    /// The picture at `timestamp_ms` was handed over.
    pub fn delivered(&mut self, timestamp_ms: u64) {
        self.delivered_ms = Some(timestamp_ms);
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
    /// The texture's width in pixels (`D3D11_TEXTURE2D_DESC::Width`).
    pub surface_cols: usize,
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
    #[error("a surface {surface_cols} pixels wide cannot hold a {width}-pixel picture")]
    Cols { surface_cols: usize, width: u32 },
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
    /// The mapped bytes the copy reads, up to the last UV row's last byte.
    pub needed: usize,
    pitch: usize,
    luma_rows: usize,
    chroma_rows: usize,
    /// Where the UV plane starts in the mapped bytes.
    chroma_offset: usize,
}

impl SurfaceLayout {
    /// Check the surface before anything is read: the format is NV12, the
    /// picture is not empty nor over Direct3D's 16384 side, the surface is as
    /// wide and as tall as the picture (else the copy would read a row's
    /// padding as pixels), a pitch holds a packed row, and the `mapped` bytes
    /// (from scanline 0 to the end of the mapping) hold every row the copy
    /// reads, up to the last UV row's last byte.
    pub fn check(&self, format: u32, mapped: usize) -> Result<SurfaceCopy, SurfaceError> {
        if format != DXGI_FORMAT_NV12 {
            return Err(SurfaceError::Format(format));
        }
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 || width > MAX_PICTURE_SIDE || height > MAX_PICTURE_SIDE {
            return Err(SurfaceError::Size { width, height });
        }
        if width as usize > self.surface_cols {
            return Err(SurfaceError::Cols {
                surface_cols: self.surface_cols,
                width,
            });
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
            needed,
            pitch: self.pitch,
            luma_rows,
            chroma_rows,
            chroma_offset,
        })
    }
}

impl SurfaceCopy {
    /// Append the packed picture to `dst`: the first `stride` bytes of each
    /// luma row, then of each UV row (`sp_gpu::unpad_rows_into`, the rows
    /// the compositor's readback packs). A `mapped` shorter than
    /// [`SurfaceCopy::needed`] is refused before anything is appended.
    pub fn copy(&self, mapped: &[u8], dst: &mut Vec<u8>) -> Result<(), SurfaceError> {
        let short = SurfaceError::Short {
            len: mapped.len(),
            needed: self.needed,
        };
        if mapped.len() < self.needed {
            return Err(short);
        }
        let row = self.stride as usize;
        let (luma, chroma) = mapped.split_at(self.chroma_offset);
        unpad_rows_into(luma, self.pitch, row, self.luma_rows, dst).ok_or(short)?;
        unpad_rows_into(chroma, self.pitch, row, self.chroma_rows, dst).ok_or(short)?;
        Ok(())
    }
}

/// The size a D3D11-decoded picture is handed over at: the stream's own
/// size (the native media type's `MF_MT_FRAME_SIZE`, read at open), the
/// top-left of the decoded surface, when it fits on both sides; else the
/// decoded size. The hardware decoder pads its surface and the current
/// type's frame size to 16 rows (NVDEC: a 1920×1080 AV1 decodes as
/// 1920×1088, 2048×858 as 2048×864), and those rows are no picture
/// (Y = U = V = 0: a green strip at the bottom of SP-program and
/// SP-program-MAX, #223 follow-up). The software path hands over the
/// stream's size already.
pub fn visible_size(decoded: (u32, u32), native: Option<(u32, u32)>) -> (u32, u32) {
    match native {
        Some((w, h)) if w > 0 && h > 0 && w <= decoded.0 && h <= decoded.1 => (w, h),
        _ => decoded,
    }
}

/// The bytes from scanline 0 to the end of a mapping that starts at `start`
/// and is `len` bytes long (`Lock2DSize`'s bounds). `None` when scanline 0
/// lies outside it.
pub fn mapped_from_scanline0(start: usize, scanline0: usize, len: usize) -> Option<usize> {
    let offset = scanline0.checked_sub(start)?;
    len.checked_sub(offset)
}

// ---------------------------------------------------------------------------
// The decode cap (#282)
// ---------------------------------------------------------------------------
//
// The setting `max_resolution` caps download AND decode: the operator's
// switch per site (the owner's ruling on #282: a permanent switch, set
// lower while 4K is a problem, e.g. until #279's GPU-resident path). A
// picture larger than the cap's 16:9 box is opened scaled: Media
// Foundation's video processor
// (`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` + the NV12 output
// type's `MF_MT_FRAME_SIZE`) scales it at decode, on the GPU on the D3D11
// path, so the readback, the frame pool, the MAX upload and the canvas all
// carry the smaller picture. A picture inside the box opens exactly as
// before.

/// The box's aspect, width per height: 16:9.
const BOX_ASPECT_W: u64 = 16;
const BOX_ASPECT_H: u64 = 9;

/// Euclid's worst case on `u128` (consecutive Fibonacci numbers) is 185
/// steps; the bound keeps a mutated step from looping forever.
const EUCLID_MAX_STEPS: u32 = 200;

/// The box of the decode cap `cap` (rows): `cap` × 16/9 wide and `cap`
/// tall, each side rounded down to even (an odd cap set through the API
/// would otherwise let the nearest-even rounding of [`scaled_size`] give a
/// picture one row over the box). 1080 → 1920×1080, 1440 → 2560×1440,
/// 2160 → 3840×2160, 480 → 852×480.
pub fn decode_box(cap: u32) -> (u64, u64) {
    let cap = u64::from(cap);
    ((cap * BOX_ASPECT_W / BOX_ASPECT_H) & !1, cap & !1)
}

/// `num / den` rounded to the nearest even whole number, a tie (an odd
/// whole number) upwards: 803 → 804, 802.9 → 802, 1919.2 → 1920.
fn nearest_even(num: u128, den: u128) -> u128 {
    (num + den) / (2 * den) * 2
}

/// The size a `native` picture is decoded at under the cap `cap`: scaled
/// to fit inside [`decode_box`], its aspect kept (scaled by
/// min(box width / width, box height / height); the side that binds takes
/// the box's own size, the other is rounded to the nearest even), each side
/// at least 2. `None` (decoded at its own size, exactly as before #282)
/// for a picture already inside the box (never upscaled), a size not known
/// (a side 0), or a cap under 2 rows (it caps nothing).
///
/// 3840×2160 at 1080 → 1920×1080; 3840×1606 → 1920×804; 2880×2160 at 1440
/// → 1920×1440; 1920×1080 at 1080 → `None`.
pub fn scaled_size(native: (u32, u32), cap: u32) -> Option<(u32, u32)> {
    let (w, h) = (u128::from(native.0), u128::from(native.1));
    if w == 0 || h == 0 {
        return None;
    }
    let (bw, bh) = decode_box(cap);
    let (bw, bh) = (u128::from(bw), u128::from(bh));
    if bh < 2 {
        return None;
    }
    if w <= bw && h <= bh {
        return None;
    }
    // Which side binds: bw / w against bh / h, cross-multiplied. At an equal
    // aspect both give the box itself.
    let (sw, sh) = match (bw * h).cmp(&(bh * w)) {
        std::cmp::Ordering::Greater => (nearest_even(w * bh, h), bh),
        _ => (bw, nearest_even(h * bw, w)),
    };
    // Each side is at most the native one (a scale under 1, rounded to the
    // nearest even), so it fits a `u32`.
    Some((sw.max(2) as u32, sh.max(2) as u32))
}

/// The size a D3D11 picture's readback cuts the decoded surface to
/// ([`visible_size`]): the size asked of the video processor when the file
/// was opened `scaled` (the stream's own size would never fit the scaled
/// surface), else the stream's own size (`native`, #223 follow-up).
pub fn visible_reference(
    native: Option<(u32, u32)>,
    scaled: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    scaled.or(native)
}

/// Whether Media Foundation took a scaled open: the type it negotiated is
/// exactly the `requested` size. Behind the video processor the output
/// type is ours and the processor draws the picture into all of it, so a
/// larger type (even by NVDEC's padding) would be picture the readback
/// cuts off, and a reader that kept the native size would hand over the
/// top-left of the native picture on the D3D11 path. Anything but the
/// exact size is a refusal: the file plays at its own size, counted in
/// `scale_refusals`. (A padded TEXTURE with an exact type is fine: the
/// readback takes the UV plane's offset from the texture's height.)
pub fn scale_took(requested: (u32, u32), negotiated: (u32, u32)) -> bool {
    requested == negotiated
}

/// #282: whether a picture's surface tells the DECODER's path: only on
/// the D3D11 path of an unscaled open. A scaled open's picture lies in
/// the video processor's texture ([`DecodePath::of_surface`]), so its
/// first picture is neither a GPU decode nor a software one in
/// [`HwCounters`] (the scaled open is counted in `scaled_opens`); the
/// decoder's own path behind the processor shows only in the bench's
/// timings.
pub fn decoder_path_visible(on_d3d: bool, scaled: bool) -> bool {
    on_d3d && !scaled
}

/// #282: the frame rate a reader reports: the negotiated type's. A scaled
/// open's output type carries no rate of ours (setting one could start the
/// video processor's frame-rate conversion when the decoder reports
/// another), so when the processor's type has none either, the native
/// type's rate is reported instead of the 29.97 guess. An unscaled open
/// with none keeps that guess, as before #282.
pub fn scaled_frame_rate(
    negotiated: Option<u64>,
    native: Option<u64>,
    scaled: bool,
) -> Option<u64> {
    match (negotiated, scaled) {
        (Some(rate), _) => Some(rate),
        (None, true) => native,
        (None, false) => None,
    }
}

/// #282: a display aperture (`MF_MT_MINIMUM_DISPLAY_APERTURE`, an
/// `MFVideoArea`): the picture's whole-pixel offset and size inside a
/// decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoArea {
    pub x: i16,
    pub y: i16,
    pub width: i32,
    pub height: i32,
}

/// #282: an `MFVideoArea` from its 16-byte blob (little-endian:
/// `OffsetX {fract: u16, value: i16}`, `OffsetY`, then `SIZE {cx, cy}` as
/// two `i32`); `None` for any other length.
pub fn video_area(blob: &[u8]) -> Option<VideoArea> {
    let b: &[u8; 16] = blob.try_into().ok()?;
    Some(VideoArea {
        x: i16::from_le_bytes([b[2], b[3]]),
        y: i16::from_le_bytes([b[6], b[7]]),
        width: i32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        height: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
    })
}

/// #282 (review round 2): whether a scaled open would scale the decoder's
/// padding into the picture. NVDEC pads its output type to 16 rows (a
/// 3840×1606 stream decodes as 3840×1616, #223's green strip); the video
/// processor behind it is taken to crop the padding only by the decoder
/// type's display aperture (reasoned in review round 2, not measured). So
/// a `decoded` type larger than the `native` picture is safe only with an
/// aperture of exactly the native picture at the origin; otherwise the
/// scaled open is refused (the file plays at its own size, the readback
/// cutting the padding as before).
pub fn padding_reaches_processor(
    decoded: (u32, u32),
    aperture: Option<VideoArea>,
    native: (u32, u32),
) -> bool {
    let padded = decoded.0 > native.0 || decoded.1 > native.1;
    padded && !aperture_is(aperture, native)
}

/// Whether `aperture` is exactly the `native` picture at the origin.
fn aperture_is(aperture: Option<VideoArea>, native: (u32, u32)) -> bool {
    aperture.is_some_and(|area| {
        area.x == 0
            && area.y == 0
            && i64::from(area.width) == i64::from(native.0)
            && i64::from(area.height) == i64::from(native.1)
    })
}

/// NVDEC pads its surfaces and output types to whole 16-pixel blocks
/// (1080 → 1088, 858 → 864, 1350 → 1360 on SNV; 1440 / 1280 / 720 never).
const DECODER_BLOCK: u32 = 16;

/// #282 (review round 3): whether a scaled open may go on, judged at its
/// open from what the decoder hands the video processor (`decoder`: its
/// output type's size and display aperture; `None` when the source
/// reader's chain could not be read).
///
/// - The decoder's padding in reach of the processor
///   ([`padding_reaches_processor`]): refused, on either path and for any
///   size (whole 16-pixel blocks included).
/// - On the D3D11 path the decoder may only learn its padded size at the
///   first frame, after this check (a VP9 / AV1 sample entry carries no
///   frame header), so a stream whose size is not whole 16-pixel blocks is
///   scaled only when the decoder has already shown the native aperture.
///   NVDEC's 16-row padding cannot reach a stream of whole blocks
///   (3840×2160, 2560×1440, 4096×2160).
/// - In software only the padding seen at open refuses, and an unreadable
///   chain goes on. What is proven of the software decoders: on CI's
///   runner its H.264 software decoder showed the stream's own size at open
///   with no aperture (160×120 for the fixture coded 160×128, CI run
///   39040435639); the VP9 / AV1 software decoders, and any decoder's type
///   after its first frame, are not proven here. The reader logs the
///   decoder's output again at the first scaled picture (one INFO).
///
/// A refusal plays the file at its own size: never a green strip.
pub fn scaled_open_safe(
    native: (u32, u32),
    decoder: Option<((u32, u32), Option<VideoArea>)>,
    on_d3d: bool,
) -> bool {
    let whole_blocks =
        native.0.is_multiple_of(DECODER_BLOCK) && native.1.is_multiple_of(DECODER_BLOCK);
    match decoder {
        Some((size, aperture)) => {
            !padding_reaches_processor(size, aperture, native)
                && (!on_d3d || whole_blocks || aperture_is(aperture, native))
        }
        None => !on_d3d || whole_blocks,
    }
}

/// `a` and `b`'s greatest common divisor (`a` when `b` is 0).
fn gcd(mut a: u128, mut b: u128) -> u128 {
    for _ in 0..EUCLID_MAX_STEPS {
        if b == 0 {
            break;
        }
        (a, b) = (b, a % b);
    }
    a
}

/// #282: the pixel aspect ratio of a scaled open's output type that keeps
/// the picture's display aspect exactly: the `native` size at its own
/// `native_par` (`MF_MT_PIXEL_ASPECT_RATIO`; none or a zero term = square
/// pixels) shown at the `scaled` size, reduced. The nearest-even rounding
/// of [`scaled_size`] leaves the scaled size a fraction of a pixel off the
/// native aspect (3840×1606 → 1920×804: 804:803); this ratio states the
/// picture's true pixel aspect at that size (the stretch, 0.1 %, is
/// invisible). Whether the source reader keeps it is unproven for an NV12
/// output: Wine's Windows-run `test_source_reader_aspect_ratio` shows an
/// ARGB32 output's ratio absent from the current type. The CI test's
/// software 66×50 edge bands prove the picture fills the frame, not which
/// attribute fills it (the ratio and the frame size agree there); the CI
/// log prints the negotiated ratio. `None` when a size is 0 or a reduced
/// term does not fit a `u32` (the type then carries no ratio).
pub fn scaled_pixel_aspect(
    native: (u32, u32),
    native_par: Option<(u32, u32)>,
    scaled: (u32, u32),
) -> Option<(u32, u32)> {
    let (par_w, par_h) = match native_par {
        Some((w, h)) if w > 0 && h > 0 => (u128::from(w), u128::from(h)),
        _ => (1, 1),
    };
    let num = par_w * u128::from(native.0) * u128::from(scaled.1);
    let den = par_h * u128::from(native.1) * u128::from(scaled.0);
    if num == 0 || den == 0 {
        return None;
    }
    let common = gcd(num, den);
    Some((
        u32::try_from(num / common).ok()?,
        u32::try_from(den / common).ok()?,
    ))
}

/// What the hardware path did across the process, every open asked for in
/// `Hardware` mode (the decode bench's included); and (#282) the scaled
/// opens of any mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HwDecodeStats {
    /// Opens asked for in `Hardware` mode, counted before the open: a file
    /// that then opened nowhere (no reader at all) counts here and in none of
    /// the counts below.
    pub requested: u64,
    /// Of those, the ones whose first picture came out of the GPU decoder
    /// (a scaled open's never counts here nor in `mf_software`:
    /// [`decoder_path_visible`]).
    pub gpu_decodes: u64,
    /// Of those, the ones on the D3D path whose first picture Media
    /// Foundation's decoder made in software (it found no decoder on the
    /// GPU for the stream).
    pub mf_software: u64,
    /// Of those, the ones whose `Hardware` open failed and that opened in
    /// software.
    pub open_fallbacks: u64,
    /// Reopens in software after a decode error on the D3D path.
    pub mid_stream_fallbacks: u64,
    /// Changes of path mid-file on the D3D path with no error
    /// ([`PathNote::Changed`]).
    pub path_changes: u64,
    /// The last fall back, [`HwFallback::describe`].
    pub last_fallback: Option<String>,
    /// #282: opens of a file larger than the decode cap's box that Media
    /// Foundation scaled at decode, in any mode (a software reopen after a
    /// decode error on the D3D path counts again).
    pub scaled_opens: u64,
    /// #282: scaled opens Media Foundation refused, negotiated at another
    /// size ([`scale_took`]), or that [`scaled_open_safe`] refused (the
    /// decoder's padding in reach of the processor, or on the D3D11 path a
    /// stream that is not whole 16-pixel blocks with no native aperture
    /// shown); each of those readers then opens that file at its own size,
    /// when it opens at all (a refused D3D11 open that then fell back to
    /// software may open it scaled there, counted in `scaled_opens`).
    pub scale_refusals: u64,
}

/// The process's [`HwDecodeStats`], counted as the readers go (every
/// `Hardware` open, and the scaled opens of any mode).
#[derive(Debug, Default)]
pub struct HwCounters {
    requested: AtomicU64,
    gpu_decodes: AtomicU64,
    mf_software: AtomicU64,
    open_fallbacks: AtomicU64,
    mid_stream_fallbacks: AtomicU64,
    path_changes: AtomicU64,
    last_fallback: Mutex<Option<String>>,
    scaled_opens: AtomicU64,
    scale_refusals: AtomicU64,
}

impl HwCounters {
    const fn zero() -> Self {
        HwCounters {
            requested: AtomicU64::new(0),
            gpu_decodes: AtomicU64::new(0),
            mf_software: AtomicU64::new(0),
            open_fallbacks: AtomicU64::new(0),
            mid_stream_fallbacks: AtomicU64::new(0),
            path_changes: AtomicU64::new(0),
            last_fallback: Mutex::new(None),
            scaled_opens: AtomicU64::new(0),
            scale_refusals: AtomicU64::new(0),
        }
    }

    /// #282: a file opened scaled to the decode cap.
    pub fn scaled_open(&self) {
        self.scaled_opens.fetch_add(1, Ordering::Relaxed);
    }

    /// #282: Media Foundation refused a scaled open (the file opened at its
    /// own size).
    pub fn scale_refused(&self) {
        self.scale_refusals.fetch_add(1, Ordering::Relaxed);
    }

    /// A `Hardware` open was asked for (counted before the open).
    pub fn requested(&self) {
        self.requested.fetch_add(1, Ordering::Relaxed);
    }

    /// The first picture of a reader on the D3D path came out of `path`.
    pub fn first_picture(&self, path: DecodePath) {
        let counter = match path {
            DecodePath::Hardware => &self.gpu_decodes,
            DecodePath::Software => &self.mf_software,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A reader on the D3D path changed its pictures' path mid-file.
    pub fn path_changed(&self) {
        self.path_changes.fetch_add(1, Ordering::Relaxed);
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
            gpu_decodes: self.gpu_decodes.load(Ordering::Relaxed),
            mf_software: self.mf_software.load(Ordering::Relaxed),
            open_fallbacks: self.open_fallbacks.load(Ordering::Relaxed),
            mid_stream_fallbacks: self.mid_stream_fallbacks.load(Ordering::Relaxed),
            path_changes: self.path_changes.load(Ordering::Relaxed),
            last_fallback,
            scaled_opens: self.scaled_opens.load(Ordering::Relaxed),
            scale_refusals: self.scale_refusals.load(Ordering::Relaxed),
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

#[cfg(test)]
#[path = "hw_decode_tests_scale.rs"]
mod tests_scale;
