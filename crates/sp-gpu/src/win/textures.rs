//! The textures: the shared BGRA render target (3840×2160 for MAX; #239:
//! 1920×1080 for the `SP-program` sender), a slot's two plane textures (Y as
//! R8, UV as R8G8), and the staging copy for readback (#279 P1a: the
//! compositor's mapped-rows reader, `map_rows_into`, the UYVY readback's
//! too).

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_SAMPLE_DESC,
};

use super::failed;
use crate::error::GpuError;
use crate::picture::{Nv12Picture, Planes};
use crate::readback::{mapped_len, unpad_rows_into};
use crate::residency::Resident;

/// A one-mip, one-sample 2D texture's description (#279 P0: the probe's
/// textures too, `probe.rs`).
pub(super) fn desc(
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
    misc: u32,
) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: misc,
    }
}

/// `CreateTexture2D` with no initial data (#279 P1a: the UYVY pass's
/// textures too, `uyvy.rs`).
pub(super) fn texture(
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D, GpuError> {
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(desc, None, Some(&mut texture)) }
        .map_err(|e| failed("CreateTexture2D", &e))?;
    texture.ok_or(GpuError::NoObject {
        call: "CreateTexture2D",
    })
}

/// #279 P0 / P2: the description of a shader view of ONE plane of an NV12
/// texture as `format` (R8 = the luma, R8G8 = the interleaved chroma at
/// half size: Microsoft's NV12 view rule): the probe's views and a ring
/// slot's.
pub(super) fn plane_view_desc(format: DXGI_FORMAT) -> D3D11_SHADER_RESOURCE_VIEW_DESC {
    D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    }
}

/// #279 P0 / P2: a shared texture's NT handle (`CreateSharedHandle`),
/// closed once when dropped (what was opened through it stays open). Kept
/// as a number, so it is `Send` and `Sync`: every ring frame holds its
/// slot's.
pub(super) struct NtHandle(pub isize);

impl NtHandle {
    /// The handle, for a call that takes one.
    pub fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

impl Drop for NtHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateSharedHandle` and is closed
        // once, here.
        let _ = unsafe { CloseHandle(self.handle()) };
    }
}

/// A shader view of the whole texture (#279 P1a: the UYVY pass reads its
/// target through one).
pub(super) fn shader_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView, GpuError> {
    let mut view: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(texture, None, Some(&mut view)) }
        .map_err(|e| failed("CreateShaderResourceView", &e))?;
    view.ok_or(GpuError::NoObject {
        call: "CreateShaderResourceView",
    })
}

/// The `width`×`height` `B8G8R8A8_UNORM` render target. Created
/// `D3D11_RESOURCE_MISC_SHARED`, not keyed: the description Spout2's own
/// sender texture has (`spoutDirectX::CreateSharedDX11Texture`, keyed =
/// false), so S1b may hand its handle to Spout or `SendTexture` it.
pub(super) struct RenderTarget {
    pub texture: ID3D11Texture2D,
    pub view: ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}

impl RenderTarget {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, GpuError> {
        let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        let texture = texture(
            device,
            &desc(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_DEFAULT,
                bind,
                0,
                D3D11_RESOURCE_MISC_SHARED.0 as u32,
            ),
        )?;
        let mut view: Option<ID3D11RenderTargetView> = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut view)) }
            .map_err(|e| failed("CreateRenderTargetView", &e))?;
        let view = view.ok_or(GpuError::NoObject {
            call: "CreateRenderTargetView",
        })?;
        Ok(Self {
            texture,
            view,
            width,
            height,
        })
    }
}

/// One slot's plane textures and what they hold.
pub(super) struct PlaneTextures {
    /// The picture last written into them.
    pub resident: Resident,
    luma: ID3D11Texture2D,
    chroma: ID3D11Texture2D,
    pub luma_view: ID3D11ShaderResourceView,
    pub chroma_view: ID3D11ShaderResourceView,
}

impl PlaneTextures {
    /// Textures of `planes`' sizes (Y: R8, UV: R8G8), holding nothing yet
    /// (`resident` is the picture about to be written).
    pub fn new(
        device: &ID3D11Device,
        planes: &Planes,
        resident: Resident,
    ) -> Result<Self, GpuError> {
        let bind = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        let plane = |width, height, format| {
            texture(
                device,
                &desc(width, height, format, D3D11_USAGE_DEFAULT, bind, 0, 0),
            )
        };
        let luma = plane(planes.luma.width, planes.luma.height, DXGI_FORMAT_R8_UNORM)?;
        let chroma = plane(
            planes.chroma.width,
            planes.chroma.height,
            DXGI_FORMAT_R8G8_UNORM,
        )?;
        let luma_view = shader_view(device, &luma)?;
        let chroma_view = shader_view(device, &chroma)?;
        Ok(Self {
            resident,
            luma,
            chroma,
            luma_view,
            chroma_view,
        })
    }

    /// Write `picture`'s planes (`planes`, already checked whole) into the
    /// textures, row by row `planes.pitch` bytes apart.
    pub fn write(
        &mut self,
        context: &ID3D11DeviceContext,
        picture: &Nv12Picture<'_>,
        planes: &Planes,
    ) {
        let luma = picture.data[planes.luma.offset..].as_ptr();
        let chroma = picture.data[planes.chroma.offset..].as_ptr();
        // SAFETY: `planes` is `picture.planes()`, so the data holds both
        // planes whole: each texture reads its rows `pitch` bytes apart,
        // every row inside the slice.
        unsafe {
            context.UpdateSubresource(&self.luma, 0, None, luma.cast::<c_void>(), planes.pitch, 0);
            context.UpdateSubresource(
                &self.chroma,
                0,
                None,
                chroma.cast::<c_void>(),
                planes.pitch,
                0,
            );
        }
        self.resident = Resident::of(picture);
    }
}

/// A CPU-readable `width`×`height` BGRA copy (of the render target, or of a
/// sender's shared texture).
pub(super) fn staging(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, GpuError> {
    texture(
        device,
        &desc(
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
            0,
        ),
    )
}

/// Copy `source` (a `width`×`height` BGRA texture on `context`'s device: the
/// render target, or Spout's shared texture opened by a receiver) into
/// `staging` (of the same size) and read it: `width`×`height` BGRA rows,
/// tightly packed (the mapped rows may be padded). The map waits for the
/// copy.
pub(super) fn read_back(
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    staging: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GpuError> {
    let mut packed = Vec::new();
    unsafe { context.CopyResource(staging, source) };
    let rows = (width as usize * 4, height as usize);
    map_rows_into(context, staging, 0, rows, &mut packed, |hresult| {
        GpuError::from_hresult("Map", hresult)
    })?;
    Ok(packed)
}

/// Map `staging` (subresource 0, a texture of 4 bytes a texel: BGRA, or the
/// UYVY pass's RGBA) for reading with the map `flags`, put its
/// first `row` bytes of each of its `rows` rows into `out` (cleared once
/// the map succeeded, untouched on an error; the mapped rows may be
/// padded), and unmap it. A failed `Map` is `map_error` of its HRESULT
/// (the caller names it, #279 P1a: the UYVY readback's `DO_NOT_WAIT` map,
/// `uyvy::map_failed`). More rows or a longer row than the texture has, or
/// a mapping that does not hold the rows, is [`GpuError::NoObject`]: the
/// unsafe slice below never reaches past the mapping, whatever the caller
/// passes.
pub(super) fn map_rows_into(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    flags: u32,
    (row, rows): (usize, usize),
    out: &mut Vec<u8>,
    map_error: impl FnOnce(u32) -> GpuError,
) -> Result<(), GpuError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { staging.GetDesc(&mut desc) };
    let held = rows <= desc.Height as usize && row <= desc.Width as usize * 4;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, flags, Some(&mut mapped)) }
        .map_err(|e| map_error(e.code().0 as u32))?;
    let pitch = mapped.RowPitch as usize;
    let packed = match mapped_len(pitch, row, rows) {
        Some(len) if held && !mapped.pData.is_null() => {
            // SAFETY: the mapped staging texture holds its `desc.Height` ≥
            // `rows` rows of `desc.Width × 4` ≥ `row` bytes, `pitch` bytes
            // apart (`pitch` ≥ `row`, `mapped_len`), so its first `len`
            // bytes end inside its last asked row, inside the mapping,
            // which stays mapped until the Unmap below.
            let bytes =
                unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().cast_const(), len) };
            out.clear();
            unpad_rows_into(bytes, pitch, row, rows, out)
        }
        _ => None,
    };
    unsafe { context.Unmap(staging, 0) };
    packed.ok_or(GpuError::NoObject {
        call: "Map (the mapped staging rows)",
    })
}
