//! The Direct3D 11 device Media Foundation decodes video on (#223 S3b,
//! `VideoDevice`): created with the video API, multithread-protected, on the
//! adapter `pick_adapter` chooses (never WARP on its own).
//!
//! Windows only. `windows-latest` has no GPU, and its WARP has no video API:
//! `D3D11CreateDevice` on WARP with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`
//! returns DXGI_ERROR_UNSUPPORTED (CI run 37293259981), although Microsoft's
//! `D3D11_CREATE_DEVICE_FLAG` page says a WARP device "succeeds to allow
//! software fallback for video". That refusal is asserted here as a fact of
//! the runner: a WARP that one day takes the flag fails this, and the tests
//! built on the refusal (sp-decoder's `tests/mf_hw_decode.rs`) must then be
//! written for the D3D path they reach. `VideoDevice::new` reports
//! `NoAdapter` there. Nothing is skipped.

#![cfg(windows)]

use sp_gpu::{GpuError, VideoDevice, pick_adapter};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_UNSUPPORTED, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
};
use windows::core::Interface;

/// The flags `VideoDevice` creates its device with (`DeviceUse::VideoDecode`).
fn video_flags() -> D3D11_CREATE_DEVICE_FLAG {
    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT
}

/// What the driver itself answers to `D3D11CreateDevice` with `flags`, at
/// the feature levels `sp_gpu` asks for (11.1, else 11.0): on DXGI's
/// `adapter` (`D3D_DRIVER_TYPE_UNKNOWN`, the adapter decides), or on WARP
/// for `None`. `Ok` when it made a device, else the call's HRESULT.
fn driver_answer(
    adapter: Option<&IDXGIAdapter>,
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> Result<(), u32> {
    let driver = match adapter {
        Some(_) => D3D_DRIVER_TYPE_UNKNOWN,
        None => D3D_DRIVER_TYPE_WARP,
    };
    let mut device: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            None,
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0][..]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|e| e.code().0 as u32)?;
    assert!(
        device.is_some(),
        "D3D11CreateDevice succeeded with no device"
    );
    Ok(())
}

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

/// WARP makes a device with the compositor's flags (BGRA) but refuses the
/// video API, and `VideoDevice::new_warp` reports exactly that refusal:
/// sp-decoder's `Hardware` reader on WARP falls back at open on it.
#[test]
fn warp_refuses_the_video_api_and_new_warp_reports_it() {
    driver_answer(None, D3D11_CREATE_DEVICE_BGRA_SUPPORT)
        .unwrap_or_else(|hr| panic!("WARP makes a BGRA device (the compositor's): {hr:#010x}"));
    let unsupported = DXGI_ERROR_UNSUPPORTED.0 as u32;
    assert_eq!(
        driver_answer(None, video_flags()).err(),
        Some(unsupported),
        "windows-latest's WARP refuses D3D11_CREATE_DEVICE_VIDEO_SUPPORT"
    );
    assert_eq!(
        VideoDevice::new_warp().err(),
        Some(GpuError::Api {
            call: "D3D11CreateDevice",
            hresult: unsupported,
        })
    );
}

/// `VideoDevice::new`'s device path (an explicit DXGI adapter,
/// `D3D_DRIVER_TYPE_UNKNOWN`) on the listed Basic Render Driver, the only
/// adapter of `windows-latest`. Whether that driver takes the video API is
/// NOT proven on CI: Microsoft says it does, in the same paragraph that
/// says WARP does, which CI refuted. So the test asks the driver first,
/// with the same call, and asserts that `new_on_listed_adapter` agrees: a
/// device on that adapter, ready for Media Foundation, or the driver's own
/// refusal as `GpuError::Api`. A device made without the video flag, or
/// on another adapter, fails either way. The answer is printed; pin it here
/// once a CI run has shown it.
#[test]
fn a_video_device_on_a_listed_adapter_agrees_with_the_driver() {
    let adapters = sp_gpu::adapters().expect("DXGI lists its adapters");
    let index = adapters
        .iter()
        .position(|a| a.vendor_id == 0x1414 && a.device_id == 0x8c)
        .expect("DXGI always lists the Basic Render Driver");
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("CreateDXGIFactory1");
    let listed: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(index as u32) }
        .unwrap_or_else(|e| panic!("DXGI adapter {index}: {e}"));
    let adapter: &IDXGIAdapter = &listed;
    let answer = driver_answer(Some(adapter), video_flags());
    match answer {
        Ok(()) => eprintln!("the listed Basic Render Driver takes the video API"),
        Err(hr) => eprintln!("the listed Basic Render Driver refuses the video API: {hr:#010x}"),
    }
    match (answer, VideoDevice::new_on_listed_adapter(index)) {
        (Ok(()), Ok(video)) => {
            assert_eq!(video.adapter(), &adapters[index]);
            assert_ready_for_media_foundation(&video, "listed adapter");
        }
        (Err(hresult), Err(e)) => assert_eq!(
            e,
            GpuError::Api {
                call: "D3D11CreateDevice",
                hresult,
            }
        ),
        (Ok(()), Err(e)) => panic!("the driver takes the video API, VideoDevice failed: {e}"),
        (Err(hresult), Ok(_)) => {
            panic!("the driver refused the video API ({hresult:#010x}), VideoDevice made a device")
        }
    }
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
