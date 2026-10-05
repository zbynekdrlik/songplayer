//! The Direct3D 11 device Media Foundation decodes video on (#223 S3b,
//! `VideoDevice`): created with the video API, multithread-protected, on the
//! adapter `pick_adapter` chooses (never WARP on its own), or on WARP for CI.
//!
//! Windows only. `windows-latest` has no GPU: `VideoDevice::new` reports
//! `NoAdapter` there, and its explicit-adapter path runs on the listed Basic
//! Render Driver. A WARP refusal FAILS here; nothing is skipped.

#![cfg(windows)]

use sp_gpu::{GpuError, VideoDevice, pick_adapter};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, ID3D11Multithread,
    ID3D11VideoDevice,
};
use windows::core::Interface;

/// The device has the video API (its creation flags and the
/// `ID3D11VideoDevice` interface DXVA decoders query) and is
/// multithread-protected.
fn assert_ready_for_media_foundation(video: &VideoDevice, what: &str) {
    let device = video.device();
    let flags = unsafe { device.GetCreationFlags() };
    assert_ne!(
        flags & D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
        0,
        "{what}: created with the video API ({flags:#x})"
    );
    assert_ne!(
        flags & D3D11_CREATE_DEVICE_BGRA_SUPPORT.0,
        0,
        "{what}: BGRA support ({flags:#x})"
    );
    let video_api: Result<ID3D11VideoDevice, _> = device.cast();
    assert!(video_api.is_ok(), "{what}: ID3D11VideoDevice {video_api:?}");
    let multithread: ID3D11Multithread = device
        .cast()
        .unwrap_or_else(|e| panic!("{what}: ID3D11Multithread {e}"));
    assert!(
        unsafe { multithread.GetMultithreadProtected() }.as_bool(),
        "{what}: multithread-protected (Media Foundation drives it from its own threads)"
    );
}

#[test]
fn a_warp_video_device_has_the_video_api_and_multithread_protection() {
    let video = VideoDevice::new_warp()
        .unwrap_or_else(|e| panic!("WARP must accept D3D11_CREATE_DEVICE_VIDEO_SUPPORT: {e}"));
    assert_ready_for_media_foundation(&video, "WARP");
    let adapter = video.adapter();
    assert!(adapter.software_flag, "WARP's adapter: {adapter:?}");
    assert_eq!((adapter.vendor_id, adapter.device_id), (0x1414, 0x8c));
}

#[test]
fn a_video_device_runs_on_a_listed_adapter_the_way_new_does() {
    // windows-latest has no GPU, so VideoDevice::new's device path (an
    // explicit DXGI adapter, D3D_DRIVER_TYPE_UNKNOWN) runs on the Basic Render
    // Driver.
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    let index = adapters
        .iter()
        .position(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c)
        .expect("DXGI always lists the Basic Render Driver");
    let video = VideoDevice::new_on_listed_adapter(index)
        .unwrap_or_else(|e| panic!("a video device on the listed Basic Render Driver: {e}"));
    assert_eq!(video.adapter(), &adapters[index]);
    assert_ready_for_media_foundation(&video, "listed adapter");
}

#[test]
fn new_takes_the_picked_adapter_or_reports_none() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    match pick_adapter(&adapters) {
        None => assert_eq!(
            VideoDevice::new().err(),
            Some(GpuError::NoAdapter),
            "never WARP on its own: {adapters:?}"
        ),
        Some(index) => {
            let video = VideoDevice::new().unwrap_or_else(|e| {
                panic!(
                    "the picked adapter {:?} must make a video device: {e}",
                    adapters[index]
                )
            });
            assert_eq!(video.adapter(), &adapters[index]);
            assert_ready_for_media_foundation(&video, "picked adapter");
        }
    }
}

#[test]
fn an_adapter_index_past_the_list_is_no_adapter() {
    let count = sp_gpu::adapters().expect("DXGI lists its adapters").len();
    assert_eq!(
        VideoDevice::new_on_listed_adapter(count).err(),
        Some(GpuError::NoAdapter)
    );
}
