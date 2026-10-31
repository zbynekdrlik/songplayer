//! #279 P0: the capability probe's Direct3D calls (glue). Two devices of
//! one adapter (A = a producer, B = the compositor), each step on its own
//! (a step that needs a shared NV12 texture makes its own), every answer
//! read by the pure `crate::probe` rules. The steps and why they matter:
//! `crate::probe`'s module doc.

use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_FEATURE_D3D11_OPTIONS, D3D11_FEATURE_DATA_D3D11_OPTIONS, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11Device1, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, DXGI_SHARED_RESOURCE_READ,
    DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter3, IDXGIDevice, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use super::device::{self, Device, DeviceUse};
use super::pipeline;
use super::textures::{self, NtHandle};
use crate::adapter::AdapterInfo;
use crate::error::GpuError;
use crate::probe::{
    CONSUMER_ACQUIRE_MS, GpuProbe, KEY_CONSUMER, KEY_PRODUCER, PROBE_CLEAR, PROBE_CLEAR_BYTES,
    PROBE_NV12_HEIGHT, PROBE_NV12_WIDTH, PROBE_UYVY_HEIGHT, PROBE_UYVY_WIDTH, PRODUCER_ACQUIRE_MS,
    ProbeError, ProbeReport, ProbeStep, Vram, acquire_step, content_step, feature_step, pixel_step,
    probe_picture,
};
use crate::readback::{mapped_len, unpad_rows};

/// A failed call's HRESULT.
fn code(error: &windows::core::Error) -> u32 {
    error.code().0 as u32
}

/// The probe on the adapter `pick_adapter` chooses (the compositor's: the
/// RTX on SNV): two devices on it, every step, and its video memory. A is
/// made like a producer, the device Media Foundation decodes on (the video
/// API, `DeviceUse::VideoDecode`), B like the compositor (BGRA).
/// `GpuError::NoAdapter` when there is none; a device that cannot be made
/// fails the whole probe (no step can run without it).
pub fn probe_gpu() -> Result<GpuProbe, GpuError> {
    let a = device::create_on_picked(DeviceUse::VideoDecode)?;
    let b = device::create_on_picked(DeviceUse::Probe)?;
    Ok(run(a, b.0, device::adapters()?))
}

/// The probe on two WARP devices, both BGRA (WARP refuses the video API,
/// `video-decode.md`): for CI (`tests/probe.rs`). Not for production.
#[doc(hidden)]
pub fn probe_warp() -> Result<GpuProbe, GpuError> {
    let a = device::create_warp(DeviceUse::Probe)?;
    let b = device::create_warp(DeviceUse::Probe)?;
    Ok(run(a, b.0, device::adapters()?))
}

/// The probe on DXGI's adapter `index` (`adapters()`' order), whatever
/// `pick_adapter` would say: `probe_gpu`'s path on a CI box whose only
/// adapter is the Basic Render Driver, with A BGRA (whether that driver
/// makes the video device is not proven, `video-decode.md`). Not for
/// production.
#[doc(hidden)]
pub fn probe_on_listed_adapter(index: usize) -> Result<GpuProbe, GpuError> {
    let a = device::create_on_listed(index, DeviceUse::Probe)?;
    let b = device::create_on_listed(index, DeviceUse::Probe)?;
    Ok(run(a, b.0, device::adapters()?))
}

/// Every step on A (with its adapter) and B, in the report's order.
fn run((a, adapter): (Device, AdapterInfo), b: Device, adapters: Vec<AdapterInfo>) -> GpuProbe {
    let report = ProbeReport {
        extended_resource_sharing: extended_sharing(&a.0).and_then(|()| extended_sharing(&b.0)),
        nv12_shared_nthandle: shared_nv12(&a, &b).map(drop),
        nv12_keyed_mutex_roundtrip: keyed_round_trip(&a, &b),
        nv12_srv_r8_r8g8: plane_views(&a, &b),
        uyvy_target_and_staging_do_not_wait: uyvy_readback(&b),
    };
    GpuProbe {
        vram: vram_of(&a.0),
        adapter,
        adapters,
        report,
    }
}

/// `ExtendedResourceSharing` of `device`'s `D3D11_OPTIONS`.
fn extended_sharing(device: &ID3D11Device) -> ProbeStep {
    const CALL: &str = "CheckFeatureSupport(D3D11_OPTIONS).ExtendedResourceSharing";
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
    // SAFETY: the data pointer and size are those of the options struct
    // the feature asks for.
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS,
            (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS).cast::<c_void>(),
            std::mem::size_of::<D3D11_FEATURE_DATA_D3D11_OPTIONS>() as u32,
        )
    }
    .map_err(|e| ProbeError::failed(CALL, code(&e)))?;
    feature_step(CALL, options.ExtendedResourceSharing.as_bool())
}

/// `CreateTexture2D` of `desc` (with `init`), named `call`.
fn texture(
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
    init: Option<&D3D11_SUBRESOURCE_DATA>,
    call: &'static str,
) -> Result<ID3D11Texture2D, ProbeError> {
    let mut texture: Option<ID3D11Texture2D> = None;
    let init = init.map(|init| init as *const D3D11_SUBRESOURCE_DATA);
    unsafe { device.CreateTexture2D(desc, init, Some(&mut texture)) }
        .map_err(|e| ProbeError::failed(call, code(&e)))?;
    texture.ok_or_else(|| ProbeError::refused(call, "gave no object"))
}

/// The probe's NV12 texture description.
fn nv12(bind: u32, misc: u32) -> D3D11_TEXTURE2D_DESC {
    textures::desc(
        PROBE_NV12_WIDTH,
        PROBE_NV12_HEIGHT,
        DXGI_FORMAT_NV12,
        D3D11_USAGE_DEFAULT,
        bind,
        0,
        misc,
    )
}

/// An NV12 texture made on A for sharing, and the same texture opened on B.
struct SharedNv12 {
    made: ID3D11Texture2D,
    opened: ID3D11Texture2D,
}

/// Step 2: an NV12 texture on A with `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`
/// (shader-resource bound, as the compositor samples it), its NT handle,
/// opened on B.
fn shared_nv12(a: &Device, b: &Device) -> Result<SharedNv12, ProbeError> {
    let misc =
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32;
    let desc = nv12(D3D11_BIND_SHADER_RESOURCE.0 as u32, misc);
    let made = texture(
        &a.0,
        &desc,
        None,
        "CreateTexture2D (NV12, SHARED_NTHANDLE | SHARED_KEYEDMUTEX)",
    )?;
    let resource: IDXGIResource1 = made
        .cast()
        .map_err(|e| ProbeError::failed("IDXGIResource1 (the NV12 on A)", code(&e)))?;
    let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
    let handle = unsafe { resource.CreateSharedHandle(None, access, PCWSTR::null()) }
        .map_err(|e| ProbeError::failed("IDXGIResource1::CreateSharedHandle", code(&e)))?;
    let handle = NtHandle(handle.0 as isize);
    let device1: ID3D11Device1 =
        b.0.cast()
            .map_err(|e| ProbeError::failed("ID3D11Device1 (B)", code(&e)))?;
    let opened: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle.handle()) }
        .map_err(|e| ProbeError::failed("ID3D11Device1::OpenSharedResource1 (B)", code(&e)))?;
    Ok(SharedNv12 { made, opened })
}

/// A keyed mutex's `AcquireSync`, its RAW HRESULT: the `windows` crate's
/// `Result` would turn `WAIT_TIMEOUT` and `WAIT_ABANDONED` (success codes)
/// into `Ok` (`probe::acquire_step` reads it).
pub(super) fn acquire(mutex: &IDXGIKeyedMutex, key: u64, milliseconds: u32) -> u32 {
    // SAFETY: the interface's own vtable entry, called on its own pointer.
    let hresult = unsafe {
        (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, milliseconds)
    };
    hresult.0 as u32
}

/// Step 3: A acquires key 0, writes the shared texture (a copy of
/// `probe_picture`, made with initial data), releases key 1; B acquires key
/// 1 with a 0 ms timeout, reads the texture back, releases key 1; the bytes
/// must be A's (`content_step`).
fn keyed_round_trip(a: &Device, b: &Device) -> ProbeStep {
    let shared = shared_nv12(a, b)?;
    let picture = probe_picture();
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: picture.as_ptr().cast(),
        SysMemPitch: PROBE_NV12_WIDTH,
        SysMemSlicePitch: 0,
    };
    let source = texture(
        &a.0,
        &nv12(0, 0),
        Some(&init),
        "CreateTexture2D (NV12 picture on A)",
    )?;
    let on_a: IDXGIKeyedMutex = shared
        .made
        .cast()
        .map_err(|e| ProbeError::failed("IDXGIKeyedMutex (A)", code(&e)))?;
    let on_b: IDXGIKeyedMutex = shared
        .opened
        .cast()
        .map_err(|e| ProbeError::failed("IDXGIKeyedMutex (B)", code(&e)))?;
    acquire_step(
        "IDXGIKeyedMutex::AcquireSync (A, key 0)",
        acquire(&on_a, KEY_PRODUCER, PRODUCER_ACQUIRE_MS),
    )?;
    unsafe {
        a.1.CopyResource(&shared.made, &source);
        a.1.Flush();
    }
    unsafe { on_a.ReleaseSync(KEY_CONSUMER) }
        .map_err(|e| ProbeError::failed("IDXGIKeyedMutex::ReleaseSync (A, key 1)", code(&e)))?;
    acquire_step(
        "IDXGIKeyedMutex::AcquireSync (B, key 1, 0 ms)",
        acquire(&on_b, KEY_CONSUMER, CONSUMER_ACQUIRE_MS),
    )?;
    let read = read_nv12(b, &shared.opened);
    unsafe { on_b.ReleaseSync(KEY_CONSUMER) }
        .map_err(|e| ProbeError::failed("IDXGIKeyedMutex::ReleaseSync (B, key 1)", code(&e)))?;
    content_step("the shared NV12 as B read it back", read?)
}

/// The NV12 texture `opened` on B, copied to a staging copy and mapped: its
/// rows packed (`None` when the mapping holds no whole picture). The mapped
/// NV12 is Direct3D's layout: the UV plane follows the luma at
/// `RowPitch × Height`, so it is `Height × 3/2` rows `RowPitch` apart.
fn read_nv12(b: &Device, opened: &ID3D11Texture2D) -> Result<Option<Vec<u8>>, ProbeError> {
    let (device, context) = b;
    let read = D3D11_CPU_ACCESS_READ.0 as u32;
    let desc = textures::desc(
        PROBE_NV12_WIDTH,
        PROBE_NV12_HEIGHT,
        DXGI_FORMAT_NV12,
        D3D11_USAGE_STAGING,
        0,
        read,
        0,
    );
    let staging = texture(device, &desc, None, "CreateTexture2D (NV12 staging on B)")?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.CopyResource(&staging, opened);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    }
    .map_err(|e| ProbeError::failed("Map (B's NV12 staging copy)", code(&e)))?;
    let pitch = mapped.RowPitch as usize;
    let (row, rows) = (
        PROBE_NV12_WIDTH as usize,
        PROBE_NV12_HEIGHT as usize * 3 / 2,
    );
    let packed = match mapped_len(pitch, row, rows) {
        Some(len) if !mapped.pData.is_null() => {
            // SAFETY: the mapped NV12 staging texture is `rows` rows `pitch`
            // bytes apart (`pitch` ≥ `row`, `mapped_len`), so its first `len`
            // bytes end with the last UV row's pixels, inside the mapping,
            // which stays mapped until the Unmap below.
            let bytes =
                unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().cast_const(), len) };
            unpad_rows(bytes, pitch, row, rows)
        }
        _ => None,
    };
    unsafe { context.Unmap(&staging, 0) };
    Ok(packed)
}

/// A shader view of `texture` as `format` (one plane of an NV12 texture).
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    call: &'static str,
) -> ProbeStep {
    let desc = textures::plane_view_desc(format);
    let mut view: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }
        .map_err(|e| ProbeError::failed(call, code(&e)))?;
    if view.is_some() {
        Ok(())
    } else {
        Err(ProbeError::refused(call, "gave no object"))
    }
}

/// Step 4: the R8 (luma) and R8G8 (chroma) views on the NV12 B opened.
fn plane_views(a: &Device, b: &Device) -> ProbeStep {
    let shared = shared_nv12(a, b)?;
    plane_view(
        &b.0,
        &shared.opened,
        DXGI_FORMAT_R8_UNORM,
        "CreateShaderResourceView (R8_UNORM, the opened NV12)",
    )?;
    plane_view(
        &b.0,
        &shared.opened,
        DXGI_FORMAT_R8G8_UNORM,
        "CreateShaderResourceView (R8G8_UNORM, the opened NV12)",
    )
}

/// Step 5, on B: an R8G8B8A8 960×1080 render target cleared, copied to a
/// staging texture, the copy waited for (an event query, as the
/// compositor waits), then mapped with `DO_NOT_WAIT`; its first texel must
/// be the clear.
fn uyvy_readback(b: &Device) -> ProbeStep {
    let (device, context) = b;
    let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    let size = (PROBE_UYVY_WIDTH, PROBE_UYVY_HEIGHT);
    let target_desc = textures::desc(
        size.0,
        size.1,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        D3D11_USAGE_DEFAULT,
        bind,
        0,
        0,
    );
    let target = texture(
        device,
        &target_desc,
        None,
        "CreateTexture2D (R8G8B8A8 960x1080 render target)",
    )?;
    let mut view: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(&target, None, Some(&mut view)) }
        .map_err(|e| ProbeError::failed("CreateRenderTargetView (R8G8B8A8)", code(&e)))?;
    let view = view.ok_or_else(|| {
        ProbeError::refused("CreateRenderTargetView (R8G8B8A8)", "gave no object")
    })?;
    let read = D3D11_CPU_ACCESS_READ.0 as u32;
    let staging_desc = textures::desc(
        size.0,
        size.1,
        DXGI_FORMAT_R8G8B8A8_UNORM,
        D3D11_USAGE_STAGING,
        0,
        read,
        0,
    );
    let staging = texture(
        device,
        &staging_desc,
        None,
        "CreateTexture2D (R8G8B8A8 960x1080 staging)",
    )?;
    let done = pipeline::query(device).map_err(|e| ProbeError::from_gpu(&e))?;
    unsafe {
        context.ClearRenderTargetView(&view, &PROBE_CLEAR);
        context.CopyResource(&staging, &target);
        context.End(&done);
    }
    pipeline::wait_until_each_done(context, &[&done]).map_err(|e| ProbeError::from_gpu(&e))?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let flags = D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32;
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, flags, Some(&mut mapped)) }
        .map_err(|e| ProbeError::failed("Map (staging, DO_NOT_WAIT)", code(&e)))?;
    let first = (!mapped.pData.is_null()).then(|| {
        // SAFETY: a mapped 960-texel row holds at least its first texel.
        let texel = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 4) };
        [texel[0], texel[1], texel[2], texel[3]]
    });
    unsafe { context.Unmap(&staging, 0) };
    pixel_step("Map (the staging texel)", first, PROBE_CLEAR_BYTES)
}

/// The adapter's local video memory, from `device`'s adapter.
fn vram_of(device: &ID3D11Device) -> Result<Vram, ProbeError> {
    let dxgi: IDXGIDevice = device
        .cast()
        .map_err(|e| ProbeError::failed("IDXGIDevice", code(&e)))?;
    let adapter = unsafe { dxgi.GetAdapter() }
        .map_err(|e| ProbeError::failed("IDXGIDevice::GetAdapter", code(&e)))?;
    let adapter: IDXGIAdapter3 = adapter
        .cast()
        .map_err(|e| ProbeError::failed("IDXGIAdapter3", code(&e)))?;
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    unsafe { adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }
        .map_err(|e| ProbeError::failed("IDXGIAdapter3::QueryVideoMemoryInfo (local)", code(&e)))?;
    Ok(Vram::from_bytes(info.Budget, info.CurrentUsage))
}
