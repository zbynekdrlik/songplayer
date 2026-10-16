//! #279 P1a: the UYVY pass on the GPU (Windows glue): its shaders
//! (`uyvy.hlsl`, compiled at runtime), its half-width `R8G8B8A8_UNORM`
//! render target, its ring of staging textures, the draw and the readback
//! map. Every decision is the pure `crate::uyvy`: the conversion rows and
//! their constant buffer, the size rule, which staging texture a compose
//! writes and a readback maps, and the map's error. This module only calls
//! Direct3D.

use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11Query, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
use windows::core::s;

use super::failed;
use super::pipeline::{compile, created, query, raster};
use super::textures::{desc, map_rows_into, shader_view, texture};
use crate::error::GpuError;
use crate::targets::TargetId;
use crate::uyvy::{
    StagingRing, UYVY_CONSTANTS_BYTES, UYVY_STAGING, constants_bytes, map_failed, uyvy_size,
};

/// The pass's HLSL (`uyvy.hlsl`).
const SHADER: &str = include_str!("../uyvy.hlsl");

/// A staging slot the ring named that the pass does not hold: a bug,
/// reported instead of a panic.
const NO_SLOT: GpuError = GpuError::NoObject {
    call: "the UYVY staging slot",
};

/// The UYVY pass of one target: everything it draws with, what it draws
/// into, and the staging textures its readback maps.
pub(super) struct UyvyPass {
    /// The target it converts (a removed target takes its pass with it).
    pub target: TargetId,
    /// The pass's texture size: half the target's width, its height.
    width: u32,
    height: u32,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    raster: ID3D11RasterizerState,
    constants: ID3D11Buffer,
    /// The target's BGRA texture, read by the pixel shader.
    source: ID3D11ShaderResourceView,
    /// The `R8G8B8A8_UNORM` texture the pass draws: one texel per pair.
    pairs: ID3D11Texture2D,
    view: ID3D11RenderTargetView,
    /// [`UYVY_STAGING`] CPU-readable copies of `pairs`.
    staging: Vec<ID3D11Texture2D>,
    /// Ended after the staging copy: the GPU has passed it once the copy is
    /// done.
    pub done: ID3D11Query,
    pub ring: StagingRing,
}

impl UyvyPass {
    /// The pass of the target `target`, whose BGRA texture `source` is
    /// `size` (width, height). [`GpuError::UyvyOddWidth`] for an odd width,
    /// before anything is made.
    pub fn new(
        device: &ID3D11Device,
        target: TargetId,
        source: &ID3D11Texture2D,
        size: (u32, u32),
    ) -> Result<Self, GpuError> {
        let (width, height) = uyvy_size(size.0, size.1)?;
        let hlsl = (SHADER, s!("uyvy.hlsl"));
        let vertex_code = compile(hlsl, s!("vs_main"), s!("vs_5_0"), "UYVY vertex")?;
        let pixel_code = compile(hlsl, s!("ps_main"), s!("ps_5_0"), "UYVY pixel")?;
        let mut vertex: Option<ID3D11VertexShader> = None;
        unsafe { device.CreateVertexShader(&vertex_code, None, Some(&mut vertex)) }
            .map_err(|e| failed("CreateVertexShader (UYVY)", &e))?;
        let mut pixel: Option<ID3D11PixelShader> = None;
        unsafe { device.CreatePixelShader(&pixel_code, None, Some(&mut pixel)) }
            .map_err(|e| failed("CreatePixelShader (UYVY)", &e))?;
        let bind = D3D11_BIND_RENDER_TARGET.0 as u32;
        let pairs = texture(
            device,
            &desc(
                width,
                height,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D11_USAGE_DEFAULT,
                bind,
                0,
                0,
            ),
        )?;
        let mut view: Option<ID3D11RenderTargetView> = None;
        unsafe { device.CreateRenderTargetView(&pairs, None, Some(&mut view)) }
            .map_err(|e| failed("CreateRenderTargetView (UYVY)", &e))?;
        let read = D3D11_CPU_ACCESS_READ.0 as u32;
        let staging_desc = desc(
            width,
            height,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            read,
            0,
        );
        let staging = (0..UYVY_STAGING)
            .map(|_| texture(device, &staging_desc))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            target,
            width,
            height,
            vertex: created(vertex, "CreateVertexShader (UYVY)")?,
            pixel: created(pixel, "CreatePixelShader (UYVY)")?,
            raster: raster(device)?,
            constants: constants(device)?,
            source: shader_view(device, source)?,
            pairs,
            view: created(view, "CreateRenderTargetView (UYVY)")?,
            staging,
            done: query(device)?,
            ring: StagingRing::new(),
        })
    }

    /// Queue the pass and its copy into the staging texture the ring names,
    /// then end [`done`](Self::done): run after the targets' draws, before
    /// the compose's one flush and wait. The ring moves on only once that
    /// wait passed ([`StagingRing::written`], the compose's call).
    pub fn draw(&self, context: &ID3D11DeviceContext) -> Result<(), GpuError> {
        let staging = self.staging.get(self.ring.slot_to_write()).ok_or(NO_SLOT)?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: every object bound below lives in `self` for the whole
        // call. The pass's own render target is bound FIRST: that takes the
        // program target off the output stage, so its view can be the
        // shader's input (the runtime unbinds an input that is still bound
        // as an output). Both are unbound after the draw, so the next
        // compose draws into the program target and the copy reads a
        // texture no stage holds.
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(self.view.clone())]), None);
            context.OMSetBlendState(None, None, u32::MAX);
            context.RSSetViewports(Some(&[viewport]));
            context.RSSetState(&self.raster);
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(self.source.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
            context.CopyResource(staging, &self.pairs);
            context.End(&self.done);
        }
        Ok(())
    }

    /// The newest finished copy's rows into `out` (cleared once mapped):
    /// `width × 4` bytes a row (one UYVY pair per texel), `height` rows,
    /// packed. `Ok(false)` before any compose finished a copy; on
    /// `Ok(false)` or an error `out` keeps what it held.
    /// The map never waits (`D3D11_MAP_FLAG_DO_NOT_WAIT`): the compose
    /// already waited for the copy, so a copy still drawing is an error
    /// ([`map_failed`]).
    pub fn read(&self, context: &ID3D11DeviceContext, out: &mut Vec<u8>) -> Result<bool, GpuError> {
        let Some(slot) = self.ring.newest() else {
            return Ok(false);
        };
        let staging = self.staging.get(slot).ok_or(NO_SLOT)?;
        let rows = (self.width as usize * 4, self.height as usize);
        let flags = D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32;
        map_rows_into(context, staging, flags, rows, out, map_failed)?;
        Ok(true)
    }
}

/// The pass's constant buffer: [`constants_bytes`], made once (immutable).
fn constants(device: &ID3D11Device) -> Result<ID3D11Buffer, GpuError> {
    let bytes = constants_bytes();
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: UYVY_CONSTANTS_BYTES as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr().cast::<c_void>(),
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut buffer: Option<ID3D11Buffer> = None;
    // SAFETY: `data` points at `bytes`, `UYVY_CONSTANTS_BYTES` long, alive
    // for the whole call (the buffer copies them).
    unsafe { device.CreateBuffer(&desc, Some(&data), Some(&mut buffer)) }
        .map_err(|e| failed("CreateBuffer (UYVY constants)", &e))?;
    created(buffer, "CreateBuffer (UYVY constants)")
}
