---
paths:
  - "crates/sp-gpu/**"
---

# The `SP-program-MAX` GPU compositor: `crates/sp-gpu` (#223 S1a)

Design: #223 revision 3, R3-2 (comment 5979609879). `SP-program-MAX` is a
FIXED 3840×2160 canvas (owner, 3.10.2026: "4k", "both outputs static"). A 4K
fade costs ~40–45 ms on the CPU, over the 33.3 ms slot, so MAX is composed on
the GPU. This crate is the compositor: device, upload, draw, readback. S1b
adds the Spout send (to this crate), S2 the `program-max` thread and the
`MaxJob` hand-off (sp-server).

## What one boundary draws (`composition.rs`)

- `Composition::{Black, Picture, Fade { from, to, weight_q8 }}`. The weight
  is `SP-program`'s Q8 weight (0..=256, capped), incoming side.
- `layers()` gives the quads in draw order: the outgoing side (slot
  `Outgoing`) at `(256 − w)/256`, then the incoming one (slot `Incoming`) at
  `w/256`. A plain picture is one `Outgoing` quad at 1. A side of weight 0 is
  neither drawn nor uploaded.
- Placement: `sp_core::fit::aspect_fit` into 3840×2160. This is the same
  function `SP-program`'s canvas fit and the preview use. #223 S1a moved it
  from sp-server, and `playback::nv12_fit` re-exports it. Never copy the math.
- The draw:
  - clear to (0, 0, 0, 1);
  - each quad ADDS `saturate(rgb) · weight`: blend one/one, write mask RGB
    only, so alpha stays 255 (Arena reads alpha);
  - where a quad does not reach (its bars), it adds nothing.

  So a fade is `from·(1 − w) + to·w` with black bars, like the CPU fade.

## Colour, sampling and the CPU reference

- BT.709 limited → full: `color.rs` `BT709_LIMITED_TO_FULL`, three affine rows
  over the sampled UNORM `(y, u, v, 1)`. The rows reach the shader as f32 in
  the constant buffer (`quad.rs`). The CPU reference uses the same f32 values,
  so a matrix change is made ONCE. `color_tests.rs` recomputes the rows from
  Kr/Kb and pins BT.709's colour bars (each within ±1 of the ideal).
- The constant buffer (`QuadConstants::to_bytes`) is 5 float4 registers:
  `rect` (NDC), `to_r`, `to_g`, `to_b`, `weight`. `compose.hlsl`'s
  `cbuffer Quad` must match it register for register.
- Sampling is D3D11's bilinear (`MIN_MAG_MIP_LINEAR`, CLAMP). The texel
  coordinate is `u·size − ½`, the pixel-centre rule of
  `program_transition::tap`. Luma is sampled on its grid, the UV texture
  (R8G8, ⌈w/2⌉×⌈h/2⌉) on its own, both at every output pixel.
- `reference.rs` is the CPU model of exactly that. It quantizes the render
  target between draws, `c = unorm8(c/255 + rgb·w)`. The WARP pins compare
  with it, not with `nv12_mix` (which outputs NV12 at canvas resolution).
- **The ±1 tolerance and its precondition.** D3D11 guarantees only 8 bits of
  sub-texel filter-weight precision, and float → UNORM rounding may differ by
  0.6 ULP. So the test pictures must be SMOOTH: `tests/warp.rs` `pattern` =
  triangle waves, ≤ 12 luma codes per texel and ≤ 8 chroma codes per chroma
  texel. A sharp test edge (0 → 255 between texels) could miss by 2–3 codes
  on correct code. Do not "fix" that by widening the tolerance.

## Uploads (`residency.rs`, `win/textures.rs`)

- Y → R8 texture, UV → R8G8 texture, both `UpdateSubresource` with the
  picture's stride as the row pitch. The texture width is the picture width,
  so the stride's padding is never sampled.
- `upload_for(resident, picture)`:
  - `Skip` = same id and size on that slot;
  - `Write` = same size, new id;
  - `Create` = no textures yet, or another size.
- The id is the caller's: S2 must take it from a COUNTER, never from an
  `Arc` address (a freed buffer's address comes back, and a stale texture
  would be shown).
- `compose` checks every picture (`Nv12Picture::planes`, the rule of
  sp-server's `nv12_whole`) BEFORE any upload. A refused picture leaves the
  last frame.

## Device, render target, Spout facts

- `pick_adapter` takes the largest dedicated VRAM that is not software. It
  never takes the Basic Render Driver (0x1414/0x8C) or any adapter with 0
  VRAM (the box's virtual display adapters). `Compositor::new` never falls
  back to WARP: no candidate means `GpuError::NoAdapter`. `new_warp()` is for
  tests and CI.
- The render target is `B8G8R8A8_UNORM`, `BIND_RENDER_TARGET |
  BIND_SHADER_RESOURCE`, `D3D11_RESOURCE_MISC_SHARED`, NOT keyed.
- **Spout2 2.007.017 facts** (SpoutDX source, read for S1a):
  - `spoutDX::SendTexture(tex)` `CopyResource`s the caller's texture into the
    sender's OWN texture, made by `CreateSharedDX11Texture(bKeyed = false,
    bNThandle = false)`: `MISC_SHARED`, no keyed mutex;
  - access is synced by the named sender mutex (`IsKeyedMutex` is false);
  - so Spout needs no keyed mutex, and `SendTexture` does not even need our
    target to be shared;
  - it is shared anyway, with Spout's own description, so S1b may register
    its handle directly instead of copying.
- WARP supports shared resources since Windows 8 (D3D11_RESOURCE_MISC_FLAG
  docs). `tests/warp.rs` asserts the flag and a non-null `GetSharedHandle`,
  and it fails (never skips) if WARP refuses.
- Device lost: `GpuError::from_hresult` maps DXGI's
  `DEVICE_REMOVED/HUNG/RESET/DRIVER_INTERNAL_ERROR` to
  `GpuError::DeviceLost`. `compose` checks `GetDeviceRemovedReason` after
  every frame. S2 rebuilds the compositor on it and counts the rebuild.

## Telemetry (`ComposeStats`, for S2's `max.*_p99`)

- `upload_us`: the CPU time of this call's uploads, texture creation
  included.
- `draw_us`: from the first draw command until an event query says the GPU
  has finished the frame. The wait spins with `yield_now`, bounded at 10 s
  (`GpuError::Timeout`).
- `uploads`: 0, 1 or 2 pictures uploaded.

## HLSL at runtime

`compose.hlsl` (`include_str!`) is compiled by `D3DCompile`
(d3dcompiler_47.dll, which ships with Windows 10/11 and windows-latest) once
per `Compositor`, `vs_5_0` / `ps_5_0` at feature level 11.0+. No fxc/dxc
build step, and WARP and the RTX run the same source. A compile error is
`GpuError::Shader` with the compiler's log.

## Tier-0 and the gates

- `src/win/` is `#[cfg(windows)]` and excluded from the Linux mutation gate
  (`.cargo/mutants.toml`). Every decision lives in a pure, Linux-tested module:
  `adapter`, `picture`, `composition`, `quad`, `residency`, `color`,
  `reference`, `error`. Keep it that way: logic added inside `win/` is
  untested by the gate.
- The off-Windows `Compositor` (`stub.rs`) is an uninhabited enum. `new` /
  `new_warp` return `Unsupported`; its methods are `mutants::skip`, since no
  value exists to call them on.
- `windows` 0.58 signatures are read from the crate source (download it into
  the scratchpad, `rust-workspace.md`). The traps this crate met:
  - `D3D11_TEXTURE2D_DESC.BindFlags` / `CPUAccessFlags` / `MiscFlags`, and
    `D3D11_BUFFER_DESC.BindFlags`, are `u32`, but the constants are `i32`
    newtypes: write `D3D11_BIND_RENDER_TARGET.0 as u32`.
    `RenderTargetWriteMask` is a `u8`.
  - An interface argument takes `&Child` for a parent parameter
    (`&ID3D11Texture2D` where `ID3D11Resource` is asked, `&ID3D11Query` for
    `ID3D11Asynchronous`).
  - `None` for an optional interface or `HMODULE` parameter infers.
  - `GetData` maps `S_FALSE` to `Ok`: read the BOOL it writes.
- `tests/warp.rs` is `#![cfg(windows)]`. The `Build (Windows)` job runs it
  (`cargo test --workspace`). Linux compiles it to an empty test binary.
