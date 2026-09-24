//! Production [`RealNdiBackend`] — the real NDI SDK backend.
//!
//! Split out of `sender.rs` (which reached the airuleset 1000-line file-size
//! cap, #147) into a `real`/`mock` sibling pair. Pure move: no behaviour
//! change. Re-exported from `sender.rs` (`pub use crate::sender_real::RealNdiBackend`)
//! and from the crate root (`sp_ndi::RealNdiBackend`), so every external path
//! still resolves.

use std::ffi::CString;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{debug, info};

use crate::deinterleave::deinterleave;
use crate::error::NdiError;
use crate::handle_table::HandleTable;
use crate::ndi_sdk::NdiLib;
use crate::sender::NdiBackend;
use crate::types::{
    FRAME_FORMAT_PROGRESSIVE, FourCCAudioType, FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE,
    NDIlib_audio_frame_v3_t, NDIlib_send_create_t, NDIlib_send_instance_t, NDIlib_tally_t,
    NDIlib_video_frame_v2_t,
};

/// Per-handle state kept by the real backend.
struct RealHandleState {
    ptr: *mut NDIlib_send_instance_t,
    /// Planar audio scratch buffer — reused to avoid per-frame allocation.
    audio_scratch: Vec<f32>,
}

// SAFETY: the raw NDI pointer is only touched through NDI SDK calls, and
// only while this handle's own lock in `HandleTable` is held, so no two
// threads ever call the SDK on the same sender at once (the SDK is
// thread-safe across DIFFERENT sender instances). `send_destroy` runs under
// that same lock after the state was taken out of its slot, so no call can
// reach the pointer after it. The scratch Vec is a plain owned buffer.
unsafe impl Send for RealHandleState {}

/// Production [`NdiBackend`] backed by the real NDI SDK via [`NdiLib`].
pub struct RealNdiBackend {
    lib: Arc<NdiLib>,
    next_id: AtomicUsize,
    /// One lock PER sender (#147 round 11): a blocking SDK call on one output
    /// never delays another output's send. See `handle_table.rs`.
    handles: HandleTable<RealHandleState>,
}

unsafe impl Send for RealNdiBackend {}
unsafe impl Sync for RealNdiBackend {}

impl RealNdiBackend {
    /// Create a new backend from an already-loaded NDI SDK.
    pub fn new(lib: Arc<NdiLib>) -> Self {
        Self {
            lib,
            next_id: AtomicUsize::new(1),
            handles: HandleTable::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_video_frame(
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: *const u8,
        timecode_100ns: Option<i64>,
    ) -> NDIlib_video_frame_v2_t {
        NDIlib_video_frame_v2_t {
            xres: width,
            yres: height,
            four_cc,
            frame_rate_n,
            frame_rate_d,
            picture_aspect_ratio: 0.0,
            frame_format_type: FRAME_FORMAT_PROGRESSIVE,
            // Genlock: real frames carry the floored wall-clock boundary
            // (camera-box#1294 §4). `None` keeps SYNTHESIZE for the standby
            // black frame only. `timestamp` stays 0.
            timecode: timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE),
            p_data: data,
            line_stride_in_bytes: stride,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

impl NdiBackend for RealNdiBackend {
    // cargo-mutants: skip — these methods dereference NDI SDK function pointers
    // that are only loaded when the real NDI runtime is installed. On the Linux
    // mutation runner the calls cannot be exercised, so mutants would survive
    // without observable behaviour. The NdiSender + NdiBackend contract is tested
    // via MockNdiBackend which the mutation runner handles correctly.
    #[cfg_attr(test, mutants::skip)]
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError> {
        let c_name = CString::new(name).map_err(|_| NdiError::InitFailed)?;

        let create_desc = NDIlib_send_create_t {
            p_ndi_name: c_name.as_ptr(),
            p_groups: ptr::null(),
            clock_video,
            clock_audio,
        };

        let ptr = unsafe { (self.lib.send_create)(&create_desc) };
        if ptr.is_null() {
            return Err(NdiError::InitFailed);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.handles.insert(
            id,
            RealHandleState {
                ptr,
                audio_scratch: Vec::new(),
            },
        );
        info!(
            "Created NDI sender '{name}' handle={id} clock_video={clock_video} clock_audio={clock_audio}"
        );
        Ok(id)
    }

    #[cfg_attr(test, mutants::skip)]
    fn send_destroy(&self, handle: usize) {
        // Waits for a send in flight on this handle, then destroys the sender
        // while still holding its lock. A send that looked the handle up
        // before the remove finds an empty slot and does nothing.
        self.handles.remove_with(handle, |state| {
            debug!("Destroying NDI sender handle {handle}");
            unsafe {
                (self.lib.send_destroy)(state.ptr);
            }
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn send_video(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        let frame = Self::build_video_frame(
            four_cc,
            width,
            height,
            stride,
            frame_rate_n,
            frame_rate_d,
            data.as_ptr(),
            timecode_100ns,
        );
        self.handles.with(handle, |state| unsafe {
            (self.lib.send_send_video_v2)(state.ptr, &frame);
        });
    }

    #[cfg_attr(test, mutants::skip)]
    unsafe fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        let frame = Self::build_video_frame(
            four_cc,
            width,
            height,
            stride,
            frame_rate_n,
            frame_rate_d,
            data.as_ptr(),
            timecode_100ns,
        );
        // Blocks until the SDK has finished with this sender's previous frame
        // — while holding only THIS sender's lock.
        self.handles.with(handle, |state| unsafe {
            (self.lib.send_send_video_async_v2)(state.ptr, &frame);
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn send_video_flush(&self, handle: usize) {
        self.handles.with(handle, |state| unsafe {
            (self.lib.send_send_video_async_v2)(state.ptr, ptr::null());
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
        timecode_100ns: Option<i64>,
    ) {
        if channels <= 0 || samples_per_channel <= 0 || interleaved.is_empty() {
            return;
        }
        self.handles.with(handle, |state| {
            // Deinterleave into the per-sender scratch buffer.
            deinterleave(interleaved, channels as usize, &mut state.audio_scratch);

            let frame = NDIlib_audio_frame_v3_t {
                sample_rate,
                no_channels: channels,
                no_samples: samples_per_channel,
                // Genlock: raw wall clock at submission (camera-box#1294 §6),
                // SYNTHESIZE only when unstamped. `timestamp` stays 0.
                timecode: timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE),
                four_cc: FourCCAudioType::FLTP,
                p_data: state.audio_scratch.as_ptr(),
                channel_stride_in_bytes: samples_per_channel * std::mem::size_of::<f32>() as i32,
                p_metadata: ptr::null(),
                timestamp: 0,
            };

            unsafe {
                (self.lib.send_send_audio_v3)(state.ptr, &frame);
            }
        });
    }

    #[cfg_attr(test, mutants::skip)]
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        self.handles
            .with(handle, |state| {
                let mut tally = NDIlib_tally_t::default();
                let changed =
                    unsafe { (self.lib.send_get_tally)(state.ptr, &mut tally, timeout_ms) };
                if changed {
                    Some((tally.on_program, tally.on_preview))
                } else {
                    None
                }
            })
            .flatten()
    }

    // mutants::skip — dereferences NDI SDK function pointer; only exercised on
    // real Windows runtime. Behaviour is verified through MockNdiBackend.
    #[cfg_attr(test, mutants::skip)]
    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32 {
        self.handles
            .with(handle, |state| unsafe {
                (self.lib.send_get_no_connections)(state.ptr, timeout_ms)
            })
            .unwrap_or(-1)
    }

    // mutants::skip — dereferences NDI SDK function pointer; only exercised on
    // the real Windows runtime. The `host:port` parsing it delegates to
    // (`crate::source_url::parse_source_url`) is unit-tested on Linux.
    #[cfg_attr(test, mutants::skip)]
    fn send_get_source_url(&self, handle: usize) -> Option<String> {
        self.handles
            .with(handle, |state| {
                // SAFETY: `send_get_source_name` returns a pointer to a source
                // descriptor owned by the SDK, valid until the next NDI call on
                // this sender. We copy the URL string out immediately, before
                // releasing this sender's lock (which keeps every other call on
                // this sender out), and never retain the pointer.
                let src_ptr = unsafe { (self.lib.send_get_source_name)(state.ptr) };
                if src_ptr.is_null() {
                    return None;
                }
                let url_ptr = unsafe { (*src_ptr).p_url_address };
                if url_ptr.is_null() {
                    return None;
                }
                let raw = unsafe { std::ffi::CStr::from_ptr(url_ptr) }.to_string_lossy();
                crate::source_url::parse_source_url(&raw)
            })
            .flatten()
    }

    // mutants::skip — opens/polls/destroys a real NDI finder over the network;
    // not exercisable on the Linux mutation runner. The pure name→URL matching
    // it feeds (`crate::find::{source_matches,match_source_urls}`) is unit-tested.
    #[cfg_attr(test, mutants::skip)]
    fn discover_local_sources(
        &self,
        want_names: &[String],
        overall_timeout_ms: u32,
    ) -> Vec<(String, String)> {
        use std::time::{Duration, Instant};

        let create = crate::types::NDIlib_find_create_t {
            show_local_sources: true,
            p_groups: ptr::null(),
            p_extra_ips: ptr::null(),
        };
        // SAFETY: `create` lives across the call; the returned finder is owned
        // by us and destroyed below.
        let finder = unsafe { (self.lib.find_create_v2)(&create) };
        if finder.is_null() {
            tracing::warn!(
                "ndi: NDIlib_find_create_v2 returned null — cannot discover sender URLs"
            );
            return Vec::new();
        }

        let deadline = Instant::now() + Duration::from_millis(overall_timeout_ms as u64);
        let mut discovered: Vec<(String, String)> = Vec::new();
        loop {
            let slice_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(250) as u32;
            // Block up to `slice_ms` for a source-list change (bounded by the
            // deadline). SAFETY: valid finder handle.
            unsafe {
                (self.lib.find_wait_for_sources)(finder, slice_ms);
            }
            let mut count: u32 = 0;
            // SAFETY: valid finder; the returned array is owned by the finder
            // and valid until the next find call / destroy — we copy each
            // string out immediately below.
            let arr = unsafe { (self.lib.find_get_current_sources)(finder, &mut count) };
            discovered.clear();
            if !arr.is_null() {
                for i in 0..count as isize {
                    let src = unsafe { &*arr.offset(i) };
                    let name = ptr_to_string(src.p_ndi_name);
                    let url = ptr_to_string(src.p_url_address);
                    discovered.push((name, url));
                }
            }
            let all_present = want_names.iter().all(|n| {
                discovered
                    .iter()
                    .any(|(dn, _)| crate::find::source_matches(dn, n))
            });
            if all_present || Instant::now() >= deadline {
                break;
            }
        }
        // SAFETY: valid finder handle, not used after this.
        unsafe {
            (self.lib.find_destroy)(finder);
        }
        discovered
    }
}

/// Copy a possibly-null C string pointer into an owned `String` (empty on null).
///
/// # Safety
/// `p`, if non-null, must point to a valid NUL-terminated C string owned by the
/// caller for the duration of this call.
///
/// mutants::skip — reachable only from the `mutants::skip` finder driver
/// (`discover_local_sources`), so no Linux unit test exercises it; a mutant
/// here would be a structural MISSED, not a real gap (FFI-glue class).
#[cfg_attr(test, mutants::skip)]
fn ptr_to_string(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}
