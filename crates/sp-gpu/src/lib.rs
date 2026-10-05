//! The GPU compositor of `SP-program-MAX` (#223, revision 3 R3-2, slice S1a).
//!
//! `SP-program-MAX` is a fixed 3840×2160 canvas. Every picture is
//! aspect-fitted into it with black bars (`sp_core::fit::aspect_fit`, the rule
//! `SP-program`'s 1920×1080 canvas and the preview use), and a fade blends
//! two pictures of any sizes. On the CPU a 4K fade costs ~40–45 ms per
//! boundary, over the 33.3 ms slot (revision 3), so it is drawn on the GPU:
//!
//! - one Direct3D 11 device on the largest non-software adapter
//!   ([`pick_adapter`]: the RTX 3070 Ti, never the box's virtual display
//!   adapters or the Basic Render Driver), or on WARP for tests and CI;
//! - each NV12 picture is uploaded as two textures, its Y plane as R8 and its
//!   interleaved UV plane as R8G8, honouring the stride; a picture whose id
//!   is already resident on its side is not uploaded again ([`upload_for`]);
//! - the render target is cleared to black, then one textured quad per
//!   picture is drawn into its [`aspect_fit`](sp_core::fit::aspect_fit)
//!   rectangle: a bilinear sampler (the scale), a pixel shader for BT.709
//!   limited range → full-range RGB ([`BT709_LIMITED_TO_FULL`]), and additive
//!   blending at the picture's weight — the outgoing side at 1 − w first, then
//!   the incoming one at w ([`Composition::layers`]);
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

mod adapter;
mod color;
mod composition;
mod error;
mod picture;
mod quad;
mod readback;
pub mod reference;
mod residency;
mod spout;
pub mod spout_state;
mod stats;

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod win;

pub use adapter::{
    AdapterInfo, BASIC_RENDER_DEVICE_ID, MICROSOFT_VENDOR_ID, adapter_name, pick_adapter,
};
pub use color::{BT709_LIMITED_TO_FULL, KB, KR, matrix_f32};
pub use composition::{CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Layer, Q8_ONE, Slot};
pub use error::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, GpuError, is_device_lost,
};
pub use picture::{MAX_PICTURE_SIDE, Nv12Picture, PictureError, Plane, Planes};
pub use quad::{QUAD_CONSTANTS_BYTES, QuadConstants, ndc_rect};
pub use readback::{mapped_len, unpad_rows, unpad_rows_into};
pub use residency::{Resident, Upload, upload_for};
pub use spout::{
    NAME_SLOT_LEN, SENDER_NAMES_MAP, SHARED_TEXTURE_INFO_LEN, SPOUT_NAME_MAX_LEN,
    SPOUT_SENDER_NAME, SharedTextureInfo, check_sender_name, parse_sender_names,
};
pub use stats::{ComposeStats, SpoutSendStats};

#[cfg(not(windows))]
pub use stub::{Compositor, SpoutSender, spout_sender_info, spout_sender_names};
#[cfg(windows)]
pub use win::{
    Compositor, SpoutSender, VideoDevice, adapters, read_shared_texture, spout_sender_info,
    spout_sender_names,
};
