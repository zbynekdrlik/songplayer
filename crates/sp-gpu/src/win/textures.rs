//! The textures: the shared 3840×2160 BGRA render target, a slot's two
//! plane textures (Y as R8, UV as R8G8), and the staging copy for readback.

use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_SAMPLE_DESC,
};

use super::failed;
use crate::composition::{CANVAS_HEIGHT, CANVAS_WIDTH};
use crate::error::GpuError;
use crate::picture::{Nv12Picture, Planes};
use crate::residency::Resident;

/// A one-mip, one-sample 2D texture's description.
fn desc(
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

/// `CreateTexture2D` with no initial data.
fn texture(
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

/// A shader view of the whole texture.
fn shader_view(
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

/// The 3840×2160 `B8G8R8A8_UNORM` render target. Created
/// `D3D11_RESOURCE_MISC_SHARED`, not keyed: the description Spout2's own
/// sender texture has (`spoutDirectX::CreateSharedDX11Texture`, keyed =
/// false), so S1b may hand its handle to Spout or `SendTexture` it.
pub(super) struct RenderTarget {
    pub texture: ID3D11Texture2D,
    pub view: ID3D11RenderTargetView,
}

impl RenderTarget {
    pub fn new(device: &ID3D11Device) -> Result<Self, GpuError> {
        let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        let texture = texture(
            device,
            &desc(
                CANVAS_WIDTH,
                CANVAS_HEIGHT,
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
        Ok(Self { texture, view })
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

/// The CPU-readable copy of the render target.
pub(super) fn staging(device: &ID3D11Device) -> Result<ID3D11Texture2D, GpuError> {
    texture(
        device,
        &desc(
            CANVAS_WIDTH,
            CANVAS_HEIGHT,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
            0,
        ),
    )
}

/// Copy the render target into `staging` and read it: 3840×2160 BGRA rows,
/// tightly packed (the mapped rows may be padded).
pub(super) fn read_back(
    context: &ID3D11DeviceContext,
    target: &RenderTarget,
    staging: &ID3D11Texture2D,
) -> Result<Vec<u8>, GpuError> {
    let row = CANVAS_WIDTH as usize * 4;
    let rows = CANVAS_HEIGHT as usize;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.CopyResource(staging, &target.texture);
        context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    }
    .map_err(|e| failed("Map", &e))?;
    let pitch = mapped.RowPitch as usize;
    let mut out = Vec::with_capacity(row * rows);
    if !mapped.pData.is_null() && pitch >= row {
        let base = mapped.pData.cast::<u8>().cast_const();
        for y in 0..rows {
            // SAFETY: the mapped staging texture is `rows` rows of `pitch`
            // bytes, each holding `row` bytes of pixels.
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(base.add(y * pitch), row) });
        }
    }
    unsafe { context.Unmap(staging, 0) };
    if out.len() == row * rows {
        Ok(out)
    } else {
        Err(GpuError::NoObject {
            call: "Map (no readable rows)",
        })
    }
}
