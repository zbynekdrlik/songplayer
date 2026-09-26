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

use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{debug, info, warn};

use crate::error::NdiError;
use crate::handle_table::HandleTable;
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
/// `NDIlib_frame_format_type_interleaved`: a whole frame, field 0 on the even
/// lines and field 1 on the odd ones (full height).
pub const FRAME_FORMAT_TYPE_INTERLEAVED: i32 = 0;
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
///
/// Calls on DIFFERENT handles must never wait for each other: the NDI input
/// creates and destroys its receivers on helper threads while its grid thread
/// captures from the live one (#212 follow-up), so a slow destroy of the old
/// pair must not hold up a capture of the new one.
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
/// SDK, under its own handle's lock.
#[derive(Clone, Copy)]
struct Raw(usize);

/// The SDK's receiver + FrameSync calls, with ONE lock PER receiver and PER
/// FrameSync (the #147 round 11 `HandleTable`). Every call holds its own
/// instance's lock only: a destroy still waits for a call in flight on the
/// SAME instance, while a slow create / destroy (~0.5 s on the box, run on the
/// input's helper threads) never holds up a call on ANOTHER instance (#212
/// follow-up — one lock across every SDK call made the grid thread's capture
/// of the new pair wait for the old pair's destroy). Split from
/// [`RealNdiReceiveBackend`] so the lock scope is tested over fake SDK
/// functions (`tests::calls_on_one_instance_never_wait_for_a_slow_call_on_another`).
struct RecvHandles {
    fns: RecvFns,
    /// Handle ids, shared by both tables (a receiver and a FrameSync never
    /// get the same id).
    next: AtomicUsize,
    recv: HandleTable<Raw>,
    fs: HandleTable<Raw>,
}

/// Production [`NdiReceiveBackend`] on the process's one [`NdiLib`] (the same
/// library the senders use — `NDIlib_initialize` runs once per process).
pub struct RealNdiReceiveBackend {
    /// Keeps the library loaded (the `fns` point into it) and serves `find`.
    lib: Arc<NdiLib>,
    handles: RecvHandles,
}

impl RealNdiReceiveBackend {
    /// The receive backend on an already-loaded SDK, or `None` when the runtime
    /// lacks the receive / FrameSync symbols (logged at load).
    #[cfg_attr(test, mutants::skip)] // needs a loaded NDI runtime (never on Linux CI)
    pub fn new(lib: Arc<NdiLib>) -> Option<Self> {
        let fns = lib.recv?;
        Some(Self {
            lib,
            handles: RecvHandles::new(fns),
        })
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

// mutants::skip on every method: `find_source_names` dereferences the loaded
// library's find functions, and the rest delegate to `RecvHandles`. None is
// reachable without a loaded NDI runtime, because `RealNdiReceiveBackend::new`
// needs one (never on the Linux mutation runner). The contract is exercised
// through `MockNdiReceiveBackend` + `receiver::NdiFrameSync` tests; the box
// acceptance exercises the real SDK.
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
        self.handles.recv_create(source_name, recv_name)
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_destroy(&self, recv: usize) {
        self.handles.recv_destroy(recv);
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_connections(&self, recv: usize) -> i32 {
        self.handles.recv_connections(recv)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_create(&self, recv: usize) -> Result<usize, NdiError> {
        self.handles.framesync_create(recv)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_destroy(&self, fs: usize) {
        self.handles.framesync_destroy(fs);
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_capture_video(&self, fs: usize) -> NDIlib_video_frame_v2_recv_t {
        self.handles.framesync_capture_video(fs)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_video(&self, fs: usize, frame: &mut NDIlib_video_frame_v2_recv_t) {
        self.handles.framesync_free_video(fs, frame);
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_capture_audio(
        &self,
        fs: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
    ) -> NDIlib_audio_frame_v2_t {
        self.handles
            .framesync_capture_audio(fs, sample_rate, channels, samples)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_audio(&self, fs: usize, frame: &mut NDIlib_audio_frame_v2_t) {
        self.handles.framesync_free_audio(fs, frame);
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_audio_queue_depth(&self, fs: usize) -> i32 {
        self.handles.framesync_audio_queue_depth(fs)
    }
}

// mutants::skip on every method: they are thin pass-throughs to the SDK
// function pointers. The fake-SDK test below pins their LOCK SCOPE, not every
// value passed through; e.g. `recv_connections -> 1` would survive a fake
// that returns 1.
impl RecvHandles {
    #[cfg_attr(test, mutants::skip)]
    fn new(fns: RecvFns) -> Self {
        Self {
            fns,
            next: AtomicUsize::new(0),
            recv: HandleTable::new(),
            fs: HandleTable::new(),
        }
    }

    #[cfg_attr(test, mutants::skip)] // reached only from the SDK methods below
    fn next_id(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
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
        // SAFETY: `create` and its strings outlive the call. No lock: the new
        // instance is not shared with anyone until it is inserted below.
        let recv = unsafe { (self.fns.recv_create_v3)(&create) };
        if recv.is_null() {
            return Err(NdiError::ReceiveFailed(
                "NDIlib_recv_create_v3 returned null",
            ));
        }
        let id = self.next_id();
        self.recv.insert(id, Raw(recv as usize));
        info!(
            source = source_name,
            recv = id,
            "ndi input: receiver created"
        );
        Ok(id)
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_destroy(&self, recv: usize) {
        self.recv.remove_with(recv, |Raw(p)| {
            // SAFETY: a live receiver, taken out of its slot (never reused)
            // under that slot's lock, which waits for a call in flight.
            unsafe { (self.fns.recv_destroy)(p as *mut NDIlib_recv_instance_t) };
            debug!(recv, "ndi input: receiver destroyed");
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn recv_connections(&self, recv: usize) -> i32 {
        self.recv
            .with(recv, |Raw(p)| {
                // SAFETY: a live receiver (its slot lock keeps it alive).
                unsafe { (self.fns.recv_get_no_connections)(*p as *mut NDIlib_recv_instance_t) }
            })
            .unwrap_or(0)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_create(&self, recv: usize) -> Result<usize, NdiError> {
        let fs = self
            .recv
            .with(recv, |Raw(r)| {
                // SAFETY: a live receiver (its slot lock keeps it alive).
                unsafe { (self.fns.framesync_create)(*r as *mut NDIlib_recv_instance_t) }
            })
            .ok_or(NdiError::ReceiveFailed("unknown receiver"))?;
        if fs.is_null() {
            return Err(NdiError::ReceiveFailed(
                "NDIlib_framesync_create returned null",
            ));
        }
        let id = self.next_id();
        self.fs.insert(id, Raw(fs as usize));
        Ok(id)
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_destroy(&self, fs: usize) {
        self.fs.remove_with(fs, |Raw(p)| {
            // SAFETY: a live FrameSync, taken out of its slot (never reused)
            // under that slot's lock, which waits for a capture in flight.
            unsafe { (self.fns.framesync_destroy)(p as *mut NDIlib_framesync_instance_t) };
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_capture_video(&self, fs: usize) -> NDIlib_video_frame_v2_recv_t {
        let mut frame = NDIlib_video_frame_v2_recv_t::empty();
        self.fs.with(fs, |Raw(p)| {
            // SAFETY: a live FrameSync; `frame` is a valid out-pointer.
            unsafe {
                (self.fns.framesync_capture_video)(
                    *p as *mut NDIlib_framesync_instance_t,
                    &mut frame,
                    FRAME_FORMAT_TYPE_PROGRESSIVE,
                )
            };
        });
        frame
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_video(&self, fs: usize, frame: &mut NDIlib_video_frame_v2_recv_t) {
        self.fs.with(fs, |Raw(p)| {
            // SAFETY: `frame` was returned by this FrameSync and not freed yet.
            unsafe {
                (self.fns.framesync_free_video)(*p as *mut NDIlib_framesync_instance_t, frame)
            };
        });
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
        self.fs.with(fs, |Raw(p)| {
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
        });
        frame
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_free_audio(&self, fs: usize, frame: &mut NDIlib_audio_frame_v2_t) {
        self.fs.with(fs, |Raw(p)| {
            // SAFETY: `frame` was returned by this FrameSync and not freed yet.
            unsafe {
                (self.fns.framesync_free_audio)(*p as *mut NDIlib_framesync_instance_t, frame)
            };
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn framesync_audio_queue_depth(&self, fs: usize) -> i32 {
        self.fs
            .with(fs, |Raw(p)| {
                // SAFETY: a live FrameSync (its slot lock keeps it alive).
                unsafe {
                    (self.fns.framesync_audio_queue_depth)(*p as *mut NDIlib_framesync_instance_t)
                }
            })
            .unwrap_or(0)
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
        assert_eq!(FRAME_FORMAT_TYPE_INTERLEAVED, 0);
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

    // --- the lock scope, over fake SDK functions (#212 follow-up) -------------

    use std::sync::mpsc;
    use std::sync::{Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    /// The first fake receiver / FrameSync: its destroy / video capture blocks
    /// while the gate holds it. Later ones get addresses 16 apart.
    const SLOW_RECV: usize = 0x1000;
    const SLOW_FS: usize = 0x2000;
    static RECVS: AtomicUsize = AtomicUsize::new(0);
    static SYNCS: AtomicUsize = AtomicUsize::new(0);
    /// `(slow receiver destroy held, slow FrameSync capture held)`.
    static GATE: Mutex<(bool, bool)> = Mutex::new((false, false));
    static RELEASED: Condvar = Condvar::new();
    /// The held calls that were entered.
    static ENTERED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    fn set_gate(destroy: bool, capture: bool) {
        *GATE.lock().unwrap() = (destroy, capture);
        RELEASED.notify_all();
    }

    /// Record `call` as entered, then wait while `held` says so.
    fn enter(call: &'static str, held: fn(&(bool, bool)) -> bool) {
        ENTERED.lock().unwrap().push(call);
        let gate = GATE.lock().unwrap();
        let _released = RELEASED.wait_while(gate, |g| held(g)).unwrap();
    }

    fn wait_entered(call: &'static str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ENTERED.lock().unwrap().contains(&call) {
            assert!(Instant::now() < deadline, "{call} never started");
            thread::sleep(Duration::from_millis(1));
        }
    }

    unsafe extern "C" fn fake_recv_create(
        _: *const NDIlib_recv_create_v3_t,
    ) -> *mut NDIlib_recv_instance_t {
        (SLOW_RECV + 16 * RECVS.fetch_add(1, Ordering::SeqCst)) as *mut NDIlib_recv_instance_t
    }

    unsafe extern "C" fn fake_recv_destroy(recv: *mut NDIlib_recv_instance_t) {
        if recv as usize == SLOW_RECV {
            enter("recv_destroy", |g| g.0);
        }
    }

    unsafe extern "C" fn fake_recv_connections(_: *mut NDIlib_recv_instance_t) -> i32 {
        1
    }

    unsafe extern "C" fn fake_framesync_create(
        _: *mut NDIlib_recv_instance_t,
    ) -> *mut NDIlib_framesync_instance_t {
        (SLOW_FS + 16 * SYNCS.fetch_add(1, Ordering::SeqCst)) as *mut NDIlib_framesync_instance_t
    }

    unsafe extern "C" fn fake_framesync_destroy(_: *mut NDIlib_framesync_instance_t) {}

    unsafe extern "C" fn fake_capture_video(
        fs: *mut NDIlib_framesync_instance_t,
        _: *mut NDIlib_video_frame_v2_recv_t,
        _: i32,
    ) {
        if fs as usize == SLOW_FS {
            enter("capture_video", |g| g.1);
        }
    }

    unsafe extern "C" fn fake_free_video(
        _: *mut NDIlib_framesync_instance_t,
        _: *mut NDIlib_video_frame_v2_recv_t,
    ) {
    }

    unsafe extern "C" fn fake_capture_audio(
        _: *mut NDIlib_framesync_instance_t,
        _: *mut NDIlib_audio_frame_v2_t,
        _: i32,
        _: i32,
        _: i32,
    ) {
    }

    unsafe extern "C" fn fake_free_audio(
        _: *mut NDIlib_framesync_instance_t,
        _: *mut NDIlib_audio_frame_v2_t,
    ) {
    }

    unsafe extern "C" fn fake_queue_depth(_: *mut NDIlib_framesync_instance_t) -> i32 {
        7
    }

    fn fake_fns() -> RecvFns {
        RecvFns {
            recv_create_v3: fake_recv_create,
            recv_destroy: fake_recv_destroy,
            recv_get_no_connections: fake_recv_connections,
            framesync_create: fake_framesync_create,
            framesync_destroy: fake_framesync_destroy,
            framesync_capture_video: fake_capture_video,
            framesync_free_video: fake_free_video,
            framesync_capture_audio: fake_capture_audio,
            framesync_free_audio: fake_free_audio,
            framesync_audio_queue_depth: fake_queue_depth,
        }
    }

    /// Run every call a grid boundary makes on the pair `(recv, fs)` on its
    /// own thread; the answer arrives only once all of them returned.
    fn boundary_calls(h: &Arc<RecvHandles>, recv: usize, fs: usize) -> mpsc::Receiver<(i32, i32)> {
        let (tx, rx) = mpsc::channel();
        let h = Arc::clone(h);
        thread::spawn(move || {
            let mut video = h.framesync_capture_video(fs);
            h.framesync_free_video(fs, &mut video);
            let mut audio = h.framesync_capture_audio(fs, 48_000, 2, 1_600);
            h.framesync_free_audio(fs, &mut audio);
            tx.send((h.recv_connections(recv), h.framesync_audio_queue_depth(fs)))
                .unwrap();
        });
        rx
    }

    #[test]
    fn calls_on_one_instance_never_wait_for_a_slow_call_on_another() {
        // #212 follow-up (review round 2): the input's close helper destroys the
        // old pair (~0.5 s on the box) while its grid thread captures from the
        // new one. One lock across every SDK call made that capture wait.
        let h = Arc::new(RecvHandles::new(fake_fns()));
        let old = h.recv_create("A (a)", "SP-input").unwrap();
        let old_fs = h.framesync_create(old).unwrap();
        let new = h.recv_create("B (b)", "SP-input").unwrap();
        let new_fs = h.framesync_create(new).unwrap();
        assert_eq!((old, old_fs, new, new_fs), (1, 2, 3, 4));

        // A capture held on the old FrameSync: its destroy waits for it (the
        // SAME instance), the new pair's calls go straight through.
        set_gate(false, true);
        let capture = boundary_calls(&h, old, old_fs);
        wait_entered("capture_video");
        let (destroyed_tx, destroyed_rx) = mpsc::channel();
        let closer = Arc::clone(&h);
        thread::spawn(move || {
            closer.framesync_destroy(old_fs);
            destroyed_tx.send(()).unwrap();
        });
        let live = boundary_calls(&h, new, new_fs);
        assert_eq!(
            live.recv_timeout(Duration::from_secs(10))
                .expect("the new pair waited for the old FrameSync's capture"),
            (1, 7)
        );
        assert!(
            destroyed_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "the FrameSync destroy waits for the capture in flight on it"
        );
        set_gate(false, false);
        capture
            .recv_timeout(Duration::from_secs(10))
            .expect("the held capture finishes once released");
        destroyed_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the destroy follows the capture");

        // The old receiver's destroy held inside the SDK: the new pair's calls
        // still go straight through.
        set_gate(true, false);
        let (gone_tx, gone_rx) = mpsc::channel();
        let closer = Arc::clone(&h);
        thread::spawn(move || {
            closer.recv_destroy(old);
            gone_tx.send(()).unwrap();
        });
        wait_entered("recv_destroy");
        let live = boundary_calls(&h, new, new_fs);
        assert_eq!(
            live.recv_timeout(Duration::from_secs(10))
                .expect("the new pair waited for the old receiver's destroy"),
            (1, 7)
        );
        assert!(
            gone_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the old receiver's destroy is still held"
        );
        set_gate(false, false);
        gone_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("released");
        // The destroyed instances are gone: their calls are no-ops now.
        assert_eq!(h.recv_connections(old), 0);
        assert_eq!(h.framesync_audio_queue_depth(old_fs), 0);
    }
}
