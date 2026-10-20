//! The GPU compositor of `SP-program-MAX` (#223, revision 3 R3-2, slice S1a).
//!
//! `SP-program-MAX` is a fixed 3840×2160 canvas, the LED wall's feed. #267:
//! every picture fills its whole width ([`MAX_FIT`],
//! `sp_core::fit::fill_width`: a picture narrower than 16:9 is cropped top
//! and bottom, never pillarboxed; a wider one is letterboxed as by
//! `sp_core::fit::aspect_fit`, the rule `SP-program`'s 1920×1080 canvas and
//! the preview use), and a fade blends two pictures of any sizes. On the CPU
//! a 4K fade costs ~40–45 ms per boundary, over the 33.3 ms slot (revision
//! 3), so it is drawn on the GPU:
//!
//! - one Direct3D 11 device on the largest non-software adapter
//!   ([`pick_adapter`]: the RTX 3070 Ti, never the box's virtual display
//!   adapters or the Basic Render Driver), or on WARP for tests and CI;
//! - each NV12 picture is uploaded as two textures, its Y plane as R8 and its
//!   interleaved UV plane as R8G8, honouring the stride; a picture whose id
//!   is already resident on its side is not uploaded again ([`upload_for`]);
//! - the render target is cleared to black, then one textured quad per
//!   picture is drawn into its place in the target, showing the part of the
//!   picture its [`Fit`] keeps ([`uv_rect`]): a bilinear sampler (the
//!   scale), a pixel shader for BT.709 limited range → full-range RGB
//!   ([`BT709_LIMITED_TO_FULL`]), and additive blending at the picture's
//!   weight — the outgoing side at 1 − w first, then the incoming one at w
//!   ([`Composition::layers`]);
//! - the render target is `B8G8R8A8_UNORM`, created `D3D11_RESOURCE_MISC_SHARED`
//!   (not keyed: the desc Spout2's own sender texture has).
//!
//! [`SpoutSender`] (#223 S1b) shares that render target with Resolume Arena
//! under the name [`SPOUT_SENDER_NAME`] (`SP-program-MAX`; Arena lists it as
//! `SPOUT_SP-program-MAX`), through the vendored Spout2 SDK 2.007.017
//! (`vendor/spout2`, SpoutDX `SendTexture`, built by `build.rs`).
//! [`spout_sender_names`] and [`spout_sender_info`] read Spout's registry the
//! way a receiver does; on Windows the doc-hidden `read_shared_texture`
//! reads a sender's shared texture on a second WARP device, for tests
//! (#223 S2).
//!
//! Every decision is a pure function here, tested on Linux (adapter choice,
//! picture validation, layers, quad constants, upload residency, the colour
//! matrix, Spout's name rule and registry formats, the sender's
//! registration in [`spout_state`]). The Windows module
//! (`win/`) only calls Direct3D, Win32 and the Spout shim. [`reference`] is
//! the CPU model of the GPU's output that the WARP pixel pins
//! (`tests/warp.rs`) compare against; `tests/spout.rs` proves the sender on
//! WARP. Off Windows, [`Compositor`] cannot be built: it reports
//! [`GpuError::Unsupported`].
//!
//! #223 S3b: on Windows, `VideoDevice` is the Direct3D 11 device Media
//! Foundation decodes video on (sp-decoder's hardware decode): the same
//! adapter rule, the video API, multithread-protected
//! (`tests/video_device.rs`).
//!
//! #239: the FHD program goes out over Spout too, as `SP-program`
//! ([`SPOUT_FHD_SENDER_NAME`], `SpoutSender::new_fhd`), into a render
//! target of [`FHD_WIDTH`] × [`FHD_HEIGHT`] (the layers, the quads and the
//! CPU reference take the target's size, [`Composition::layers_on`]). It
//! carries the program the NDI `SP-program` carries, so it keeps
//! `aspect_fit` ([`FHD_FIT`], #267). #251 step 2: that target is a SECOND
//! target of MAX's compositor ([`FHD_TARGET`], `Compositor::add_target`,
//! ids and draw order in the pure [`Targets`]): one device, one upload of
//! each picture ([`Composition::uploads`]), every target drawn from it,
//! one wait; `SpoutSender::for_target` sends any one of them, and each
//! target's own share of the draw is [`target_draws_us`].
//!
//! #223 follow-up: [`VblankTracker`] measures the primary display's refresh
//! (DWM's clock, which Arena renders in) on its own thread, `WaitForVBlank`
//! on the output [`pick_output`] chooses, fitted by [`VblankFit`] into a
//! [`VblankGrid`]; sp-server sends each MAX picture at a fixed point of it.
//!
//! #243 follow-up: [`spout_list_holder`] names the program that holds
//! Spout's sender list ([`LockHolder`]), and a [`ListSentinel`] waits on
//! that lock to give it back the moment the holder exits ([`SentinelEnd`]).
//!
//! #279 P0: [`probe_gpu`] asks the picked adapter, on two devices of it,
//! what the GPU-resident video path needs (shared NV12 by NT handle and
//! keyed mutex, its plane views, a `DO_NOT_WAIT` readback); the report, its
//! verdict and every rule that reads an answer are the pure [`probe`].
//!
//! #279 P1a: the compositor converts one target (the FHD program) to UYVY
//! for NDI on the GPU (`Compositor::enable_uyvy`): a pass into a half-width
//! `R8G8B8A8_UNORM` texture, copied into a ring of staging textures inside
//! the compose, read back without a wait (`Compositor::read_uyvy`). Its
//! conversion, its CPU model and its ring are the pure [`uyvy`].
//!
//! #279 P2: the GPU-resident video path's picture ring. A pipeline's
//! decoded pictures will (P3a) be copied into slots of its ring: shared
//! NV12 textures (an NT handle and a keyed mutex each), made on its
//! producer device (`PictureRing`, Windows). Each picture travels as a
//! [`GpuFrame`] (an `Arc`, like sp-server's `SharedFrame`; its drop frees
//! the slot) and is drawn as a [`Side::Gpu`] of a composition: the
//! compositor opens each slot once on its own device and samples it in
//! place under the keyed mutex, with no upload. Every decision (slots,
//! size classes, back-pressure, keys, a lost device, the open slots) is
//! the pure [`ring_state`]; [`FakeRing`] (doc-hidden) makes frames without
//! textures, for the tests of every platform.
//!
//! #279 P3a: `ProducerGpu` is a pipeline's producer device (the picked
//! adapter, the video API, multithread-protected: what Media Foundation
//! decodes on) and its ring: sp-decoder's GPU-resident reader copies each
//! decoded picture into a slot of it (`ProducerGpu::copy_from`, Windows;
//! off Windows it cannot be built). [`RingStats`] carries the copies'
//! p99 and the slots' VRAM.

mod adapter;
mod color;
mod composition;
mod error;
mod picture;
pub mod probe;
mod quad;
mod readback;
pub mod reference;
mod residency;
mod ring;
pub mod ring_state;
mod spout;
mod spout_lock;
pub mod spout_state;
mod stats;
mod targets;
pub mod uyvy;
mod vblank;

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod win;

pub use adapter::{
    AdapterInfo, BASIC_RENDER_DEVICE_ID, MICROSOFT_VENDOR_ID, adapter_name, pick_adapter,
};
pub use color::{BT709_LIMITED_TO_FULL, KB, KR, matrix_f32};
pub use composition::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Composition, FHD_FIT, FHD_HEIGHT, FHD_TARGET, FHD_WIDTH, Fit,
    Layer, MAX_FIT, MAX_TARGET, Q8_ONE, Side, Slot, Target,
};
pub use error::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, GpuError, is_device_lost,
};
pub use picture::{MAX_PICTURE_SIDE, Nv12Picture, PictureError, Plane, Planes};
pub use quad::{QUAD_CONSTANTS_BYTES, QuadConstants, ndc_rect, uv_in_texture, uv_rect};
pub use readback::{mapped_len, unpad_rows, unpad_rows_into};
pub use residency::{Resident, Upload, swap_slots, upload_for};
#[doc(hidden)]
pub use ring::FAKE_HANDLE_BASE;
pub use ring::{COPY_WINDOW, FakeRing, GpuFrame, RingStats};
pub use ring_state::{RING_CAPACITY, RING_WAIT, RingFormat, SizeClass, SlotKey};
pub use spout::{
    NAME_SLOT_LEN, SENDER_NAMES_MAP, SHARED_TEXTURE_INFO_LEN, SPOUT_FHD_SENDER_NAME,
    SPOUT_NAME_MAX_LEN, SPOUT_SENDER_NAME, SharedTextureInfo, check_sender_name,
    parse_sender_names,
};
pub use spout_lock::{LockHolder, SentinelEnd};
pub use stats::{ComposeStats, SpoutSendStats, split_uyvy_part, target_draws_us};
pub use targets::{TargetId, Targets};
pub use vblank::{
    OutputInfo, Seen, VBLANK_BOOT_INTERVALS, VBLANK_IDLE_MAX, VBLANK_MAX_PERIOD, VBLANK_MIN_FIT,
    VBLANK_MIN_PERIOD, VBLANK_MIN_WAIT, VBLANK_NOT_TICKING, VBLANK_RECHECK, VBLANK_RESTART,
    VBLANK_STALE, VBLANK_WINDOW, VblankFit, VblankGrid, VblankState, grid_is_fresh,
    not_waiting_sleep, pick_output, recheck_due, repick, vblank_state, waited,
};

#[cfg(not(windows))]
pub use stub::{
    Compositor, ListSentinel, ProducerGpu, SpoutSender, VblankTracker, probe_gpu,
    spout_list_holder, spout_sender_info, spout_sender_names,
};
#[cfg(windows)]
pub use win::{
    Compositor, ListSentinel, PictureRing, ProducerGpu, SpoutSender, VblankTracker, VideoDevice,
    adapters, mutex_holder, probe_gpu, probe_on_listed_adapter, probe_warp, read_shared_texture,
    spout_list_holder, spout_sender_info, spout_sender_names,
};
