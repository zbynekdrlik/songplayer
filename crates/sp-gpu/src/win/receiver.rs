//! A receiver's view of a Spout sender (#223 S2): its shared texture opened
//! on a SECOND WARP device, as Resolume Arena opens it on its own device,
//! and read back. For the WARP tests of the program output (sp-server's
//! `program_max_tests_warp.rs`), which cannot reach this crate's test
//! helpers; `tests/spout.rs` does the same with its own code.

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use super::{device, failed, textures};
use crate::error::GpuError;
use crate::spout::SharedTextureInfo;

/// The 3840×2160 BGRA pixels of the shared texture `info` names (a sender's
/// map, `spout_sender_info`), opened from its handle on a new WARP device
/// and read back: rows packed, as `Compositor::read_back` returns them.
///
/// It takes no Spout mutex and no keyed mutex (Spout's texture has none):
/// the caller makes sure the sender's copy is complete, e.g. by reading after
/// a LATER frame was composed on the sender's device (its draw waits for the
/// GPU, which runs the copy before it). For tests, not production.
#[doc(hidden)]
pub fn read_shared_texture(info: &SharedTextureInfo) -> Result<Vec<u8>, GpuError> {
    let ((device, context), _) = device::create_warp()?;
    let handle = HANDLE(info.share_handle_value() as *mut c_void);
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.OpenSharedResource(handle, &mut texture) }
        .map_err(|e| failed("OpenSharedResource", &e))?;
    let texture = texture.ok_or(GpuError::NoObject {
        call: "OpenSharedResource",
    })?;
    let staging = textures::staging(&device)?;
    textures::read_back(&context, &texture, &staging)
}
