//! #212 (B3 of EPIC #174): the RECEIVE half of the NDI SDK — a receiver bound
//! to a FrameSync, so a received NDI source is pulled on SongPlayer's own
//! genlock grid (time-base correction by the SDK, not a hand-rolled buffer).
//!
//! Layouts and signatures are copied from the official SDK headers
//! (`Processing.NDI.Recv.h`, `Processing.NDI.FrameSync.h`,
//! `Processing.NDI.Find.h`, `Processing.NDI.structs.h`; Vizrt NDI AB
//! 2023-2026) — see the design record on #212 and the `Anchors-confirmed`
//! comment:
//!
//! - `NDIlib_framesync_capture_audio` fills an `NDIlib_audio_frame_v2_t`
//!   (planar float + `channel_stride_in_bytes`, NO FourCC field), and
//!   `NDIlib_framesync_free_audio` frees that same v2 struct.
//! - `NDIlib_framesync_capture_video(fs, frame, field_type)` returns an ALL-ZERO
//!   frame until the first video arrived; the same frame may be returned again
//!   (a repeat) and frames may be skipped (a drop) — that is the time-base
//!   correction.
//! - The received FourCC is read as a plain `u32` (never the send-side Rust
//!   enum): an unknown value in a Rust enum would be undefined behaviour.
//!
//! The receive symbols are resolved OPTIONALLY (`NdiLib::recv`): a runtime
//! without them keeps every sender working and only has no input.
//!
//! [`NdiReceiveBackend`] is the mockable seam (the same pattern as
//! [`crate::NdiBackend`]): [`RealNdiReceiveBackend`] calls the SDK;
//! `MockNdiReceiveBackend` (`receive_mock.rs`, `sp_ndi::test_util`) records the
//! calls for Linux tests. The safe RAII wrapper is `receiver::NdiFrameSync`.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::{Arc, Mutex};

use tracing::{debug, info, warn};

use crate::error::NdiError;
use crate::ndi_sdk::NdiLib;
use crate::types::{NDIlib_find_create_t, NDIlib_source_t};

// ---------------------------------------------------------------------------
// FFI constants (Processing.NDI.Recv.h / structs.h)
// ---------------------------------------------------------------------------

/// `NDIlib_recv_color_format_fastest`: UYVY for a source without alpha, UYVA
/// (a UYVY plane followed by an alpha plane) for one with alpha — so ONE
/// UYVY→NV12 converter covers both (#212 design, documented choice).
pub const RECV_COLOR_FORMAT_FASTEST: i32 = 100;
/// `NDIlib_recv_bandwidth_highest`: full resolution.
pub const RECV_BANDWIDTH_HIGHEST: i32 = 100;
/// `NDIlib_frame_format_type_progressive`: the FrameSync capture's field type.
pub const FRAME_FORMAT_TYPE_PROGRESSIVE: i32 = 1;
/// `NDI_LIB_FOURCC('U','Y','V','Y')`.
pub const FOURCC_UYVY: u32 = 0x5956_5955;
/// `NDI_LIB_FOURCC('U','Y','V','A')`.
pub const FOURCC_UYVA: u32 = 0x4156_5955;

// ---------------------------------------------------------------------------
// FFI types
// ---------------------------------------------------------------------------

/// Opaque `NDIlib_recv_instance_t`.
#[allow(non_camel_case_types)]
pub enum NDIlib_recv_instance_t {}

/// Opaque `NDIlib_framesync_instance_t`.
#[allow(non_camel_case_types)]
pub enum NDIlib_framesync_instance_t {}

/// `NDIlib_recv_create_v3_t`, field for field.
#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct NDIlib_recv_create_v3_t {
    /// The source to connect to (by `p_ndi_name`, `"MACHINE (stream)"`; a null
    /// URL lets the SDK find it on the network).
    pub source_to_connect_to: NDIlib_source_t,
    /// `NDIlib_recv_color_format_e` (a 32-bit C enum).
    pub color_format: i32,
    /// `NDIlib_recv_bandwidth_e` (a 32-bit C enum).
    pub bandwidth: i32,
    /// `false` = every frame is progressive (fields are de-interlaced).
    pub allow_video_fields: bool,
    /// The receiver's own name (UTF-8, NUL-terminated), or null.
    pub p_ndi_recv_name: *const c_char,
}

/// `NDIlib_video_frame_v2_t` as RECEIVED: the send-side layout
/// (`types::NDIlib_video_frame_v2_t`) with the FourCC as a plain `u32` and a
/// mutable data pointer owned by the SDK until it is freed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct NDIlib_video_frame_v2_recv_t {
    pub xres: i32,
    pub yres: i32,
    /// `NDIlib_FourCC_video_type_e` as a raw `u32`.
    pub four_cc: u32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
    pub picture_aspect_ratio: f32,
    pub frame_format_type: i32,
    /// Timecode of this frame, 100 ns.
    pub timecode: i64,
    pub p_data: *mut u8,
    /// Union with `data_size_in_bytes` (compressed types only).
    pub line_stride_in_bytes: i32,
    pub p_metadata: *const c_char,
    pub timestamp: i64,
}

impl NDIlib_video_frame_v2_recv_t {
    /// The all-zero frame (what FrameSync returns before any video arrived).
    pub fn empty() -> Self {
        Self {
            xres: 0,
            yres: 0,
            four_cc: 0,
            frame_rate_n: 0,
            frame_rate_d: 0,
            picture_aspect_ratio: 0.0,
            frame_format_type: 0,
            timecode: 0,
            p_data: ptr::null_mut(),
            line_stride_in_bytes: 0,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

/// `NDIlib_audio_frame_v2_t` (planar float, one plane per channel,
/// `channel_stride_in_bytes` apart) — the struct `NDIlib_framesync_capture_audio`
/// fills.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct NDIlib_audio_frame_v2_t {
    pub sample_rate: i32,
    pub no_channels: i32,
    /// Samples PER CHANNEL.
    pub no_samples: i32,
    pub timecode: i64,
    pub p_data: *mut f32,
    pub channel_stride_in_bytes: i32,
    pub p_metadata: *const c_char,
    pub timestamp: i64,
}

impl NDIlib_audio_frame_v2_t {
    /// The all-zero frame.
    pub fn empty() -> Self {
        Self {
            sample_rate: 0,
            no_channels: 0,
            no_samples: 0,
            timecode: 0,
            p_data: ptr::null_mut(),
            channel_stride_in_bytes: 0,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Function-pointer types (the C signatures, verbatim)
// ---------------------------------------------------------------------------

pub(crate) type FnRecvCreateV3 =
    unsafe extern "C" fn(*const NDIlib_recv_create_v3_t) -> *mut NDIlib_recv_instance_t;
pub(crate) type FnRecvDestroy = unsafe extern "C" fn(*mut NDIlib_recv_instance_t);
pub(crate) type FnRecvGetNoConnections = unsafe extern "C" fn(*mut NDIlib_recv_instance_t) -> i32;
pub(crate) type FnFramesyncCreate =
    unsafe extern "C" fn(*mut NDIlib_recv_instance_t) -> *mut NDIlib_framesync_instance_t;
pub(crate) type FnFramesyncDestroy = unsafe extern "C" fn(*mut NDIlib_framesync_instance_t);
pub(crate) type FnFramesyncCaptureVideo =
    unsafe extern "C" fn(*mut NDIlib_framesync_instance_t, *mut NDIlib_video_frame_v2_recv_t, i32);
pub(crate) type FnFramesyncFreeVideo =
    unsafe extern "C" fn(*mut NDIlib_framesync_instance_t, *mut NDIlib_video_frame_v2_recv_t);
pub(crate) type FnFramesyncCaptureAudio = unsafe extern "C" fn(
    *mut NDIlib_framesync_instance_t,
    *mut NDIlib_audio_frame_v2_t,
    i32,
    i32,
    i32,
);
pub(crate) type FnFramesyncFreeAudio =
    unsafe extern "C" fn(*mut NDIlib_framesync_instance_t, *mut NDIlib_audio_frame_v2_t);
pub(crate) type FnFramesyncAudioQueueDepth =
    unsafe extern "C" fn(*mut NDIlib_framesync_instance_t) -> i32;

/// The receive + FrameSync entry points, resolved together (all or none).
#[derive(Clone, Copy)]
pub(crate) struct RecvFns {
    pub recv_create_v3: FnRecvCreateV3,
    pub recv_destroy: FnRecvDestroy,
    pub recv_get_no_connections: FnRecvGetNoConnections,
    pub framesync_create: FnFramesyncCreate,
    pub framesync_destroy: FnFramesyncDestroy,
    pub framesync_capture_video: FnFramesyncCaptureVideo,
    pub framesync_free_video: FnFramesyncFreeVideo,
    pub framesync_capture_audio: FnFramesyncCaptureAudio,
    pub framesync_free_audio: FnFramesyncFreeAudio,
    pub framesync_audio_queue_depth: FnFramesyncAudioQueueDepth,
}

// ---------------------------------------------------------------------------
// The mockable backend
// ---------------------------------------------------------------------------

/// The receive half of the NDI SDK, behind a trait so Linux tests drive it with
/// `MockNdiReceiveBackend`. Handles are opaque ids (like [`crate::NdiBackend`]);
/// an unknown / destroyed id is a no-op (an empty frame, `0`, nothing freed).
/// Use it through `receiver::NdiFrameSync`, which owns the handles and frees
/// every captured frame.
pub trait NdiReceiveBackend: Send + Sync {
    /// The names (`"MACHINE (stream)"`) of the NDI sources visible now, after
    /// waiting up to `wait_ms` for the network to be discovered.
    fn find_source_names(&self, wait_ms: u32) -> Vec<String>;
    /// `NDIlib_recv_create_v3` for the source named `source_name` (UYVY-fastest,
    /// highest bandwidth, progressive), advertising itself as `recv_name`.
    fn recv_create(&self, source_name: &str, recv_name: &str) -> Result<usize, NdiError>;
    /// `NDIlib_recv_destroy` (after the FrameSync bound to it is destroyed).
    fn recv_destroy(&self, recv: usize);
    /// `NDIlib_recv_get_no_connections`: 1 while the source is connected.
    fn recv_connections(&self, recv: usize) -> i32;
    /// `NDIlib_framesync_create` bound to `recv`.
    fn framesync_create(&self, recv: usize) -> Result<usize, NdiError>;
    /// `NDIlib_framesync_destroy`.
    fn framesync_destroy(&self, fs: usize);
    /// `NDIlib_framesync_capture_video(fs, &frame, progressive)`. The frame's
    /// data stays valid until [`framesync_free_video`](Self::framesync_free_video).
    fn framesync_capture_video(&self, fs: usize) -> NDIlib_video_frame_v2_recv_t;
    /// `NDIlib_framesync_free_video` for a frame this FrameSync returned.
    fn framesync_free_video(&self, fs: usize, frame: &mut NDIlib_video_frame_v2_recv_t);
    /// `NDIlib_framesync_capture_audio(fs, &frame, sample_rate, channels,
    /// samples)`: exactly the requested format, resampled to the caller's
    /// cadence, silence when the source has none.
    fn framesync_capture_audio(
        &self,
        fs: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
    ) -> NDIlib_audio_frame_v2_t;
    /// `NDIlib_framesync_free_audio` for a frame this FrameSync returned.
    fn framesync_free_audio(&self, fs: usize, frame: &mut NDIlib_audio_frame_v2_t);
    /// `NDIlib_framesync_audio_queue_depth`: samples waiting in the FrameSync.
    fn framesync_audio_queue_depth(&self, fs: usize) -> i32;
}

// ---------------------------------------------------------------------------
// The real backend
// ---------------------------------------------------------------------------

/// A raw SDK pointer as a `Send` handle-table entry. Touched only through the
/// SDK, under the table lock.
#[derive(Clone, Copy)]
struct Raw(usize);

#[derive(Default)]
struct Handles {
    next: usize,
    recv: HashMap<usize, Raw>,
    fs: HashMap<usize, Raw>,
}

/// Production [`NdiReceiveBackend`] on the process's one [`NdiLib`] (the same
/// library the senders use — `NDIlib_initialize` runs once per process).
pub struct RealNdiReceiveBackend {
    lib: Arc<NdiLib>,
    fns: RecvFns,
    /// Every SDK call runs under this lock: a destroy can never race a capture
    /// on the same instance (one input thread + the rare config-task find).
    handles: Mutex<Handles>,
}

impl RealNdiReceiveBackend {
    /// The receive backend on an already-loaded SDK, or `None` when the runtime
    /// lacks the receive / FrameSync symbols (logged at load).
    #[cfg_attr(test, mutants::skip)] // needs a loaded NDI runtime (never on Linux CI)
    pub fn new(lib: Arc<NdiLib>) -> Option<Self> {
        let fns = lib.recv?;
        Some(Self {
            lib,
            fns,
            handles: Mutex::new(Handles::default()),
        })
    }

    #[cfg_attr(test, mutants::skip)] // reached only from the FFI methods below
    fn lock(&self) -> std::sync::MutexGuard<'_, Handles> {
        self.handles.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Copy a possibly-null C string into an owned `String` (empty on null).
#[cfg_attr(test, mutants::skip)]
fn c_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the SDK hands out NUL-terminated UTF-8 strings valid until the
    // next call on the same finder; we copy immediately.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// mutants::skip on every method — each one dereferences an NDI SDK function
// pointer that exists only with the real runtime (never on the Linux mutation
// runner). The contract is exercised through `MockNdiReceiveBackend` +
// `receiver::NdiFrameSync` tests; the box acceptance exercises the real SDK.
impl NdiReceiveBackend for RealNdiReceiveBackend {
    #[cfg_attr(test, mutants::skip)]
    fn find_source_names(&self, wait_ms: u32) -> Vec<String> {
        let create = NDIlib_find_create_t {
            show_local_sources: true,
            p_groups: ptr::null(),
            p_extra_ips: ptr::null(),
        };
        // SAFETY: `create` outlives the call; the finder is destroyed below.
        let finder = unsafe { (self.lib.find_create_v2)(&create) };
        if finder.is_null() {
            warn!("ndi input: NDIlib_find_create_v2 returned null — cannot list sources");
            return Vec::new();
        }
        let mut names = Vec::new();
        // SAFETY: valid finder; the returned array is owned by the finder and
        // valid until the next find call / destroy — copied out at once.
        unsafe {
            (self.lib.find_wait_for_sources)(finder, wait_ms);
            let mut count: u32 = 0;
            let arr = (self.lib.find_get_current_sources)(finder, &mut count);
            if !arr.is_null() {
                for i in 0..count as usize {
                    names.push(c_string((*arr.add(i)).p_ndi_name));
                }
            }
            (self.lib.find_destroy)(finder);
        }
        names
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_create(&self, source_name: &str, recv_name: &str) -> Result<usize, NdiError> {
        let src = CString::new(source_name).map_err(|_| NdiError::ReceiveFailed("source name"))?;
        let name = CString::new(recv_name).map_err(|_| NdiError::ReceiveFailed("receiver name"))?;
        let create = NDIlib_recv_create_v3_t {
            source_to_connect_to: NDIlib_source_t {
                p_ndi_name: src.as_ptr(),
                p_url_address: ptr::null(),
            },
            color_format: RECV_COLOR_FORMAT_FASTEST,
            bandwidth: RECV_BANDWIDTH_HIGHEST,
            allow_video_fields: false,
            p_ndi_recv_name: name.as_ptr(),
        };
        let mut h = self.lock();
        // SAFETY: `create` and its strings outlive the call.
        let recv = unsafe { (self.fns.recv_create_v3)(&create) };
        if recv.is_null() {
            return Err(NdiError::ReceiveFailed(
                "NDIlib_recv_create_v3 returned null",
            ));
        }
        h.next += 1;
        let id = h.next;
        h.recv.insert(id, Raw(recv as usize));
        info!(
            source = source_name,
            recv = id,
            "ndi input: receiver created"
        );
        Ok(id)
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_destroy(&self, recv: usize) {
        let mut h = self.lock();
        if let Some(Raw(p)) = h.recv.remove(&recv) {
            // SAFETY: a live receiver, removed from the table so never reused.
            unsafe { (self.fns.recv_destroy)(p as *mut NDIlib_recv_instance_t) };
            debug!(recv, "ndi input: receiver destroyed");
        }
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_connections(&self, recv: usize) -> i32 {
        let h = self.lock();
        match h.recv.get(&recv) {
            // SAFETY: a live receiver (the table lock keeps it alive).
            Some(Raw(p)) => unsafe {
                (self.fns.recv_get_no_connections)(*p as *mut NDIlib_recv_instance_t)
            },
            None => 0,
        }
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_create(&self, recv: usize) -> Result<usize, NdiError> {
        let mut h = self.lock();
        let Some(Raw(r)) = h.recv.get(&recv).copied() else {
            return Err(NdiError::ReceiveFailed("unknown receiver"));
        };
        // SAFETY: a live receiver.
        let fs = unsafe { (self.fns.framesync_create)(r as *mut NDIlib_recv_instance_t) };
        if fs.is_null() {
            return Err(NdiError::ReceiveFailed(
                "NDIlib_framesync_create returned null",
            ));
        }
        h.next += 1;
        let id = h.next;
        h.fs.insert(id, Raw(fs as usize));
        Ok(id)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_destroy(&self, fs: usize) {
        let mut h = self.lock();
        if let Some(Raw(p)) = h.fs.remove(&fs) {
            // SAFETY: a live FrameSync, removed from the table.
            unsafe { (self.fns.framesync_destroy)(p as *mut NDIlib_framesync_instance_t) };
        }
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_capture_video(&self, fs: usize) -> NDIlib_video_frame_v2_recv_t {
        let mut frame = NDIlib_video_frame_v2_recv_t::empty();
        let h = self.lock();
        if let Some(Raw(p)) = h.fs.get(&fs) {
            // SAFETY: a live FrameSync; `frame` is a valid out-pointer.
            unsafe {
                (self.fns.framesync_capture_video)(
                    *p as *mut NDIlib_framesync_instance_t,
                    &mut frame,
                    FRAME_FORMAT_TYPE_PROGRESSIVE,
                )
            };
        }
        frame
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_video(&self, fs: usize, frame: &mut NDIlib_video_frame_v2_recv_t) {
        let h = self.lock();
        if let Some(Raw(p)) = h.fs.get(&fs) {
            // SAFETY: `frame` was returned by this FrameSync and not freed yet.
            unsafe {
                (self.fns.framesync_free_video)(*p as *mut NDIlib_framesync_instance_t, frame)
            };
        }
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_capture_audio(
        &self,
        fs: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
    ) -> NDIlib_audio_frame_v2_t {
        let mut frame = NDIlib_audio_frame_v2_t::empty();
        let h = self.lock();
        if let Some(Raw(p)) = h.fs.get(&fs) {
            // SAFETY: a live FrameSync; `frame` is a valid out-pointer.
            unsafe {
                (self.fns.framesync_capture_audio)(
                    *p as *mut NDIlib_framesync_instance_t,
                    &mut frame,
                    sample_rate,
                    channels,
                    samples,
                )
            };
        }
        frame
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_audio(&self, fs: usize, frame: &mut NDIlib_audio_frame_v2_t) {
        let h = self.lock();
        if let Some(Raw(p)) = h.fs.get(&fs) {
            // SAFETY: `frame` was returned by this FrameSync and not freed yet.
            unsafe {
                (self.fns.framesync_free_audio)(*p as *mut NDIlib_framesync_instance_t, frame)
            };
        }
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_audio_queue_depth(&self, fs: usize) -> i32 {
        let h = self.lock();
        match h.fs.get(&fs) {
            // SAFETY: a live FrameSync.
            Some(Raw(p)) => unsafe {
                (self.fns.framesync_audio_queue_depth)(*p as *mut NDIlib_framesync_instance_t)
            },
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    use crate::types::NDIlib_video_frame_v2_t;

    #[test]
    fn fourcc_constants_spell_their_ascii_names() {
        assert_eq!(&FOURCC_UYVY.to_le_bytes(), b"UYVY");
        assert_eq!(&FOURCC_UYVA.to_le_bytes(), b"UYVA");
    }

    #[test]
    fn recv_constants_match_the_sdk_enum_values() {
        assert_eq!(RECV_COLOR_FORMAT_FASTEST, 100);
        assert_eq!(RECV_BANDWIDTH_HIGHEST, 100);
        assert_eq!(FRAME_FORMAT_TYPE_PROGRESSIVE, 1);
    }

    /// The received video frame is byte-for-byte the send-side
    /// `NDIlib_video_frame_v2_t` (only the FourCC's Rust type differs).
    #[test]
    fn received_video_frame_has_the_send_side_layout() {
        type S = NDIlib_video_frame_v2_t;
        type R = NDIlib_video_frame_v2_recv_t;
        assert_eq!(size_of::<R>(), size_of::<S>());
        assert_eq!(offset_of!(R, four_cc), offset_of!(S, four_cc));
        assert_eq!(
            offset_of!(R, frame_format_type),
            offset_of!(S, frame_format_type)
        );
        assert_eq!(offset_of!(R, timecode), offset_of!(S, timecode));
        assert_eq!(offset_of!(R, p_data), offset_of!(S, p_data));
        assert_eq!(
            offset_of!(R, line_stride_in_bytes),
            offset_of!(S, line_stride_in_bytes)
        );
        assert_eq!(offset_of!(R, p_metadata), offset_of!(S, p_metadata));
        assert_eq!(offset_of!(R, timestamp), offset_of!(S, timestamp));
    }

    /// The C layouts on the 64-bit targets we ship (Windows x64, Linux CI).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn c_layouts_on_64_bit() {
        type V = NDIlib_video_frame_v2_recv_t;
        assert_eq!(offset_of!(V, timecode), 32);
        assert_eq!(offset_of!(V, p_data), 40);
        assert_eq!(offset_of!(V, line_stride_in_bytes), 48);
        assert_eq!(size_of::<V>(), 72);

        type A = NDIlib_audio_frame_v2_t;
        assert_eq!(offset_of!(A, no_samples), 8);
        assert_eq!(offset_of!(A, timecode), 16);
        assert_eq!(offset_of!(A, p_data), 24);
        assert_eq!(offset_of!(A, channel_stride_in_bytes), 32);
        assert_eq!(offset_of!(A, p_metadata), 40);
        assert_eq!(offset_of!(A, timestamp), 48);
        assert_eq!(size_of::<A>(), 56);

        type C = NDIlib_recv_create_v3_t;
        assert_eq!(offset_of!(C, color_format), 16);
        assert_eq!(offset_of!(C, bandwidth), 20);
        assert_eq!(offset_of!(C, allow_video_fields), 24);
        assert_eq!(offset_of!(C, p_ndi_recv_name), 32);
        assert_eq!(size_of::<C>(), 40);
    }

    #[test]
    fn empty_frames_are_all_zero() {
        let v = NDIlib_video_frame_v2_recv_t::empty();
        assert_eq!(
            (v.xres, v.yres, v.four_cc, v.line_stride_in_bytes),
            (0, 0, 0, 0)
        );
        assert_eq!(
            (v.frame_rate_n, v.frame_rate_d, v.frame_format_type),
            (0, 0, 0)
        );
        assert_eq!((v.timecode, v.timestamp), (0, 0));
        assert_eq!(v.picture_aspect_ratio, 0.0);
        assert!(v.p_data.is_null() && v.p_metadata.is_null());
        let a = NDIlib_audio_frame_v2_t::empty();
        assert_eq!(
            (
                a.sample_rate,
                a.no_channels,
                a.no_samples,
                a.channel_stride_in_bytes
            ),
            (0, 0, 0, 0)
        );
        assert_eq!((a.timecode, a.timestamp), (0, 0));
        assert!(a.p_data.is_null() && a.p_metadata.is_null());
    }
}
