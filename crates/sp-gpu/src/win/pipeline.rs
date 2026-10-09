//! The draw: the shaders (compiled at runtime), the fixed pipeline states,
//! one constant buffer, and the event query that tells when the GPU is done.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{BOOL, FALSE, TRUE};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_BLUE, D3D11_COLOR_WRITE_ENABLE_GREEN,
    D3D11_COLOR_WRITE_ENABLE_RED, D3D11_COMPARISON_NEVER, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11BlendState,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11Query,
    ID3D11RasterizerState, ID3D11SamplerState, ID3D11VertexShader,
};
use windows::core::{PCSTR, s};

use super::failed;
use super::textures::{PlaneTextures, RenderTarget};
use crate::error::GpuError;
use crate::quad::{QUAD_CONSTANTS_BYTES, QuadConstants};

/// The compositor's HLSL: one quad per picture (`compose.hlsl`).
const SHADER: &str = include_str!("../compose.hlsl");

/// How long [`Pipeline::wait_until_done`] waits for the GPU before it reports
/// [`GpuError::Timeout`]. A frame takes milliseconds; a hung GPU is reset by
/// Windows after 2 s (TDR) and then reports the device removed, so this only
/// bounds a driver that never answers.
const FRAME_WAIT_LIMIT: Duration = Duration::from_secs(10);

/// What the compositor draws with: created once per device.
pub(super) struct Pipeline {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    constants: ID3D11Buffer,
    done: ID3D11Query,
}

/// One quad to draw: its constants and the plane textures it samples.
pub(super) struct Quad<'t> {
    pub constants: QuadConstants,
    pub textures: &'t PlaneTextures,
}

/// The bytes of a compiled shader.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: a blob owns `GetBufferSize` bytes at `GetBufferPointer` for its
    // lifetime, which the returned slice borrows.
    unsafe {
        std::slice::from_raw_parts(
            blob.GetBufferPointer().cast::<u8>().cast_const(),
            blob.GetBufferSize(),
        )
    }
}

/// Compile one entry point of [`SHADER`] with `D3DCompile`
/// (d3dcompiler_47.dll, part of Windows): at runtime, once per device, so the
/// build needs no shader toolchain and WARP and the GPU run the same source.
fn compile(entry: PCSTR, target: PCSTR, stage: &'static str) -> Result<Vec<u8>, GpuError> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast::<c_void>(),
            SHADER.len(),
            s!("compose.hlsl"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let log = errors
        .as_ref()
        .map(|blob| {
            String::from_utf8_lossy(blob_bytes(blob))
                .trim_end_matches('\0')
                .to_string()
        })
        .unwrap_or_default();
    match (result, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code).to_vec()),
        (Err(e), _) => Err(GpuError::Shader {
            stage,
            log: format!("{log} (HRESULT {:#010x})", e.code().0 as u32),
        }),
        (Ok(()), None) => Err(GpuError::Shader {
            stage,
            log: "no bytecode returned".to_string(),
        }),
    }
}

/// Unwrap an out-parameter a successful create call must have set.
fn created<T>(value: Option<T>, call: &'static str) -> Result<T, GpuError> {
    value.ok_or(GpuError::NoObject { call })
}

impl Pipeline {
    pub fn new(device: &ID3D11Device) -> Result<Self, GpuError> {
        let vertex_code = compile(s!("vs_main"), s!("vs_5_0"), "vertex")?;
        let pixel_code = compile(s!("ps_main"), s!("ps_5_0"), "pixel")?;
        let mut vertex: Option<ID3D11VertexShader> = None;
        unsafe { device.CreateVertexShader(&vertex_code, None, Some(&mut vertex)) }
            .map_err(|e| failed("CreateVertexShader", &e))?;
        let mut pixel: Option<ID3D11PixelShader> = None;
        unsafe { device.CreatePixelShader(&pixel_code, None, Some(&mut pixel)) }
            .map_err(|e| failed("CreatePixelShader", &e))?;
        Ok(Self {
            vertex: created(vertex, "CreateVertexShader")?,
            pixel: created(pixel, "CreatePixelShader")?,
            sampler: sampler(device)?,
            blend: blend(device)?,
            raster: raster(device)?,
            constants: constants(device)?,
            done: query(device)?,
        })
    }

    /// Clear `target` to black (alpha 1), then draw each quad over it,
    /// adding its weighted RGB. The viewport is the whole target, whatever
    /// its size (#239: MAX's 3840×2160 or `SP-program`'s 1920×1080).
    pub fn draw(&self, context: &ID3D11DeviceContext, target: &RenderTarget, quads: &[Quad<'_>]) {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: target.width as f32,
            Height: target.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: every object bound below lives in `self` or `target` for
        // the whole call; the constant bytes outlive their UpdateSubresource.
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(target.view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.RSSetState(&self.raster);
            context.OMSetBlendState(&self.blend, None, u32::MAX);
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.ClearRenderTargetView(&target.view, &[0.0, 0.0, 0.0, 1.0]);
            for quad in quads {
                let bytes = quad.constants.to_bytes();
                context.UpdateSubresource(
                    &self.constants,
                    0,
                    None,
                    bytes.as_ptr().cast::<c_void>(),
                    0,
                    0,
                );
                context.PSSetShaderResources(
                    0,
                    Some(&[
                        Some(quad.textures.luma_view.clone()),
                        Some(quad.textures.chroma_view.clone()),
                    ]),
                );
                context.Draw(4, 0);
            }
            // Unbind the plane views, so the next upload writes textures no
            // stage holds.
            context.PSSetShaderResources(0, Some(&[None, None]));
        }
    }

    /// Flush the queued work and wait until the GPU has finished it (an
    /// event query), yielding the thread between polls.
    pub fn wait_until_done(&self, context: &ID3D11DeviceContext) -> Result<(), GpuError> {
        wait_until_done_on(context, &self.done)
    }
}

/// Flush the work queued on `context` and wait until the GPU has finished
/// it, through the event query `done`, yielding the thread between polls:
/// the compositor's frame wait, and the Spout sender's wait for its copy
/// into Spout's shared texture (#223 follow-up).
pub(super) fn wait_until_done_on(
    context: &ID3D11DeviceContext,
    done: &ID3D11Query,
) -> Result<(), GpuError> {
    let start = Instant::now();
    unsafe {
        context.End(done);
        context.Flush();
    }
    loop {
        let mut finished = FALSE;
        // SAFETY: an event query's data is one BOOL, written into
        // `finished` (S_FALSE leaves it FALSE: not done yet).
        unsafe {
            context.GetData(
                done,
                Some((&mut finished as *mut BOOL).cast::<c_void>()),
                std::mem::size_of::<BOOL>() as u32,
                0,
            )
        }
        .map_err(|e| failed("GetData", &e))?;
        if finished.as_bool() {
            return Ok(());
        }
        if start.elapsed() > FRAME_WAIT_LIMIT {
            return Err(GpuError::Timeout(FRAME_WAIT_LIMIT.as_millis() as u64));
        }
        std::thread::yield_now();
    }
}

/// Bilinear, clamped at the edges (the reference's `taps`), no mips.
fn sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState, GpuError> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: D3D11_FLOAT32_MAX,
    };
    let mut sampler: Option<ID3D11SamplerState> = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut sampler)) }
        .map_err(|e| failed("CreateSamplerState", &e))?;
    created(sampler, "CreateSamplerState")
}

/// Additive (one + one) into R, G and B only: alpha keeps the clear's 1.
fn blend(device: &ID3D11Device) -> Result<ID3D11BlendState, GpuError> {
    let write_rgb = D3D11_COLOR_WRITE_ENABLE_RED.0
        | D3D11_COLOR_WRITE_ENABLE_GREEN.0
        | D3D11_COLOR_WRITE_ENABLE_BLUE.0;
    let target = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: TRUE,
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_ONE,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: write_rgb as u8,
    };
    let desc = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: FALSE,
        IndependentBlendEnable: FALSE,
        RenderTarget: [target; 8],
    };
    let mut blend: Option<ID3D11BlendState> = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut blend)) }
        .map_err(|e| failed("CreateBlendState", &e))?;
    created(blend, "CreateBlendState")
}

/// Solid, no culling (the strip's winding does not matter), no scissor.
fn raster(device: &ID3D11Device) -> Result<ID3D11RasterizerState, GpuError> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: FALSE,
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: TRUE,
        ScissorEnable: FALSE,
        MultisampleEnable: FALSE,
        AntialiasedLineEnable: FALSE,
    };
    let mut raster: Option<ID3D11RasterizerState> = None;
    unsafe { device.CreateRasterizerState(&desc, Some(&mut raster)) }
        .map_err(|e| failed("CreateRasterizerState", &e))?;
    created(raster, "CreateRasterizerState")
}

/// The quad's constant buffer, written per quad with `UpdateSubresource`.
fn constants(device: &ID3D11Device) -> Result<ID3D11Buffer, GpuError> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: QUAD_CONSTANTS_BYTES as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer: Option<ID3D11Buffer> = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .map_err(|e| failed("CreateBuffer", &e))?;
    created(buffer, "CreateBuffer")
}

/// The event query that tells when the GPU has finished a frame.
pub(super) fn query(device: &ID3D11Device) -> Result<ID3D11Query, GpuError> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query: Option<ID3D11Query> = None;
    unsafe { device.CreateQuery(&desc, Some(&mut query)) }
        .map_err(|e| failed("CreateQuery", &e))?;
    created(query, "CreateQuery")
}
