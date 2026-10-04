---
paths:
  - "crates/sp-gpu/**"
---

# The `SP-program-MAX` GPU compositor and Spout sender: `crates/sp-gpu` (#223 S1a, S1b)

Design: #223 revision 3, R3-2 (comment 5979609879). `SP-program-MAX` is a
FIXED 3840×2160 canvas (owner, 3.10.2026: "4k", "both outputs static"). A 4K
fade costs ~40–45 ms on the CPU, over the 33.3 ms slot, so MAX is composed on
the GPU. This crate is the compositor (device, upload, draw, readback) and,
since S1b, its Spout sender (below); S2 adds the `program-max` thread and the
`MaxJob` hand-off (sp-server).

## What one boundary draws (`composition.rs`)

- `Composition::{Black, Picture, Fade { from, to, weight_q8 }}`. The weight
  is `SP-program`'s Q8 weight (0..=256, capped), incoming side, in the one
  unit `sp_core::blend::Q8_ONE` (sp-server's `program_transition::Q8_ONE`
  re-exports it).
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
    only, so alpha stays the clear's 255 — an opaque frame, whatever a Spout
    receiver does with alpha;
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
- **The tolerance: one code per quad covering the pixel**
  (`reference::tolerance`).
  - D3D11 guarantees only 8 bits of sub-texel filter-weight precision, and
    float → UNORM rounding may differ by 0.6 ULP. Each stored draw rounds
    once, so each layer may add one code.
  - So the bars are exact, a plain picture is ±1, and a fade's overlap is ±2.
    Review round 1 simulated 8-bit weights: 22 of the 50 % fade's pins were
    2 codes off, so a flat ±1 would fail on correct code.
  - The proof needs each layer's colour within 1 code of the model, so the
    test pictures must be SMOOTH: `tests/warp.rs` `pattern` = triangle waves,
    ≤ 12 luma codes per texel across, ≤ 5 down, ≤ 8 chroma codes per chroma
    texel (≤ ~0.2 code of filter error).
  - A sharp test edge (0 → 255 between texels) breaks it. Never widen the
    tolerance; keep the pictures smooth.

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
- `compose` checks every picture (`Nv12Picture::planes`) BEFORE any upload.
  A refused picture leaves the last frame. "Whole NV12" is
  `sp_core::nv12::{nv12_chroma_row, nv12_len}`, the two sizes sp-server's
  `program_transition::nv12_whole` checks too: one rule for the CPU and the
  GPU path. Both saturate at `usize::MAX`, so the check that guards the
  `unsafe` upload reads can never pass on a wrapped product.
- The slot is per side, as dispatched. At a fade's end the incoming picture
  is drawn as a plain picture from the `Outgoing` slot, so a held incoming
  picture is uploaded once more (one upload; a live song brings a new id
  every boundary anyway).

## Device, render target, Spout facts

- `pick_adapter` takes the largest dedicated VRAM that is not software. It
  never takes the Basic Render Driver (0x1414/0x8C) or any adapter with 0
  VRAM (the box's virtual display adapters). `Compositor::new` never falls
  back to WARP: no candidate means `GpuError::NoAdapter`. `new_warp()` is for
  tests and CI. `new_on_listed_adapter(i)` (doc-hidden) runs `new`'s
  explicit-adapter path on DXGI's adapter `i`, so CI (whose only adapter is
  the Basic Render Driver) exercises it. The compositor's `adapter()` is
  read back from the DEVICE (`IDXGIDevice::GetAdapter`), never copied from
  the list, so a test comparing it with the list proves where the device
  landed.
- The render target is `B8G8R8A8_UNORM`, `BIND_RENDER_TARGET |
  BIND_SHADER_RESOURCE`, `D3D11_RESOURCE_MISC_SHARED`, NOT keyed.
- **Spout2 2.007.017 facts** (SpoutDX source, read for S1a):
  - `spoutDX::SendTexture(tex)` `CopyResource`s the caller's texture into the
    sender's OWN texture, made by `CreateSharedDX11Texture(bKeyed = false,
    bNThandle = false)`: `MISC_SHARED`, no keyed mutex;
  - access is synced by the named sender mutex (`IsKeyedMutex` is false);
  - so Spout needs no keyed mutex, and `SendTexture` does not even need our
    target to be shared;
  - it is shared anyway, with Spout's own description. S1b SENDS it
    (`SendTexture`, a GPU copy) rather than registering its handle: Spout's
    own texture is guarded by the sender mutex a receiver takes, so Arena
    never reads a frame the compositor is drawing.
- WARP supports shared resources since Windows 8 (D3D11_RESOURCE_MISC_FLAG
  docs). `tests/warp.rs` asserts the flag and a non-null `GetSharedHandle`,
  and it fails (never skips) if WARP refuses.
- A create call that succeeds with no object, or an empty slot at draw
  time, is `GpuError::NoObject` (never an `Api` error with HRESULT 0).
- Device lost: `GpuError::from_hresult` maps DXGI's
  `DEVICE_REMOVED/HUNG/RESET/DRIVER_INTERNAL_ERROR` to
  `GpuError::DeviceLost`. `compose` checks `GetDeviceRemovedReason` after
  every frame, and ANY reason it reports is `DeviceLost`
  (`GpuError::removed`): `DXGI_ERROR_INVALID_CALL` there means the app's
  own bad call removed the device. S2 rebuilds the compositor on it and
  counts the rebuild.
- `Nv12Picture`'s `Debug` prints its id, size and byte count, never its
  bytes (a 1440p picture is 5.5 MB in a log line otherwise).

## The Spout sender (#223 S1b)

Design: R3-2 "Transport" and revision 2's D7 "Build" (5872871751); the S1b
anchors are on #223 (comment 5984577044).

### The vendored SDK (`vendor/spout2`)

- Spout2 **2.007.017** (BSD-2, https://github.com/leadedge/Spout2, tag
  `2.007.017`), exactly upstream's `SpoutDX_static` source list, flat and
  UNMODIFIED, with its `LICENSE`. `vendor/spout2/README.md` lists the files,
  the flags and the bump steps.
- `build.rs` (`cc`, MSVC): `/std:c++17`, `/EHsc` (cc sets no exception
  model for MSVC, and the SDK uses try/catch), `SPOUT_BUILD_STATIC`,
  `NDEBUG`, no `UNICODE` (upstream's own builds), warnings off, and every
  system library named (`user32` has no `#pragma comment` in the SDK).
- **Bumping it:** read the new tag's `SpoutDX_SOURCES`, copy the files over
  unmodified, and re-read the facts below in the new source (they are what
  the shim and `src/spout.rs` rely on); the WARP tests are the gate.
- `SpoutUtils.h` carries `#pragma comment(linker,
  "/manifestdependency:…Common-Controls 6.0…")`. With no `/MANIFEST:EMBED`
  link.exe writes a side-by-side `.manifest` FILE and never embeds a second
  manifest. S2 checks the shipped exe still carries only Tauri's.
- BSD-2 asks for the notice in the documentation of a binary distribution:
  when S2 links sp-gpu into the app, the installer/about must carry
  `vendor/spout2/LICENSE`.

### The sender

- Name: `SPOUT_SENDER_NAME` = **`SP-program-MAX`**. Resolume Arena 7.28 lists
  a Spout 2.007.017 sender as category "Spout Servers", idstring
  **`SPOUT_SP-program-MAX`** (M0, 5980720789).
- `SpoutSender::new(&Compositor)` → `send()` after each `compose()`, on the
  same thread (both use the device's immediate context) →
  `SpoutSendStats { send_us }`. `with_name` is doc-hidden, for tests.
- `SendTexture` copies the render target into Spout's OWN shared texture
  (`CreateSharedDX11Texture`: `MISC_SHARED`, not keyed, a legacy handle)
  under the named mutex `<name>_SpoutAccessMutex`, then `Flush`es. If a
  receiver holds that mutex over 67 ms Spout SKIPS the copy and still
  returns true: nothing tells the caller, `send_us` shows the wait.
- Registration is lazy: Spout lists the sender at its FIRST send (it needs
  the texture's size and format). Before that `spout_sender_info` is `None`.
- `spoutDX::OpenDirectX11(device)` keeps the device pointer WITHOUT AddRef
  (and AddRefs the immediate context). So `SpoutSender` holds its own clones
  of the device and the render target and releases the shim first in
  `Drop`. It may outlive the `Compositor` value; S2 drops both on a lost
  device.
- **A second sender with the same name is REFUSED** (Spout would rename it
  `SP-program-MAX_1`, which Arena's layer never shows):
  - `new` refuses a name that a live sender has listed
    (`GpuError::SpoutNameTaken`), after Spout's own `CleanSenders` drops
    names whose sender crashed (their info map is gone);
  - the first `send` refuses when Spout registered another name (a sender
    took ours in between) or did not list it (Spout's list is full, 64 by
    default): `GpuError::SpoutNotRegistered`; the shim releases that
    registration at once and the sender never sends again: drop it.
- Spout's own race: a sender registers its name, then creates its info map;
  another program's `CleanSenders` in that window drops the name. Our first
  send then reports `SpoutNotRegistered` (the shim checks the list), and S2
  drops and recreates the sender. `tests/spout.rs` takes one lock so its
  tests never race each other.
- Names: 1..=239 bytes of printable ASCII (`check_sender_name`, and the
  shim checks the length again). Spout builds `<name>_Count_Semaphore` in
  256 bytes with `sprintf_s`, which ABORTS the process on overflow.
- `Drop` deletes the `spoutDX`: `ReleaseSender` takes the name off the list
  and closes its info map, `CloseDirectX11` flushes and releases its
  context reference.
- After every send, `GetDeviceRemovedReason`: a lost device is
  `GpuError::DeviceLost`, as in `compose`.
- Every shim entry point catches every C++ exception (`GpuError::Spout`,
  code 4): nothing unwinds into Rust.

### Spout's registry (`spout_sender_names`, `spout_sender_info`)

Read as a receiver does: open the named map, take its named mutex
`<map>_mutex` (`WaitForSingleObject`, 67 ms, as `SpoutSharedMemory::Lock`),
copy, release. A missing map (`HRESULT 0x80070002`) is "absent", never an
error.

- `SpoutSenderNames`: MaxSenders × 256 bytes (64 by default, registry
  `MaxSenders`). One NUL-terminated name per slot. The list ends at a slot
  whose first byte is 0 or ≥ 0x80 (Spout tests a signed `char` `> 0`), or a
  slot with no NUL (its `strncpy_s` refuses it).
- A sender's own map, named after it: `SharedTextureInfo`, 280 bytes LE:
  `shareHandle, width, height, format, usage` (u32 each), `description[256]`
  (the sending exe's path), `partnerId`. On x64 a receiver opens
  `LongToHandle((long)shareHandle)`: the 32 bits SIGN-EXTENDED
  (`share_handle_value`).

### WARP proof (`tests/spout.rs`, Windows only)

- the first send lists `SP-program-MAX`, its map says 3840×2160, format 87
  (`B8G8R8A8_UNORM`), usage 0, a non-zero handle and this test's exe;
  `size()` agrees; `Drop` unlists it and its map is gone;
- the handle opens on a SECOND WARP device (`OpenSharedResource`, as Arena
  opens it) and reads back byte for byte the compositor's own readback, for
  two frames, plus S1a's reference tolerance at sample points (two blank
  frames cannot pass). Sync: `compose` waits for the GPU, `send` queues the
  copy, and the compositor's `read_back` `Map` waits for it, so the second
  device reads a finished copy;
- a listed name is refused at create, and no `_1` sender appears;
- a sender that loses its name before its first send is refused, twice,
  with no `_1` left, and the winner stays listed;
- a name Spout cannot carry never reaches Spout; a missing map is `None`.

WARP has supported shared resources since Windows 8 (the
D3D11_RESOURCE_MISC_FLAG docs); the second-device test FAILS with the
HRESULT if it ever refuses, it never skips.

### The box checks S2 runs (win-resolume)

- `spout_sender_info("SP-program-MAX")` after the first send: 3840×2160,
  format 87, the host path = the installed `songplayer.exe`;
- Arena `GET /api/v1/sources` lists `SPOUT_SP-program-MAX` (category "Spout
  Servers"); on a scratch layer (Bridge.avc saved and restored) it shows the
  program at 30 frames/s, with Arena's FPS held and its CPU change measured
  (R3-2 S2 gate; M0 moved this measurement to S2);
- `max.send_us_p99` and `SP-program` / VBAN timing unchanged (health deltas
  0);
- the exe carries only Tauri's manifest (the SDK's manifestdependency
  pragma), and the installer carries the Spout2 BSD-2 notice.

## Telemetry (`ComposeStats`, for S2's `max.*_p99`)

- `upload_us`: the CPU time of this call's uploads, texture creation
  included.
- `draw_us`: from the first draw command until an event query says the GPU
  has finished the frame. The wait spins with `yield_now` (SwitchToThread:
  any ready thread runs first), bounded at 10 s (`GpuError::Timeout`). A 4K
  quad on the RTX is well under a millisecond. If S2 measures the spin as a
  cost, it can read the query a boundary later instead: Spout's copy is
  ordered after the draw on the same context and needs no wait.
- `uploads`: 0, 1 or 2 pictures uploaded.
- `SpoutSendStats::send_us` (S1b, for `max.send_us_p99`): the CPU time of
  `SendTexture`: the sender-mutex wait, the queued copy and the flush (at
  the first send also the shared texture's creation and the registration).

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
  `reference`, `error`, `readback` (the mapped rows packed), `spout` (the
  sender name rule, the registry parsers, the shim's codes). Keep it that
  way: logic added inside `win/` is untested by the gate.
- The off-Windows `Compositor` (`stub.rs`) is an uninhabited enum. `new` /
  `new_warp` return `Unsupported`; its methods are `mutants::skip`, since no
  value exists to call them on. The off-Windows `SpoutSender` is one too
  (its `new` takes a `&Compositor`, which cannot exist); the registry
  readers return `Unsupported`.
- The C++ (`vendor/spout2`, `src/win/spout_shim.cpp`) is compiled only by
  the `Build (Windows)` job: `build.rs` returns at once for a non-Windows
  `CARGO_CFG_TARGET_OS`. cargo-mutants never mutates `build.rs` (it
  mutates lib/bin targets only).
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
