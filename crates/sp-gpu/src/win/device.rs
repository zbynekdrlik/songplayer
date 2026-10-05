//! The Direct3D 11 device: DXGI's adapter list, the picked hardware
//! adapter, or WARP. The compositor's device and Media Foundation's video
//! decode device (#223 S3b, `video_device.rs`) are made here, on the same
//! adapter rule; they differ only in their creation flags ([`DeviceUse`]).

use tracing::{debug, info};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};
use windows::core::Interface;

use super::failed;
use crate::adapter::{AdapterInfo, adapter_name, pick_adapter};
use crate::error::GpuError;

/// The feature levels asked for: 11.1, else 11.0 (shader model 5).
const FEATURE_LEVELS: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

/// The device and its immediate context.
pub(super) type Device = (ID3D11Device, ID3D11DeviceContext);

/// What a device is made for, which decides its creation flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceUse {
    /// The `SP-program-MAX` compositor: BGRA support (the render target's
    /// format, and Spout's).
    Compose,
    /// Media Foundation's hardware video decode (#223 S3b): BGRA plus the
    /// Direct3D 11 video API (`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, which DXVA
    /// decoding needs; WARP accepts it too).
    VideoDecode,
}

impl DeviceUse {
    fn flags(self) -> D3D11_CREATE_DEVICE_FLAG {
        match self {
            DeviceUse::Compose => D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            DeviceUse::VideoDecode => {
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT
            }
        }
    }
}

/// What `pick_adapter` reads from one DXGI adapter.
fn info_of(desc: &DXGI_ADAPTER_DESC1) -> AdapterInfo {
    let software = DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32;
    AdapterInfo {
        name: adapter_name(&desc.Description),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        dedicated_video_memory: desc.DedicatedVideoMemory as u64,
        software_flag: (desc.Flags & software) != 0,
    }
}

/// Every DXGI adapter, in DXGI's order, with what `pick_adapter` reads.
fn list() -> Result<Vec<(IDXGIAdapter1, AdapterInfo)>, GpuError> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
        .map_err(|e| failed("CreateDXGIFactory1", &e))?;
    let mut adapters = Vec::new();
    for index in 0u32.. {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(failed("EnumAdapters1", &e)),
        };
        let desc = unsafe { adapter.GetDesc1() }.map_err(|e| failed("GetDesc1", &e))?;
        adapters.push((adapter, info_of(&desc)));
    }
    Ok(adapters)
}

/// Every DXGI adapter of this machine, in DXGI's order (the Microsoft Basic
/// Render Driver is always listed, last).
pub fn adapters() -> Result<Vec<AdapterInfo>, GpuError> {
    Ok(list()?.into_iter().map(|(_, info)| info).collect())
}

/// The device for `usage` on the adapter `pick_adapter` chooses, and that
/// adapter. The compositor's (made once) logs every adapter seen; a video
/// decode device is made per opened file, so it logs them at DEBUG.
pub(super) fn create_on_picked(usage: DeviceUse) -> Result<(Device, AdapterInfo), GpuError> {
    let adapters = list()?;
    let infos: Vec<AdapterInfo> = adapters.iter().map(|(_, info)| info.clone()).collect();
    for (index, info) in infos.iter().enumerate() {
        let vram_mb = info.dedicated_video_memory / (1024 * 1024);
        if usage == DeviceUse::Compose {
            info!(
                index,
                name = %info.name,
                vendor_id = info.vendor_id,
                device_id = info.device_id,
                vram_mb,
                software = info.is_software(),
                "sp-gpu: DXGI adapter"
            );
        } else {
            debug!(
                index,
                name = %info.name,
                vram_mb,
                software = info.is_software(),
                "sp-gpu: DXGI adapter (video decode)"
            );
        }
    }
    let picked = pick_adapter(&infos).ok_or(GpuError::NoAdapter)?;
    create_on(adapters, picked, usage)
}

/// The device on DXGI's adapter `index` ([`adapters`]' order), whatever
/// `pick_adapter` would say: the path `create_on_picked` takes, for a test
/// on a box whose only adapter is software.
pub(super) fn create_on_listed(
    index: usize,
    usage: DeviceUse,
) -> Result<(Device, AdapterInfo), GpuError> {
    create_on(list()?, index, usage)
}

/// The device on adapter `index` of `adapters` (`D3D_DRIVER_TYPE_UNKNOWN`:
/// the adapter decides the driver), and the adapter the DEVICE reports it
/// runs on (read back from it, not copied from the list).
fn create_on(
    mut adapters: Vec<(IDXGIAdapter1, AdapterInfo)>,
    index: usize,
    usage: DeviceUse,
) -> Result<(Device, AdapterInfo), GpuError> {
    if index >= adapters.len() {
        return Err(GpuError::NoAdapter);
    }
    let (hardware, _) = adapters.swap_remove(index);
    let adapter: &IDXGIAdapter = &hardware;
    let device = create(Some(adapter), D3D_DRIVER_TYPE_UNKNOWN, usage)?;
    let info = adapter_of(&device.0)?;
    Ok((device, info))
}

/// A WARP device (the CPU rasterizer) for `usage`, and its adapter as DXGI
/// describes it.
pub(super) fn create_warp(usage: DeviceUse) -> Result<(Device, AdapterInfo), GpuError> {
    let device = create(None, D3D_DRIVER_TYPE_WARP, usage)?;
    let info = adapter_of(&device.0)?;
    Ok((device, info))
}

/// `D3D11CreateDevice` at feature level 11.1 or 11.0, with `usage`'s flags.
fn create(
    adapter: Option<&IDXGIAdapter>,
    driver: D3D_DRIVER_TYPE,
    usage: DeviceUse,
) -> Result<Device, GpuError> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            None,
            usage.flags(),
            Some(&FEATURE_LEVELS[..]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| failed("D3D11CreateDevice", &e))?;
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(GpuError::NoObject {
            call: "D3D11CreateDevice",
        }),
    }
}

/// The adapter a device runs on, as DXGI describes it.
fn adapter_of(device: &ID3D11Device) -> Result<AdapterInfo, GpuError> {
    let dxgi: IDXGIDevice = device.cast().map_err(|e| failed("IDXGIDevice", &e))?;
    let adapter = unsafe { dxgi.GetAdapter() }.map_err(|e| failed("GetAdapter", &e))?;
    let adapter: IDXGIAdapter1 = adapter.cast().map_err(|e| failed("IDXGIAdapter1", &e))?;
    let desc = unsafe { adapter.GetDesc1() }.map_err(|e| failed("GetDesc1", &e))?;
    Ok(info_of(&desc))
}
