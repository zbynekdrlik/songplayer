---
paths:
  - "crates/sp-gpu/**"
  - "crates/sp-core/src/fit*.rs"
  - "crates/sp-server/src/playback/program_max*.rs"
  - "crates/sp-server/src/playback/program_output_tests_max.rs"
  - "crates/sp-server/src/api/program_tests_max.rs"
  - "crates/sp-server/src/playback/ndi_feed*.rs"
  - "crates/sp-server/src/playback/program_output_ndi*.rs"
  - "crates/sp-server/src/playback/program_output_tests_ndi*.rs"
  - "crates/sp-server/src/api/program_tests_ndi.rs"
  - "crates/sp-server/src/playback/picture*.rs"
  - "crates/sp-server/src/playback/pacer_tests_gpu.rs"
  - "src-tauri/resources/THIRD-PARTY-NOTICES.txt"
  - "e2e/post-deploy-max.spec.ts"
  - "e2e/max-gate*.ts"
  - "e2e/settings-spout.spec.ts"
---

# The `SP-program-MAX` GPU compositor and Spout sender: `crates/sp-gpu` (#223 S1a, S1b) and its runtime wiring (S2)

Design: #223 revision 3, R3-2 (comment 5979609879). `SP-program-MAX` is a
FIXED 3840×2160 canvas (owner, 3.10.2026: "4k", "both outputs static"). A 4K
fade costs ~40–45 ms on the CPU, over the 33.3 ms slot, so MAX is composed on
the GPU. This crate is the compositor (device, upload, draw, readback) and,
since S1b, its Spout sender (below); S2 wires both into the program output
(sp-server `playback/program_max.rs` + `program_max_worker.rs`, "Runtime
wiring" below). #239: the same thread also sends the FHD program as the
Spout sender `SP-program` (1920×1080), "The FHD Spout sender" below; #251
step 2: from a SECOND TARGET of the same compositor (one device, one upload
of each picture, both targets drawn at the compose phase). #279 P1b: the
FHD program is that compositor's PRIMARY target and MAX's the ADDED one, and
the FHD target also feeds NDI `SP-program` (a UYVY readback, one slot late:
"NDI `SP-program` from the GPU readback" below). #279 P2: the picture
ring, `GpuFrame` and a composition's ring-frame sides ("The picture ring"
below).

## What one boundary draws (`composition.rs`)

- `Composition::{Black, Picture, Fade { from, to, weight_q8 }}`. The weight
  is `SP-program`'s Q8 weight (0..=256, capped), incoming side, in the one
  unit `sp_core::blend::Q8_ONE` (sp-server's `program_transition::Q8_ONE`
  re-exports it).
- `layers()` gives the quads in draw order: the outgoing side (slot
  `Outgoing`) at `(256 − w)/256`, then the incoming one (slot `Incoming`) at
  `w/256`. A plain picture is one `Outgoing` quad at 1. A side of weight 0 is
  neither drawn nor uploaded.
- **Placement: each target's rule, named ONCE** (#267, design: #267
  comment 6149987923). `composition.rs` has `Fit::{Aspect, FillWidth}`,
  `MAX_FIT = FillWidth` and `FHD_FIT = Aspect`. `Fit::place` returns
  `sp_core::fit::Fill { place, scaled_h, crop_top }`.
  - **MAX fills the wall's width.** MAX is the LED wall's feed: Arena's
    clip scaling Fill scales it to the composition's width.
    `sp_core::fit::fill_width` scales EVERY picture to the canvas's whole
    width, `scaled_h = ⌊sh·3840/sw⌋` (even, ≥ 2, capped at `u32::MAX`),
    and centres it.
  - A picture narrower than 16:9 covers the whole canvas, and its rows past
    2160 are cut off top and bottom. The top count is floored to even, so
    the two ends differ by at most 2 rows. PP's `MO3ucp2mBnM` (2880×2160,
    4:3) scales to 2880 rows with 360 cut off each end; 1:1 and 9:16 work
    the same way.
  - Such a picture is NEVER pillarboxed. Before #267 the bars were part of
    MAX's picture, so Arena's Fill put them on both sides of the wall, a
    regression since #223.
  - A 16:9 or wider picture gets EXACTLY `aspect_fit`'s letterbox. It is
    the same formula, with no branch on the aspect: a `<` / `<=` branch
    would have an equivalent mutant at exactly 16:9.
    `fit_tests.rs::fill_width_is_aspect_fit_unless_the_picture_is_narrower`
    sweeps the claim.
  - **`SP-program` (FHD, #239) keeps `sp_core::fit::aspect_fit`.** It
    carries the NDI `SP-program`'s picture, bars included, so a local Spout
    consumer gets the same program. `aspect_fit` is also the function
    `SP-program`'s CPU canvas fit and the preview use; #223 S1a moved it
    from sp-server, and `playback::nv12_fit` re-exports it. Never copy the
    math, and never change `aspect_fit` for MAX's sake.
  - **A crop lives in the texture coordinates, never in a quad past the
    target.** `Layer::place` (the part of the target the quad covers)
    stays inside the target.
  - `Layer::uv_rect` = `quad::uv_rect(fill)` =
    `[0, crop_top/scaled_h, 1, (crop_top + h)/scaled_h]`, as f32. A whole
    picture gives exactly `[0, 0, 1, 1]`; a degenerate one gives
    `[0, 0, 1, 0]`, and its quad covers nothing.
  - Each corner of the vertex shader takes `uv_rect`'s edge on its side,
    from the 6th cbuffer register. `reference.rs` interpolates the SAME
    f32 values (`between`), so the shader and the reference follow one
    rule.
  - For `[0, 0, 1, 1]` the reference is bit-identical to the old
    `(dx + ½)/w`. The tolerance argument below is unchanged: the crop only
    moves where an output pixel samples, through an f32 attribute with
    about 1e-4 texel of error.
  - **Rejected:** a quad that extends past the target, or a scissor. Either
    needs a signed `Placement` through the layers, the reference's coverage
    and the WARP edge points.
- #239 / #267 / #251 step 2: a TARGET is a size and its rule
  (`Target { width, height, fit }`, `MAX_TARGET` = 3840×2160 `MAX_FIT`,
  `FHD_TARGET` = 1920×1080 `FHD_FIT`), fixed for the target's life.
  - `Compositor::new()` / `new_warp()` / `new_on_listed_adapter(i)` build
    a compositor whose PRIMARY target is MAX's; `with_size(w, h, fit)` /
    `new_warp_with_size(w, h, fit)` name another primary (tests). The
    primary is the compositor's for its life (`TargetId::PRIMARY`).
  - #251 step 2: `add_target(target) -> Result<TargetId, GpuError>` adds a
    target (a shared BGRA render target + its own event query) drawn by
    every later compose; `remove_target(id)` stops it (never the
    primary). sp-server's production compositor's primary is
    `FHD_TARGET` and it adds `MAX_TARGET` while MAX is on (#279 P1b; #251
    step 2 added `FHD_TARGET` to MAX's). The ids and
    the draw order (primary first, then as added; an id never reused) are
    the pure `targets.rs` `Targets<T>`, which `win/mod.rs` holds its
    targets in.
  - `layers()` is `layers_on(MAX_TARGET)`; `layers_on(target)` =
    `layers_in(w, h, fit)`, which places into any target.
  - `pictures()` (test-only since #279 P2) = the (slot, side) pairs one
    compose draws, in draw
    order, whatever the targets; every target's layers sample exactly
    those (`composition_tests.rs::every_target_draws_the_pictures_one_compose_uploads`).
    #279 P2: of those, `uploads()` (the CPU sides) is what one compose
    uploads, and `frames()` (the ring sides, each slot once) what it opens
    and acquires.
  - The target size also drives `QuadConstants::new(place, uv_rect,
    weight, w, h)`, the viewport, the render target, the staging copy and
    the readback.
  - `reference.rs` needs no size: each layer carries its place and its crop
    in its own target. Its pins run at both sizes; a scratch model of
    `pixel` that reproduces the 4K pins derived the FHD ones, and the #267
    crop pins too.
- #267 pins:
  - sp-core `fit_tests.rs`: 4:3, 1:1, 9:16, 16:9, 21:9, odd, tiny, zero,
    and the u32 cap.
  - sp-gpu `composition_tests.rs`: MAX fills, the FHD target keeps its
    bars, and a fade places each side by the rule. `quad_tests.rs`:
    `uv_rect`, the six registers, and the HLSL cbuffer's order read from
    the source. `reference_tests.rs`: a cropped quad (each axis), and a
    1:1 picture filling MAX.
  - WARP `tests/warp.rs`: a 4:3 and a 9:16 picture fill MAX's width.
    `assert_edge_columns_lit` checks that every pixel of both edge columns
    is lit and within the reference's tolerance; the scratch model puts
    the reference ≥ 3 codes from black there. The 50 % fade covers the
    canvas with both sides. The 21:9 letterbox and the FHD 4:3 bars are
    unchanged.
  - `tests/spout.rs`: the MAX second-device test reads its bar at (0, 0)
    off a 21:9 picture.
  - sp-server `program_max_tests_warp.rs`: a 4:3 boundary through the real
    loop.
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
- The constant buffer (`QuadConstants::to_bytes`) is 6 float4 registers
  (96 bytes): `rect` (NDC), `uv_rect` (texture coordinates, #267), `to_r`,
  `to_g`, `to_b`, `weight`. `compose.hlsl`'s `cbuffer Quad` must match it
  register for register. The shader compiles only on Windows, so
  `quad_tests.rs::the_hlsl_cbuffer_has_the_registers_in_the_same_order`
  reads the HLSL source on Linux: the declarations in this order, and each
  corner's `uv` from `uv_rect`.
- Sampling is D3D11's bilinear (`MIN_MAG_MIP_LINEAR`, CLAMP). The texel
  coordinate is `u·size − ½`, the pixel-centre rule of
  `program_transition::tap`, with u interpolated across the quad from its
  `uv_rect` edges. Luma is sampled on its grid, the UV texture (R8G8,
  ⌈w/2⌉×⌈h/2⌉) on its own, both at every output pixel.
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
- The slot is per side, as dispatched. Before its uploads `compose` lets the
  two slots TRADE PLACES when that saves uploads (#251, pure
  `residency::swap_slots(resident ids, wanted ids)`: fewer uploads after the
  trade), so a picture resident in the other slot is never uploaded again:
  a fade's incoming picture is the next fade's outgoing one (every boundary
  of a 24/25 fps song is such a fade, #251's blend), and a fade's end draws
  its incoming picture alone from `Outgoing`. Without the trade every
  picture of a blended song went up twice (~60 4K uploads/s instead of 25).
  WARP pin: `tests/warp.rs::a_chain_of_fades_uploads_each_picture_once`.
- **Uploads are what disturbs Arena** (#251 comment 6212436810, SNV
  16.10.2026). An INDEPENDENT Spout sender was shown on the wall, and
  SongPlayer's program work alone was varied:
  - SongPlayer blending a 25 fps clip: 56-86 jumps back per 40 s;
  - one picture per boundary: 20-31;
  - Blank (MAX still composing and sending black): 0-3;
  - MAX off: 5-14.

  the GPU scheduling priority class (D3DKMT), HAGS,
  Arena's CPU priority or cores and NVIDIA threaded optimization changed
  nothing. An upload is a CPU copy into driver memory plus a PCIe DMA,
  which is memory bandwidth, like the synthetic `paced_burner.py`. Every
  upload saved is measurable on the wall. The end state is GPU decode
  straight into the compositor's textures, with no upload at all.
- #251 step 2: the slots belong to the COMPOSITOR, not to a target.
  `compose` checks and uploads `uploads()` once (#279 P2: and holds the
  ring slots of `frames()`), then draws every target
  from those textures (each through its own viewport, `QuadConstants` and
  render target), ends each target's event query after its draw, flushes
  ONCE and waits for them in order (`pipeline::wait_until_each_done`).
  `ComposeStats::draw_us` covers every target (#279 P1a: and a UYVY pass
  with its copy); `draw_us(id)` is a target's
  OWN part (from the end of the target before, the first draw command for
  the first, until its query passed: the pure `stats::target_draws_us`).
  WARP pins: `tests/warp.rs` "one compose fills both targets", "each
  picture is uploaded once for both targets" (the same `uploads` as MAX
  alone), "a 4:3 picture fills MAX's width and keeps FHD's bars in one
  compose", "a removed target is no longer drawn".

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
- #223 S3b: `VideoDevice` (`win/video_device.rs`) is the device Media
  Foundation decodes video on (sp-decoder's hardware decode,
  `video-decode.md`): the SAME `pick_adapter` rule, made by `device.rs` with
  `DeviceUse::VideoDecode` (`BGRA | D3D11_CREATE_DEVICE_VIDEO_SUPPORT`; the
  compositor's `DeviceUse::Compose` is BGRA only) and multithread-protected
  (`ID3D11Multithread`). One per opened file, so its adapter list logs at
  DEBUG. `new()` never falls back to WARP; `new_warp()` /
  `new_on_listed_adapter(i)` are doc-hidden, for CI
  (`tests/video_device.rs`). On `windows-latest` WARP REFUSES the video
  flag at feature level 11.1 / 11.0 (DXGI_ERROR_UNSUPPORTED, though
  Microsoft's page reads as if WARP takes it), so `new_warp()` returns
  that error there; the test asserts it (`video-decode.md`).
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
    (a GPU copy; #251 step 3: the shim's `SendSynced`, not `SendTexture`) rather than registering its handle: Spout's
    own texture is guarded by the sender mutex a receiver takes, so Arena
    never reads a frame the compositor is drawing (#251 step 3: nor one
    Spout's copy is still writing, below).
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
  `NDEBUG`, no `UNICODE` (upstream's own builds), and every system library
  named (`user32` has no `#pragma comment` in the SDK). Two libraries: the
  shim (`sp_spout_shim`, cc's `warnings(true)` + `warnings_into_errors`:
  `-W4 -WX`; it wraps the SDK's headers in `#pragma warning(push, 0)`)
  linked before the SDK (`sp_spout_sdk`, `-W0`: not ours to fix). cl prints
  warnings on STDOUT, which cargo shows only for a failed build, so `-WX`
  is the only way a shim warning is ever seen; a `-W4` flag after cc's
  `-W0` would only earn D9025.
- **Bumping it:** read the new tag's `SpoutDX_SOURCES`, copy the files over
  unmodified, and re-read the facts below in the new source (they are what
  the shim and `src/spout.rs` rely on); the WARP tests are the gate.
- **Common-Controls 6.0 is a hard requirement.** `SpoutUtils.cpp` imports
  `TaskDialogIndirect` from COMCTL32 statically. Only comctl32 v6 exports
  it, and Windows binds v6 only for an exe whose manifest asks for it;
  otherwise the exe does not start (STATUS_ENTRYPOINT_NOT_FOUND,
  0xC0000139). Every SDK object carries `#pragma comment(linker,
  "/manifestdependency:…Common-Controls 6.0…")` (`SpoutUtils.h`). rustc
  passes no `/MANIFEST` flag, so link.exe's default writes a side-by-side
  `<exe>.manifest` FILE with that dependency: that is what lets the test
  exes start. It never embeds a second manifest. The Tauri app embeds its
  own, which wins over the side file: `src-tauri/build.rs` calls
  `tauri_build::build()` with default attributes, and tauri-build 2.x
  `WindowsAttributes::new()` embeds `windows-app-manifest.xml` (resource
  #1, RT_MANIFEST), which depends on `Microsoft.Windows.Common-Controls`
  6.0.0.0 (read in tauri-build 2.5.6 and 2.7.0). Since S2 SongPlayer.exe
  links sp-gpu, so the CI step "Check SongPlayer.exe declares
  Common-Controls 6.0" (`ci.yml`, Build Tauri job) reads the built exe's
  embedded manifest back with the Windows SDK's `mt.exe
  -inputresource:<exe>;#1` and fails without the declaration: a build.rs
  or tauri-build change can never ship an exe that does not start.
- BSD-2 asks for the notice in the documentation of a binary distribution.
  Since S2 the installer ships `src-tauri/resources/THIRD-PARTY-NOTICES.txt`
  (`tauri.conf.json` `bundle.resources`), which holds
  `vendor/spout2/LICENSE` verbatim; `tests/notice.rs` (every platform)
  fails if it differs from the vendored LICENSE or is not bundled. A
  Spout2 bump re-copies the LICENSE into it. The same file points at
  mimalloc's MIT license (#168's override DLLs), which the Build Tauri job
  stages next to them as `resources/mimalloc/LICENSE-mimalloc.txt` (CI-built,
  never committed; `tests/notice.rs` pins the staging line).

### The sender

- Name: `SPOUT_SENDER_NAME` = **`SP-program-MAX`**. Resolume Arena 7.28 lists
  a Spout 2.007.017 sender as category "Spout Servers", idstring
  **`SPOUT_SP-program-MAX`** (M0, 5980720789).
- `SpoutSender::new(&Compositor)` → `send()` after each `compose()`, on the
  same thread → `SpoutSendStats { send_us }`. `with_name` is doc-hidden, for
  tests. `registration()` says where it is (`spout_state::Registration`).
  #251 step 2: `for_target(&c, target, name)` sends a given target (its
  own reference to that texture; an id the compositor does not hold is
  `GpuError::NoObject` before Spout); `new` = the primary,
  `new_fhd(&c, target)` = `SP-program` of the FHD target. Two senders on
  one compositor share its immediate context and each waits for its own
  copy (`tests/spout.rs::two_senders_on_one_compositor_are_listed_with_their_own_sizes`).
- **Neither the sender nor the `Compositor` is `Send` (nor `Sync`).** The
  sender drives the compositor's immediate context (spoutDX takes it with
  `GetImmediateContext`), which is not thread-safe; if either could cross
  threads, safe code could send on one thread while another composes. So
  `Compositor` carries a `PhantomData<*const ()>` (#223 S1b round 3) and
  the sender's shim handle is a raw pointer: both stay on the thread that
  made them. S2 builds both on `program-max`.
- **One SDK call at a time per process** (`win/spout_sender.rs` `SDK`
  mutex, around every shim call, never nested): the SDK keeps
  process-global state (its log buffer, written by every log of level
  Notice or higher even with logging off), so senders on two threads must
  not run it at once.
- `SendTexture` copies the render target into Spout's OWN shared texture
  (`CreateSharedDX11Texture`: `MISC_SHARED`, not keyed, a legacy handle)
  under the named mutex `<name>_SpoutAccessMutex`, then `Flush`es. If a
  receiver holds that mutex over 67 ms Spout SKIPS the copy and still
  returns true: nothing tells the caller, `send_us` shows the wait.
- **#251 step 3: the shim never calls `SendTexture`.** `SendTexture`
  announces the frame (`SetNewFrame`) and releases the mutex right after
  `Flush`, which only SUBMITS the copy: a receiver could take a frame the
  GPU was still writing. On SNV's wall that was the new picture for one
  refresh (often only its top part), then the old one again, `-1 [0] -1
  +1`, 8-18x more often under background CPU load (#251 comment
  6202137095). The shim's `Sender` holds `SyncedDX` (a subclass, the
  vendor unmodified), whose `SendSynced` runs the same steps on the same
  members and WAITS for the copy (an event query of the device, `GetData`
  + `SwitchToThread`, bounded by `FRAME_WAIT_LIMIT`) before `SetNewFrame`
  and `AllowTextureAccess`, still under the mutex. Code 6
  (`status::COPY_NOT_DONE`) = not done in time: NOT announced (the mutex is
  let go all the same), a lost frame through `spout_state::after_send` (so
  a first send's registration is still recorded), then
  `spout::lost_send_error` → `GpuError::Timeout`; a lost device is read
  first. The process-wide `SDK` lock spans the wait (only `program-max`
  sends).
  `spout_tests.rs::the_shim_announces_a_frame_only_after_the_gpu_wrote_it`
  reads the shim source for the order (the shim compiles only on Windows).
- Registration is lazy: Spout lists the sender at its FIRST send (it needs
  the texture's size and format). Before that `spout_sender_info` is `None`.
- `spoutDX::OpenDirectX11(device)` keeps the device pointer WITHOUT AddRef
  (and AddRefs the immediate context). So `SpoutSender` holds its own clones
  of the device and the render target, and its `Shim` field (declared
  first) releases the spoutDX before them. It may outlive the `Compositor`
  value; S2 drops both on a lost device.
- **The shim is primitives; the decisions are `src/spout_state.rs`**, pure
  and Linux-tested (the mutation gate sees them). `spout_shim.cpp` only does
  one SpoutDX step per call: `open` (+ Spout's own `CleanSenders`), `listed`
  (yes / no / unreadable), `claim_name` (`SetSenderName`: kept or renamed),
  `send_synced` (`SyncedDX::SendSynced`, #251 step 3), `state` (`IsInitialized`, the name kept),
  `refuse` (release), `size`, `release`.
- **A second sender with the same name is REFUSED** (Spout would rename it
  `SP-program-MAX_1`, which Arena's layer never shows), and a refusal is for
  good (`Registration::Refused(why)`: every later send returns it):
  - at create (`spout_state::claim`): a listed name, or one Spout renames
    while claiming it → `GpuError::SpoutNameTaken`; Spout's `CleanSenders`
    first drops names whose sender crashed (their info map is gone);
  - before the first send (`before_send`): a name another sender listed
    since → `SpoutNotRegistered { why: TAKEN }`, BEFORE Spout can register
    `_1` (or, with a full list, take over that sender's map). Residual: an
    unreadable list lets the send go (as Spout's own `FindSenderName`
    does), and another process can list the name between the check and the
    send; with a full or locked list Spout would then write that sender's
    map. Only a second program sending as `SP-program-MAX` (a second
    SongPlayer) can meet it;
  - after the first send (`after_send`): registered under another name →
    `TAKEN`; not in the list → `NOT_LISTED` (its list was full, or its
    lock timed out during the registration: Spout's `RegisterSenderName`
    then registers nothing and `CreateSender` ignores that; or another
    program's `CleanSenders` dropped it mid-registration); nothing
    registered (the send failed or threw before the registration) →
    `REGISTRATION_FAILED`: a retry would meet its own half-made
    registration and be renamed `_1`. A send that fails AFTER the
    registration loses one frame and leaves the sender `Unconfirmed` (never
    `Fresh`: its next pre-send check would find its own name and refuse
    it).
  - A refusal calls the shim's `refuse`: `ReleaseSender` (a completed
    registration and its texture), plus a half-made registration whose info
    map THIS sender made (`FindSender` looks only at this object's maps),
    never another sender's listing. Drop it and make a new sender, after
    a BACKOFF (S2: a few seconds): a full list or a taken name refuses an
    immediate retry the same way, and each retry makes a new 4K shared
    texture.
- A list that cannot be read at the first send (its 67 ms lock) leaves the
  sender `Unconfirmed(n)`; the next send checks again, up to
  `MAX_UNREADABLE` (30) unreadable checks in a row, then `UNREADABLE`
  refuses it (each read may wait 67 ms, so a stuck list costs up to 30
  sends × 67 ms, about 2 s, not forever; its own info map may then stay
  until Drop, since Spout takes the list's lock before releasing it). A
  confirmed sender's send reads nothing (no lock per
  frame): it shares, or loses one frame (`GpuError::Spout`, code 3 or 4).
- Names: 1..=228 bytes of printable ASCII, no `\` (`check_sender_name`;
  the shim checks the length and the backslash again). A sender Spout
  renames to `<name>_<n>` (≤ 11 more bytes) gets `<name>_<n>_Count_Semaphore`
  (16 more) built in 256 bytes with `sprintf_s`, which ABORTS the process
  on overflow: 255 − 27 = 228. No kernel object name may hold a backslash.
- `Drop` deletes the `spoutDX`: `ReleaseSender` takes the name off the list
  and closes its info map, `CloseDirectX11` flushes and releases its
  context reference.
- After every send, `GetDeviceRemovedReason`: a lost device is
  `GpuError::DeviceLost`, as in `compose`.
- Every shim call that can throw catches every C++ exception (code 4);
  `size` / `state` cannot throw; `release` runs `~spoutDX` (a throw in a
  destructor terminates, it never unwinds): nothing unwinds into Rust.

### Spout's registry (`spout_sender_names`, `spout_sender_info`)

Read as a receiver does: open the named map, take its named mutex
`<map>_mutex` (`WaitForSingleObject`, 67 ms, as `SpoutSharedMemory::Lock`),
copy, release. A missing map (`HRESULT 0x80070002`) is "absent", never an
error. A map that is not committed memory, or a sender's map shorter than a
`SharedTextureInfo`, is `GpuError::SpoutMap`.

- `SpoutSenderNames`: MaxSenders × 256 bytes (64 by default, registry
  `MaxSenders`). One NUL-terminated name per slot. The list ends at a slot
  whose first byte is 0 or ≥ 0x80 (Spout tests a signed `char` `> 0`), or a
  slot with no NUL (there Spout's `strncpy_s` would hit MSVC's
  invalid-parameter handler, which ends the process).
- A sender's own map, named after it: `SharedTextureInfo`, 280 bytes LE:
  `shareHandle, width, height, format, usage` (u32 each), `description[256]`
  (the sending exe's path), `partnerId`. On x64 a receiver opens
  `LongToHandle((long)shareHandle)`: the 32 bits SIGN-EXTENDED
  (`share_handle_value`).

### Spout locks handed over abandoned (#243)

**The bug is upstream.** Spout 2.007.017 never releases a named mutex it got
with `WAIT_ABANDONED`:

- `SpoutSharedMemory::Lock` returns nullptr on anything but `WAIT_OBJECT_0`;
- `spoutFrameCount::CheckAccess` returns false on it.

The waiting thread then OWNS the mutex: Windows hands an abandoned mutex to
its next waiter. Nothing ever unlocks it, so that thread keeps it until it
exits. Only that one thread can keep using it (a mutex is recursive for its
owning thread). Every other thread's wait times out at 67 ms, the other
threads of the same process included.

**Any Spout program that dies mid-lock poisons the box.** That includes
SongPlayer at a deploy's `Stop-Process`, a crashed Arena, and an OBS with the
`win-spout` plugin. What PP showed on 11-12.10.2026:

- Arena held `SpoutSenderNames_mutex` for 8 hours.
- Every SongPlayer sender was refused `UNREADABLE` (12 172 times), so MAX
  never went out.
- Arena crashed at 01:04:48 UTC.
- The SongPlayer that was retrying then got the lock, and kept it across the
  next deploy. Owner pid 8492 was read with `NtQueryMutant`.
- A receiver keeping a sender's `<name>_SpoutAccessMutex` makes Spout skip
  every copy. Arena then shows a frozen picture (#243's event freeze), and
  each send waits 67 ms (PP's `send_us_p99` ~72 ms).

**What SongPlayer does:**

- **It gives back what it was handed.** Every shim call runs in
  `sdk(sender, ..)`. After the call it releases each Spout lock the CALLING
  thread owns: `SpoutSenderNames_mutex`, `ActiveSenderName_mutex`,
  `<name>_mutex` and `<name>_SpoutAccessMutex`. The check is `NtQueryMutant`
  `MutantBasicInformation` (`OwnedByCaller`); the thread releases as often
  as it holds the mutex (`spout_lock::releases_owed`, at most 64).
  - Spout's own locks are balanced within a call, so between calls the
    thread owns one only when it was handed it abandoned.
  - The open and the name claim run Spout's `CleanSenders`, which reads every
    listed sender's info map under `<name>_mutex`. When it meets one of those
    abandoned, it drops that sender off the list and keeps the lock.
    `sdk_cleaning` therefore gives back the map locks of the senders listed
    BEFORE or after the call. A list read after the call alone never sees
    the dropped name (review round 1).
  - Exceptions to "balanced within a call":
    - A C++ exception the shim catches can leave a lock taken. The OS mutex
      is then given back, but Spout's object still counts it as locked and
      reads its map unlocked afterwards. That is no worse than keeping the
      lock.
    - A renamed sender (`<name>_1`, refused at once `TAKEN`) locks names
      that are not on the list.
  - Our registry reader `read_map` releases an abandoned wait itself.
- **It refuses a sender whose texture lock is kept elsewhere.** After each
  send that was not refused, the sender looks at its texture lock.
  - A send COUNTS when another thread owns that lock AND the send took at
    least `ACCESS_TIMEOUT_US` (60 ms). That means Spout's 67 ms
    `CheckAccess` timed out, so the copy really was skipped.
  - At `MAX_ACCESS_HELD` (30) counted sends in a row, the sender is refused
    `ACCESS_HELD` (`spout_state::access_held`) and leaves the list.
  - Why the 60 ms condition (review round 1): a receiver may take the lock
    right after our send. Spout's own `ReceiveImage` maps a staging copy
    under it, 7–12 ms at FHD. That receiver shows up as the owner at the
    look, but it cost the send nothing, and counting it would refuse a
    healthy sender every 30 sends.
  - The `program-max` thread makes a new sender after its 3 s backoff. The
    old mutex object, and with it the kept lock, goes away only once every
    handle to it is closed, that is once every receiver has let go of the
    sender. Until then each rebuild repeats the cycle: 3 s off, then ~2 s of
    sends that each wait 67 ms.
- **It names the holder.** A refusal about a kept lock (`UNREADABLE` for the
  list, `ACCESS_HELD` for the texture) carries `holder` on
  `GpuError::SpoutNotRegistered`. It comes from `MutantOwnerInformation`,
  then the owner's image via `QueryFullProcessImageNameW`, formatted by
  `spout_lock::holder_text`.
  - `max.state` and the WARN then read "… (held by Arena.exe (pid 1500))".
  - That is the program to restart. A lock another process keeps cannot be
    released from outside.
- **The glue is small.** `win/spout_lock.rs` only queries and releases; it
  links ntdll with `raw-dylib`, since the windows crate has no
  `NtQueryMutant`.

**Reading it on a box:** `NtQueryMutant(OpenMutexW(SYNCHRONIZE |
MUTANT_QUERY_STATE, "SpoutSenderNames_mutex"), 1, …)` gives the owner's
pid/tid; a `WaitOne(300)` from another process times out while it is kept.
The PowerShell `Add-Type` probe used on PP is in #243 (comment of 12.10.2026).

**When SongPlayer itself dies mid-lock (#243 follow-up, FINDING of
12.10.2026 ~12:58).** deploy-pp 38239301748 stopped SongPlayer with
`taskkill /F` while one of its threads held `SpoutSenderNames_mutex`. The
next waiter was Arena "Songs PP": handed the mutex abandoned, it kept it, and
every new SongPlayer sender was refused `UNREADABLE` (holder named) until a
manual rescue (`C:\ProgramData\SongPlayer\diag\spout_list_rescue.ps1`
queued on the mutex, then Arena was killed: the helper was handed it
abandoned 41 ms later and released it). A receiver cannot be fixed, so:

1. **Quiesce before the kill.** `POST /api/v1/program/spout/quiesce`
   (`api/program.rs`, `202 {state}`) → `MaxOut::quiesce`: nothing is
   offered while it holds, and the `program-max` thread's next step is
   `MaxNext::Quiesce` (after a stop, before everything else, whatever the
   setting), once per quiesce. `MaxWorker::quiesce` drops both senders
   (MAX, then `SP-program`; #251 step 2: then their one compositor), stops the list
   sentinel and reports `quiesced`; the thread then waits: no Spout call
   runs when the kill lands. Every workflow step that kills SongPlayer
   runs `scripts/spout-quiesce.ps1` first (`ci-workflows.md`). Not limited
   to loopback callers: no route of this API checks its caller, and the
   box's cloudflared tunnel connects from 127.0.0.1 anyway.
   - **It expires** (review round 1 of the follow-up;
     `program_max_quiesce.rs`, a child of `program_max.rs`). It holds
     `QUIESCE_EXPIRY` = 60 s from its LAST ask (`Queue::quiesced_until`;
     an ask while it holds renews it, the thread does not quiesce again).
     A deploy kills the process within seconds; one that does not (a run
     cancelled between the ask and the kill, a kill that failed) must not
     leave MAX dark until a restart. At the deadline the thread's step is
     `MaxNext::Resume`: `MaxWorker::resume` records `running` and one WARN
     `the quiesce expired - the process was not stopped; the Spout senders
     resume`; the next job builds both senders again. `MaxOut::next` waits
     with a timeout up to the deadline, so it wakes by itself (nothing
     notifies it then). A thread that sees the ask only after its deadline
     still quiesces once, then resumes.
   - `max.state` and `max.fhd.state` read `quiesced` (`MaxPhase::Quiesced`)
     only while it holds: it wins over `off`; the resume, or the thread's
     end, ends it. No phase is "settled" any more but `Unsupported`.
   - Every decision takes its instant (`MaxOut::quiesce_at`,
     `MaxOut::try_next_at`); `set_quiesce_expiry` (tests) shortens it for
     the real-loop tests. Tests: `program_max_quiesce_tests.rs`.
   - **A normal shutdown waits for the thread** (the tray's Exit awaits
     `sp_server::start`'s join handle, 30 s): `start` ends with
     `program_max::stop_and_wait(max, MAX_STOP_WAIT)` (3 s) after its
     shutdown broadcast: stop, then `MaxOut::wait_ended` on the `ended`
     condvar (notified by the `Consumer` guard's drop, after the loop's
     final `release`) on the blocking pool. One INFO when it ended, one
     WARN with `bound_ms` when it did not; the process ends either way. The
     thread's stop path unregisters both senders, which takes Spout's list
     lock: the process must not end under it.
2. **Win the lock back: the sentinel.** After every boundary, after both
   sends, the `program-max` thread reads who holds Spout's list
   (`program_max_list.rs` `ListSide::boundary` → `SpoutList::holder` →
   sp-gpu `spout_list_holder` / `mutex_holder`: another thread's holder,
   pid + program file name). The same program (pid), never this process,
   on every read for `LIST_HELD_AFTER` = `MAX_RETRY_BACKOFF` (3 s) is named
   once per holding (`ListWatch`, pure): one WARN `another program keeps
   Spout's sender list`, and a sentinel starts, one at a time
   (`ListSentinel`, thread `spout-list-sentinel`, `win/spout_lock.rs`).
   - **It stays queued for its whole life** (review round 1 of the
     follow-up: bounded 500 ms waits left the lock's wait queue between
     them, so a relaunched Arena's wait could queue first). ONE
     `WaitForMultipleObjects([stop event, list lock], any, INFINITE)`; the
     stop event is unnamed and manual-reset (`CreateEventW`, its handle
     kept as a number, `StopEvent`, closed on drop after the join), and
     `ListSentinel::stop` sets it and joins: it ends at once. It never
     spins and never holds the lock past the release.
   - `sentinel_step(wait)` (pure) reads the answer: `WAIT_OBJECT_0 + 0`
     (0x0, the stop) → `Stopped`, the lock not taken; `+ 1` (0x1) → give it
     back, `abandoned: false`; `WAIT_ABANDONED_0 + 1` (0x81, its holder
     died) → give it back, `abandoned: true`; anything else (`WAIT_FAILED`,
     a timeout it cannot have, 0x80 for the event) → `Failed`.
   - The give-back (`give_back_taken`) tries `ReleaseMutex`
     `SENTINEL_RELEASE_TRIES` (3) times (the release of a mutex the thread
     was just handed is not expected to fail); still failing, one WARN
     names the error and it ends `Failed` holding the lock, which its end
     leaves abandoned again for the next waiter.
   - When the holder exits, Windows hands the lock to its next waiter, the
     sentinel, queued before a relaunched Arena (Windows does not promise a
     FIFO wait queue: a kernel APC can move a waiter back): one INFO
     (`abandoned`), and the next sender after its backoff reads the list
     again. A free read, this process or another pid starts the count
     again; a sentinel that ended (or did not start) lets the next naming
     start one; MAX off forgets the holder (the sentinel keeps waiting); a
     quiesce stops it (joined, at once).
3. **Visible.** `GET /api/v1/program` → `max.list_holder`: `null`, or
   `{image, pid, since_s}` once named (`since_s` from the first read that
   saw it). That program is the one to restart.

**WARP tests (`tests/spout.rs`, `tests/spout_list.rs`).** The named-mutex
helpers live in `tests/common/locks.rs` (both binaries use them;
`tests/spout.rs` was at the 1000-line cap).

- A helper thread takes the mutex and ENDS: that is an abandoned lock.
- Another thread's 300 ms wait then tells free (0x0), kept (0x102) and
  "nothing took the abandoned lock" (0x80) apart.
- A held lock is a helper thread blocking on a channel (`Hold`). Its holder
  text is the test exe with ", this process".
- `GiveBack` releases whatever the test thread still owns when dropped, so a
  failing test never poisons the next one.
- #243 follow-up (`tests/spout_list.rs`, their own mutexes): `Doomed` holds
  a mutex until `die`, then ends without releasing it; the sentinel queued
  on it is handed it abandoned, gives it back, and another thread then
  takes it with 0x0 (an `_alive` handle keeps the mutex object, so a fresh
  mutex can never pass the check). A stopped sentinel ends `Stopped` at
  once and leaves the lock to its holder; a released lock is given back
  `abandoned: false`. The stop runs on a helper thread under a 10 s
  bound, so a sentinel that ignores it fails the test instead of hanging
  it. `mutex_holder` names another thread's holder (pid +
  the test exe), none for a free lock, the caller's own or no such mutex;
  `spout_list_holder` reads Spout's own list lock (under the one-at-a-time
  lock).

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
- a sender whose name another sender listed since it was made is refused
  before Spout registers it (`TAKEN`), twice. The test runs on a list of
  63 fake senders, so the winner fills it: without the pre-send check
  Spout would not rename the loser but open the WINNER's info map and
  write its own texture there, and the loser would end `Confirmed` (the
  test fails at its refusal match, and the winner's map would no longer be
  the winner's). No `_1` ever appears, and the winner stays listed;
- the sender keeps sending after the `Compositor` is dropped (its own
  references);
- with Spout's list made FULL (64 fake senders, each with its info map),
  a new sender is created but its first send is refused `NOT_LISTED`, its
  own info map released and the 64 entries untouched (this is the only way
  to reach `NOT_LISTED` on CI).
- `FakeSenders` writes Spout's MACHINE-WIDE list, so it refuses to run
  where any sender is listed or `HKCU\Software\Leading Edge\Spout`
  `MaxSenders` is not the default 64 (Spout would read past the 16 KiB
  map): windows-latest, never a box where Spout is in use.
- a name Spout cannot carry never reaches Spout; a missing map is `None`.
- The helpers both WARP test binaries use (`warp()`, the smooth `pattern`,
  `assert_matches_reference`) live in `tests/common/mod.rs`.

WARP has supported shared resources since Windows 8 (the
D3D11_RESOURCE_MISC_FLAG docs); the second-device test FAILS with the
HRESULT if it ever refuses, it never skips.

## Runtime wiring: `SP-program-MAX` in the program output (#223 S2)

Design: revision 3 R3-1 (what MAX shows) and R3-2 (the `program-max`
thread), comment 5979609879; revision 2's D4 hand-off (5872871751). Anchors:
#223 comment 5987322560.

### What MAX shows, and where it is offered (`program_output.rs`)

- `ProgramOutput::serve` = `split` → `limit` → `feed_outputs` → **`offer_max`**
  → `submit_video`. VBAN has the boundary's block before ANY MAX work, and
  MAX has its job before the canvas fit and the NDI submit
  (`program_output_tests_max.rs`: a hook inside the offer sees VBAN's block
  and no NDI send, for all three job kinds).
- The job (`Pair::max_job`, `program_max::MaxJob`) is the NATIVE picture,
  never the 1920×1080 canvas (R3-1): a forwarded source pair → `Picture`
  (its own `SharedFrame`, an `Arc` bump, its own width/height/stride); a
  fade boundary → `Fade { from, to, weight_q8 }` (both native pictures, a
  missing side `None` = black, the weight `MixJob::weight_q8`); the
  program's standby → `Black`. #251: a forwarded pair the pacer BLENDED
  (`SubmitJob::blend`, a source the grid does not divide, `genlock.md`) →
  `Fade { from: the shown frame, to: the next frame, weight_q8 }`, so such
  a song uploads two pictures per boundary. Every job carries `SP-program`'s stamp (Spout
  has no timecode; the stamp names the boundary). Held / paused / seek /
  starve pictures reach MAX as whatever picture their source pair carries;
  the `MaxSide` plumbing that keeps them native is S3.

### The hand-off (`program_max.rs`, `MaxOut`, one per process on `ProgramBus::max`)

- `offer_with(make)`: one short lock; `make` (the `Arc` bumps) runs only
  when the job would be taken (on, a `program-max` thread attached, not
  stopped). The queue is the `SubmitQueue` (`MAX_HANDOFF_BOUND` = 2): a full
  queue drops its OLDEST job and counts it (`coalesced`). The program thread
  never waits for the consumer: a stalled consumer only coalesces
  (`a_stalled_max_thread_never_delays_vban_or_the_ndi_submit`: the fake
  GPU's compose held behind a gate, six more boundaries reach NDI and VBAN
  within the bound, `coalesced` 4, MAX then draws boundary 0 and the newest
  two).
- The consumer's `next(holding)`: `Stop` wins; while nothing wants the
  compositor (#279 P1b: MAX off AND NDI on the canvas), `Release` when it
  holds GPU objects; else the next job. `set_enabled(false)` drops the
  queued jobs (unless NDI still wants the compositor) and wakes it;
  `attach()` returns the guard whose drop stops the offers (a dead thread
  never leaves a stale "running").
- The queue and the stats are two locks: an API read takes the queue lock
  only to copy two fields (`enabled`, `coalesced`), copies the p99 windows
  under the stats lock and sorts them after it.

### The `program-max` thread (`program_max_worker.rs`)

- `run_max_loop(out, gpu)`: `attach`, then `next` → `serve` / `release` /
  exit. The GPU is the `MaxGpu` trait (`compositor()`, `sender(&c,
  target)`, `fhd_sender(&c, target)`; `MaxCompositor::{compose, adapter,
  add_target, remove_target, draw_us, enable_uyvy, disable_uyvy,
  read_uyvy}`, `MaxSender::{send, listed_size}`; since #251 step 2 in
  `program_max_gpu.rs`, re-exported by the worker), so every decision is
  Linux-tested on a fake (`program_max_worker_tests.rs`: ONE
  `FakeCompositor` minting target ids with the real `sp_gpu::Targets`).
  Production is `SpoutGpu` (#279 P1b: `Compositor::with_size` of
  `FHD_TARGET` on the picked adapter, `SpoutSender::for_target(&c, max,
  SPOUT_SENDER_NAME)` = `SP-program-MAX` of the MAX target the worker adds),
  spawned by `start_max` on Windows as thread `program-max`.
- The objects are built ON the thread at the first job (none is `Send`)
  and dropped senders first (MAX's, then the FHD one), then the compositor
  (they hold references into its device and targets).
- #251 step 2, the per-job order (`attempt`): build what is missing, each
  unless its own backoff runs (the compositor = `Device`; MAX's sender),
  pace, wait for `compose_at`, ONE compose (every target the compositor
  holds), MAX's send at `due`, then the FHD one's. **MAX goes first, whole
  (#239):** an FHD output still to build (its target, its sender: a Spout
  open with `CleanSenders`, up to 67 ms per kept lock) is built AFTER MAX's
  send (`build_fhd_after`) and drawn from the next compose; its build
  boundary reports nothing for it. So the build never delays THIS
  boundary's MAX; it still runs on the thread before the next job, so an
  open longer than the wait to the next compose instant (33.3 ms − K)
  makes that compose late (`compose_late`), longer by K its send too
  (`send_late`). Only when MAX's boundary is not drawn at all (its build
  failed, lost the device or waits a backoff, `fhd_first`) is the FHD
  output built first and sent alone at the due instant. A device lost
  while building the FHD output first fails MAX with it, nothing drawn. A
  device the FHD side loses AFTER MAX's boundary went out (its send, or its
  build after MAX's send) is MAX's too: the boundary stays counted as sent,
  never failed, and MAX's state (`MaxOut::record_lost`) and log name the
  loss, so MAX never reads `running` while it waits for the device. Every failure
  is recovered from where it happens (`output_failed`, `lose_device`),
  once: a lost device is ONE event for both outputs (`device_resets` +1,
  both fail with it, `device_lost` hands MAX's to the FHD side), and
  `lost_unsent` is cleared by a send of EITHER output that went out (in
  `send_max` / `send_fhd`, so a MAX boundary sent before the FHD send lost
  the device never counts as one after it).
- **Picture ids (`PictureIds`, `program_max_ids.rs`).** S1a's residency skips the upload of an id
  the slot holds, so an id must never name other bytes: ids come from a
  counter. A picture that is the same allocation (`SharedFrame::ptr_eq`) as
  one of the last composed boundary's keeps its id; those `Arc`s are held
  by `PictureIds`, so none can be freed and its address reused while
  compared. A held / paused / repeated frame is not uploaded again into its
  slot (sp-gpu's residency is per slot: a fade's incoming picture moves to
  the outgoing slot when the fade ends and is uploaded there once). Never
  key an id on an address or on address + stamp (the latter re-uploads
  every held picture).
- **Failures** (recovered where they happen, `output_failed` /
  `lose_device`, never a panic):
  - a lost device at compose or send drops both; rebuilt at once on the
    next job (`device_resets`), unless the rebuilt pair is lost again
    before a boundary went out (`lost_unsent`): then the rebuild waits the
    backoff, so a GPU that keeps losing its device is never rebuilt (a new
    device, shaders, a 4K target, a Spout sender and its 4K shared texture)
    30 times a second next to Arena;
  - a lost device while building waits the backoff like any failed build
    (it drops the compositor too);
  - a refused sender (`SpoutNotRegistered`, `SpoutNameTaken`: `is_refusal`)
    drops the sender only; a new one after `MAX_RETRY_BACKOFF` = 3 s
    (`sender_backoffs`), since an immediate retry is refused the same way
    and each makes a new 4K shared texture;
  - any failed build (`NoAdapter`, a shader, a sender that could not open)
    waits the same backoff: never a device creation per boundary;
  - any other failure (a picture that is not whole NV12, one frame Spout
    lost) costs that boundary only;
  - `Unsupported` (off Windows) is final: never built again.
  Every failure counts `failed`, a boundary skipped during a backoff too;
  `Unsupported` counts nothing.
- **The log (`LogGate`, pure)** says what the thread does: the WARN
  `program max: boundaries do not go out` (with the reason) when boundaries
  stop going out or fail for another reason, the INFO `program max:
  boundaries go out` when they go out again — a line whenever the state
  differs from what the log last said, at most one per 5 s of the thread's
  time (`MAX_LOG_EVERY_100NS`, a `WarnLimiter`). A change inside the window
  is not lost: the first boundary after it writes the state as it is then
  (`held_back` = the boundaries held back), so once a boundary comes after
  the window the log's last line names the current state (a lost device
  whose rebuild then finds no adapter ends on the no-adapter WARN; a
  switch-off or a stop inside the window can leave it one change behind,
  the telemetry's `state` is always current), and a failure that
  alternates with sent boundaries never floods it. `serve` returns the line it logged, which
  the tests read.
- Dropping the GPU objects (a lost device, MAX off) also drops the held
  pictures (`PictureIds::forget`): no decoded frame stays pinned out of
  `frame_pool` while MAX is off.
- **The send leaves at a constant phase** (#223 follow-up, 9.10.2026,
  `program_max_send.rs`). Arena renders at 60 Hz and reads Spout's shared
  texture at its own instant; a send made the moment the compose was done
  (offer + upload 0–2.4 ms + draw up to 5.7 ms) spread 5.9 ms (p1–p99) on
  the 30 fps grid and the wall showed frames for 1 or 3 output frames: a
  stutter, with ONE Spout layer too. Now `MaxOut::offer_with` stamps each
  job with its offer `Instant` (`MaxNext::Job(job, offered)`), the worker
  composes at once and `send_at` waits until the due instant on its
  `SendClock`, then sends (`SendSynced`); a compose that ran past it sends at once
  and counts `send_late`. Production waits on `SpinClock` (sleep to
  `SEND_SPIN_MARGIN` = 2 ms short, then spin; the decision is the pure
  `send_wait_step`); `MaxWorker::new` uses `NoWait` (tests pass made-up
  instants), `run_max_loop` gives it `SpinClock`.
- **The due instant is a slot of the PRIMARY display's refresh, DWM's
  clock** (#223 follow-up, 9.10.2026, `program_max_vblank.rs` + sp-gpu
  `vblank.rs` / `win/vblank.rs`). A constant 12 ms after the offer was not
  enough: the wall's composed frames (60.000 Hz, 0 missed) still showed bursts of
  single-refresh pictures (`wall_runs.py`: 5–87 one-refresh runs per 15 s,
  minutes apart), because our 30 fps PTP grid drifts a few ppm against
  Arena's render and for minutes at a time each send lands next to the
  instant Arena reads Spout. Spout's own advice is to match the sender's
  rate to the receiver's; an ordinary sender renders in the display's
  rhythm. So: `sp_gpu::VblankTracker` (a `program-max-vblank` thread,
  time-critical) waits `IDXGIOutput::WaitForVBlank` on the output
  `pick_output` chooses among every adapter's outputs: the attached one at
  the desktop origin, the PRIMARY, never a second display. WHICH refresh
  was measured, not assumed: the first cut picked the wall's own output
  (DISPLAY1) and re-picked a slot every ~50 s (4 in 3 min), because the
  LED processor's timing makes DISPLAY1's vblank 59.979 Hz while DWM
  composes the wall's output at 60.0000 Hz, phase-locked 2.3 ms after the
  primary's vblank (DISPLAY5, 60.0000 Hz) for 40 s
  (`C:\ProgramData\SongPlayer\ops\phase_lock.py <display> <out> <s>`:
  D3DKMTWaitForVerticalBlankEvent on a display + DXGI duplication of an
  output, the composed frames' phase per third; `vblank_rates.py` = each
  display's vblank rate). Arena renders in DWM's rhythm. DWM's own timing
  API is no shortcut: `DwmGetCompositionTimingInfo(NULL)` reported 60.005
  Hz and a `cRefresh` counting 235/s on SNV (`dwm_timing.py`). And
  `VblankFit` turns the wake-ups into a grid: the median of the first 15
  intervals boots the count, each wake-up is counted to a refresh index
  (n = round(gap / period), a gap of n counts n − 1 missed, a wake-up
  under half a period is `Early` and left out), and a least-squares fit
  over the last 240 refreshes gives the grid (reported from 60 counted,
  periods 4–50 ms only, stale after 100 ms). `VblankPacer` then sends each
  boundary in a slot `vblank + phase` (setting
  `program_max_vblank_phase_ms`, default 14 ms: measured, see below) and KEEPS its lead (due −
  offer) from one boundary to the next while it stays in
  [K, K + one period + `LEAD_HYSTERESIS` 3 ms] (#251: K =
  `compose_offset`, "The compose phase" below; it was a constant
  `LEAD_MIN` 12 ms floor before): the
  30 fps boundaries then sit on every second refresh, and only the slow
  drift carrying the lead out of the window picks a new slot (one
  `slot_repick` = one picture held one refresh more or less, hours apart;
  the 3 ms hysteresis keeps a ±0.2 ms offer jitter at the edge from
  flapping). No grid (no tracker, its output stalled) = the constant
  `MAX_SEND_LEAD` (`send_due`), and the next grid starts afresh. The slot
  maths is branch-free on purpose (`signed_ns` from the earlier instant,
  `shifted` by max(±ns, 0)): an `if t >= origin` form had equivalent
  mutants at the equal-instant edge. Tune the phase on the box: PATCH the
  setting (re-read every 5 s, no restart) and measure the wall with
  `C:\ProgramData\SongPlayer\ops\wall_runs.py` (DXGI duplication of
  the wall, region x 400–5600 y 40–560 without texts: one-refresh runs and
  change intervals; a 30 fps picture held 2 refreshes is clean) next to
  `spout_timing.py SP-program-MAX 15` (SpoutGL, ~500 polls/s).
  `ops\phase_sweep.py <s> <ms>…` PATCHes each phase, waits 8 s, runs
  `wall_runs_t.py` on the wall and appends to `phase_sweep.log` (launch it
  with `Start-Process … -WindowStyle Hidden`: an MCP shell drops a run's
  output past its 300 s idle bound). **Measured 9.10.2026 (single-refresh
  pictures):** 8 ms ~69 / min with 1/3 splits (Arena reads the texture
  there), 0 ms 13 / 60 s, 16.3 ms 25 / 180 s (near the vblank is worse
  too), 3 ms 13 / 180 s, 11 ms 8 / 60 s, **14 ms 7 / 60 s, 9 / 180 s, 0 /
  30 s**: the default. The same 8-vs-14 contrast is the causal check that
  Arena shows MAX at all: after an Arena relaunch (its clips reconnected)
  16 + 39 vs 5 + 0, after a forced SongPlayer kill 25 + 41 vs 0 + 0.
- **The compose phase** (#251, SNV 15.10.2026, design on #251, "Design-by"
  comment 6181037052). The SEND was on the grid, but the COMPOSE (the 4K
  uploads and the blend draw; every 24/25 fps boundary is a two-picture
  `Fade` since #251's frame blending) started at the OFFER, which drifts
  against the refresh (~0.4 ms/min, a full turn ~45 min). The wall then
  showed the previous picture for one refresh (`IH` pattern) whenever that
  GPU work landed where Arena renders and reads Spout. Measured with the
  `wall-25` frame-ID clip over two drift cycles (wall jumps back per
  minute against where the compose started after the PRIMARY's vblank):

  | compose start | jumps back / min |
  |---|---|
  | +10.4 … +15.2 ms | 0–2 |
  | +15.6 … +0.4 ms (around the vblank) | 4–19 |
  | +0.9 … +4.0 ms | 0–3 |
  | +4.4 … +9 ms | 7–30 (worst +6.7 … +8.9) |

  The FHD sender is not the cause (A/B: off gave both 3 and 113); the old
  12 ms lead floor added late sends at the window's bottom, a secondary
  effect. So now:
  - `VblankPacer::due(offered, grid, send_phase, compose_phase)` also
    returns `Due::compose_at` = due − K, `None` off the grid (composed at
    once: exactly the old behaviour, the constant `MAX_SEND_LEAD`);
  - K = `compose_offset(period, send_phase, compose_phase,
    COMPOSE_BUDGET)`: from the compose phase FORWARD to the send phase,
    both modulo the period, plus whole periods until K ≥ `COMPOSE_BUDGET`
    (14 ms, above the measured upload p99 ~5–6 ms + draw p99 ~5–9 ms).
    SNV (send 19 ms = 2.333 ms into a 16.667 ms refresh, compose 13 ms):
    K = 6 ms + one period = **22.667 ms**; the default send phase 14 ms:
    K = 1 ms + one period = **17.667 ms** (the design's 23.17 / 18.17 were
    0.5 ms over its own definition; with them the compose would start at
    12.5 ms, comment 6181128848);
  - the lead window's floor is K, so the compose instant is never before
    the offer; the top is K + one period + 3 ms; `LEAD_MIN` is deleted;
  - the worker paces FIRST, waits on its `SendClock` until `compose_at`
    (`program_max_send::wait_for`, the wait `send_at` shares: a job taken
    after that instant composes at once and counts `compose_late`),
    composes, then sends at `due` as before. Step 1 left the FHD side
    (its own device: a second upload, its draw and GPU wait) right after
    MAX's send, in Arena's window, and that carried a large part of the
    residual jumps (A/B on SNV, comment 6192123775: FHD on 5.35 jumps back
    per minute, off 3.09, the difference in the offer band ~14 → 4 ms).
    **Step 2** (design comment 6192134240, Approach 1): ONE compose at
    `compose_at` draws BOTH targets of the one compositor from one upload;
    at `due` MAX's send runs, then the FHD one's (plain copies, its
    `send_at` meets a passed instant). Rejected: pacing a second device's
    compose to the compose phase too (two uploads and two contexts at
    13 ms), and switching the FHD sender off (doing less, #239 asked for
    it);
  - setting `program_max_compose_phase_ms` (`sp_core::config`, default 13,
    `program_max_compose_phase_us`, the send phase's parse rule), read
    with MAX's settings every 5 s (`program_max_phase.rs`, a child of
    `program_max.rs`, holds both phases). Tune it on the box like the send
    phase: PATCH it, measure the wall over a whole drift cycle (never a
    short take: the effect comes in waves).
  - **Judging a deploy on the wall (#279 P2, 17.10.2026):** two 40 s takes
    15 min after a deploy read 11 / 11 against a baseline of 2 / 2; the
    revert, then a whole hour on each build starting after its E2E, read the
    same distribution (dev.12 23 / 30 takes at 0–2, worst 13 and 61; dev.13
    22 / 30, worst 6). A first post-deploy take decides only whether to
    revert at once; whether a build is worse needs an hour on each side.
    The read-only watch: `diag\wall-bridge\drift_watch.ps1` (one take per
    2 min with the song and MAX's counters), run as an interactive task of
    the Resolume user (`schtasks /create … /ru Resolume /it`, then `/run`;
    `/change` hangs asking for a password), deleted after.
    **Delete every such task as soon as its run ends, in the same turn: a
    `/sc once /st 23:59` trigger FIRES AGAIN at 23:59 that day** (19.10.2026:
    four A/B tasks ran at once at 21:59Z, in the middle of a deploy's E2E;
    each took the state the one before had changed as "the operator's",
    so their restores left the program on Blank and Arena's group 2
    soloed, and three post-deploy tests failed for it). Read the trigger
    back (`schtasks /query /tn X /v /fo list` → "Next Run Time"), or end
    the script with its own `schtasks /delete /tn X /f`.
  - The cost: the picture leaves later (mean lead ~K + 10 ms, up from
    ~22 ms) and the thread waits up to one period + 3 ms after the offer
    before it composes. Two outstanding jobs never coalesce: a lead under
    ~50 ms leaves at most one job queued.
  - A lead at the window's floor makes `compose_at` the offer itself, so
    the thread's wake-up latency (~0.1–0.2 ms) makes those composes late
    by that much (`compose_late` counts them, the phase median barely
    moves) for the ~30 s of each drift cycle the lead sits there.
  - Tests: `program_max_vblank_tests.rs` (the K pins, the window's two
    edges, the drift sweep over every phase with two re-picks each way:
    compose exactly at 13 ms, send at 2.333 ms, never before the offer),
    `program_max_worker_tests_send.rs` (the compose waits for its instant
    and never runs at the offer, a late job composes at once and counts,
    off the grid unchanged, three offer phases), the pins from a scratch
    Python model.
- **A dropped Spout name is put back** (#243, SNV 13.10.2026 ~12:52):
  ANY Spout program that lists or checks senders (vendored
  `SpoutSenderNames.cpp`: `GetSenderNames` + `getSharedInfo` in the
  receiver paths, `CheckSender`, `CleanSenders`) RELEASES a name whose info
  map it cannot read at that instant. `SP-program-MAX` vanished from the
  list while SongPlayer kept sending (no error, no log); Arena lost the
  source and the wall's video went black for 3 min (here my probe receiving
  `SP-program-MAX` at ~1 kHz raised the odds; Arena's own per-frame checks
  can trigger it too). Now a confirmed sender looks at the list every
  `RELIST_EVERY` (30th) send after the send (`spout_state::list_check`, the
  list's lock never taken per frame) and, when the name is gone, puts it
  back with `RegisterSenderName(name, false)` (shim `spout_sender_relist`:
  the same sender and shared texture, no `_1`), one WARN, counted as
  `max.spout_relists` / `max.fhd.spout_relists`. Never run a probe that
  RECEIVES `SP-program-MAX` on a production box; the access-mutex reads
  (`NtQueryMutant`, read-only) do not touch the list.
- **The vblank thread follows the primary** (#259, SNV 13.10.2026:
  re-forming the wall's Surround detached the VDD for a minute; the thread
  stayed on its DXGI output, `not_ticking`, and MAX sent at the constant
  lead, `send_off_grid` +2768). Every `VBLANK_RECHECK` (2 s, `recheck_due`)
  `win/vblank.rs` lists the outputs again (quietly; the full list is
  logged only at the first open) and, when `pick_output` chooses an output
  that differs in name, place or size (`repick`), moves to it with the fit
  booted afresh and one INFO naming both; a recheck with no primary keeps
  the last output (one WARN per streak). `VblankTracker::output` returns
  the output measured NOW, and the worker records it with the state at
  every boundary, so `max.vblank_output` names it.
- **A dark output never spins the thread** (#243, PP 9.10.2026: the laptop
  panel DISPLAY1 stopped presenting while still attached, `WaitForVBlank`
  returned at once, the fit's boot never ended and the time-critical
  thread spun a core for hours, unexplained on the API). Now
  `win/vblank.rs` times each wait: under `VBLANK_MIN_WAIT` (1 ms,
  `vblank::waited`) it did not wait — never fed to the fit — and the
  thread sleeps `not_waiting_sleep(streak)` (1 ms doubling to
  `VBLANK_IDLE_MAX` 250 ms, never none; the thread logs nothing for it). A
  refresh over `VBLANK_RESTART` (1 s) after the last counted one boots the
  fit afresh (the index is never carried across a long gap). The
  tracker's `state(now)` = `vblank_state`: `ticking` with a fresh grid,
  else `measuring` until `VBLANK_NOT_TICKING` (2 s) after the last
  refresh the fit COUNTED (`Seen::Counted`, or the start), then
  `not_ticking` — never reset by a lone wait that blocked (on a dark
  panel a preempted call would flip the state and its logs for 2 s,
  review); the worker records it per
  boundary (`MaxOut::record_vblank_state`) and logs only entering /
  leaving `not_ticking` (`program_max_vblank::vblank_log`, one WARN / one
  INFO). The fallback stays the constant lead: pacing on another output
  was rejected (DWM composes every output on the primary's refresh, the
  measurement above).
- **Restarts (owner's old Spout worry, verified 9.10.2026):** a SongPlayer
  restart, clean (4 deploys) or forced (`Stop-Process -Force`), brings
  `SP-program-MAX` back under its exact name in ~10 s (`sender_backoffs`
  0; the shim's `CleanSenders` drops an orphaned name first), and the
  running Arena shows it again with no step. An Arena kill + relaunch
  (`SP-ArenaLaunch`; the hotkey script did NOT relaunch within 25 s)
  reopened the composition with most clips DISCONNECTED, the Spout clip
  (layer 5 "Yt music", source `SP-program-MAX`) and the text layers alike
  (not Spout-specific); once reconnected through the REST API
  (`POST /composition/layers/<n>/clips/<c>/connect`), the Spout clip showed
  MAX at once. Before a relaunch, read the connected clips
  (`ops\arena_layers.py`) so they can be restored.
- **A forced kill can leave Spout's list to a receiver for good** (#243
  follow-up, PP 12.10.2026): hence the deploy's quiesce, the list sentinel
  and `max.list_holder` ("Spout locks handed over abandoned" above).

### The setting and the telemetry

- `program_max_enabled` (`sp_core::config::program_max_enabled`): ON unless
  it says exactly `"false"` (the owner decided MAX exists). `start_max`
  (`start_program`, before the `SP-program` thread) applies it FIRST, so an
  off setting never builds a sender; `run_max_settings_task` re-reads it
  every 5 s (`MAX_SETTINGS_POLL`; the poll is a parameter, the test uses
  5 ms) and, on shutdown, stops the thread. Off = no offers, the thread
  drops the sender (Spout unregisters `SP-program-MAX`) and the compositor.
  Toggle it with `PATCH /api/v1/settings {"program_max_enabled": "false"}`
  (a flat body; `api/program_tests_max.rs` saves it through the router and
  reads it back); no restart.
- `GET /api/v1/program` (and the cut answer) → `max {enabled, state, width:
  3840, height: 2160, submitted, coalesced, failed, upload_us_p99,
  draw_us_p99, send_us_p99, upload_mb_s, uploads_per_s (#279 P0, below),
  send_at_us_p50/p99/max, send_late,
  vblank_output, vblank_state, vblank_tracking, vblank_period_ns, vblank_phase_us,
  compose_phase_us, send_off_grid, send_phase_us_p50/p99,
  compose_phase_us_p50/p99, compose_late, slot_repicks, device_resets,
  sender_backoffs, spout_name, adapter, list_holder, fhd}` (`MaxStatus`;
  #251: `compose_phase_us` = the setting, `compose_phase_us_*` = where
  after the vblank the aligned boundaries' composes started, over the same
  window as the send phase, `compose_late` = grid boundaries whose job came
  after their compose instant;
  `list_holder` = #243 follow-up, above; `fhd` = #239,
  "The FHD Spout sender" below;
  `send_at_us_*` = send done − offer over the window, `send_late` = composes
  that ran past their due instant; `vblank_output` = the tracker's output
  (`\\.\DISPLAY2 7680x1080`, `null` with no tracker), `vblank_state` =
  #243's `measuring` / `ticking` / `not_ticking` (`null` with no tracker),
  `vblank_tracking` =
  the LAST send was on the grid, `vblank_period_ns` = the grid's period at
  the last aligned send, `vblank_phase_us` = the setting,
  `send_off_grid` = sends at the constant lead (a separate counter, so a
  read between `record_sent` and `record_vblank` can never make the gate
  see an aligned count behind `submitted`), `send_phase_us_*` = where after
  the vblank the aligned sends started, `slot_repicks` = new slot picks;
  `adapter` = the adapter the last compositor was
  built on, `None` before the first build: R3-2 asks the box to name its
  RTX). R3-2 sketched `spout {frames, adapter}`; S2 ships the flat shape
  the S2 dispatch named, with `frames` = `submitted` and `adapter` at the
  top level. `state` (`state_label`): `unsupported` (off Windows: no
  thread) wins, then `quiesced` (#243 follow-up: while the deploy's
  quiesce holds, 60 s from its last ask), then `off` (the setting), then
  the thread:
  `running` (it
  takes jobs and its last boundary went out), `error: <why>` (its last
  boundary did not, or #251 step 2 the device was lost after it went out,
  `MaxOut::record_lost`; `error: the program-max thread is not running` before
  it attached or after it ended). `submitted + failed` = the jobs it took
  (an `unsupported` platform counts neither). The p99s
  cover the last 900 sent frames (30 s). The mock (`e2e/mock-api.mjs`)
  mirrors the shape with `state: "unsupported"`, `adapter: null`.

### The live post-deploy gate (`e2e/post-deploy-max.spec.ts`)

Nothing else goes red if the thread dies on the box (the program, VBAN and
every other output work without it), so the post-deploy suite reads `max`
twice and applies `max-gate.ts` (`maxGateFailures`, unit-tested in the mock
suite by `max-gate.spec.ts`): the setting on, 3840×2160 under
`SP-program-MAX`, an adapter that is not the Basic Render Driver,
`running`, at least `MIN_BOUNDARIES` = 30 more boundaries out (the program
sends one per grid slot, standby pairs included), and `coalesced` (MAX
kept up with the program), `failed` and `device_resets` +0 in between,
a `vblank_output`, `vblank_tracking` and `send_off_grid` +0 (every
boundary on the wall's grid; a `vblank_state` `not_ticking` is named as a
dark output, #243), the median start after the vblank
(`send_phase_us_p50`) in [`vblank_phase_us`, + `SEND_PHASE_SLACK_US`
1 500 µs], #251: while `vblank_state` is `ticking`, the median compose
start (`compose_phase_us_p50`) in [`compose_phase_us`, +
`COMPOSE_PHASE_SLACK_US` 1 500 µs] (both read modulo the refresh,
`phaseInRefresh`; `compose_late` is logged, not gated), and
`slot_repicks` +1 at most (`MAX_SLOT_REPICKS`). The
first read comes after the first boundary went out, so the build's own
time never counts. A coalesce can also come from the PROGRAM: after a
program-side stall its thread serves several boundaries back to back, and
a burst of four or more overruns the 2-deep queue with MAX healthy. So the
spec logs `health.coalesced` and `health.timing.ready_late_us_max` from the
same two reads: a MAX coalesce next to a program coalesce or a late
`ready_late_us_max` is the program's stall, not a MAX fault. The p99s are logged, not gated; the budget
and Arena's side stay in the box gate below.

### The WARP proof (`program_max_tests_warp.rs`, Windows only)

A `ProgramOutput` (mock NDI) offers to a real `run_max_loop` whose GPU is
the production topology on WARP (#279 P1b): `Compositor::new_warp_with_size`
of the FHD target + `SpoutSender::for_target(…, max, "SP-program-MAX-s2-warp-test")`
of the MAX target the worker adds.
A 1280×720 Source boundary, then a 21:9 → 720p Mix boundary (slot 4 of 9):
Spout's registry lists the sender at 3840×2160 format 87, and its shared
texture read on a SECOND WARP device (`sp_gpu::read_shared_texture`,
doc-hidden, `win/receiver.rs`) matches S1a's reference within its tolerance;
after the stop the sender is unlisted. Each picture is served until both
senders sent it TWICE (`serve_twice`; the run's first picture one boundary
more, the FHD output being built after MAX's first send) and read after
that: the readback takes no Spout mutex, and the last draw's wait for the
GPU proves the earlier send's copy done (the last copy writes the same
bytes). #267: then a 4:3 Source boundary (960×720). MAX
fills its width: the layer covers columns 0..3840, and no pixel of either
edge column is black. The FHD sender keeps its bars, black at columns 239
and 1680. The FHD sender is `for_target` of the WARP compositor's primary
(#279 P1b; MAX's target is the one the worker adds, the production
`add_target` of `sp_gpu::Compositor`), so both read-backs come from one
compose. #279 P1b: then NDI `SP-program` from the GPU readback
(`check_ndi_from_the_readback`): a 1920×1080 `ProgramOutput` in gpu mode
on the same thread sends each boundary one boundary late as UYVY 1920×1080
stride 3840, and every byte is within `UYVY_TOLERANCE` of
`sp_gpu::uyvy::reference` of the FHD sender's texture (the same picture),
for a Source boundary and a fade; `ndi` reads `gpu`, one switch, 0
repeats / late / coalesced.

### The FHD Spout sender `SP-program` (#239)

Design: #239 "## Design" (the last comment). The FHD program goes out over
Spout too, so a local consumer (Arena, OBS with the Spout plugin) takes the
1920×1080 program without NDI.

- sp-gpu: `SpoutSender::new_fhd(&c, target)` = `SPOUT_FHD_SENDER_NAME` =
  `SP-program` (Arena: `SPOUT_SP-program`), the sender of the compositor's
  target `target`. #279 P1b: that target is the compositor's PRIMARY,
  `FHD_TARGET` (1920×1080, `FHD_FIT`; #251 step 2 had added it to MAX's
  compositor), never a second compositor: one device, one upload of each
  picture, one compose.
  `Compositor::with_size(w, h, fit)` / doc-hidden `new_warp_with_size` stay
  for tests (a compositor whose primary is another size).
  `read_shared_texture` reads at the size the sender's map lists.
- #267: its target places by `FHD_FIT` (`aspect_fit`, bars included),
  never MAX's fill. It is the same program as the NDI `SP-program`, which
  OBS, the Presenter and the stream take; the wall's Spout feed is MAX.
  Making the FHD feeds fill too would be one constant, but the NDI canvas
  (`program_canvas.rs`, CPU) would then have to follow, to stay one
  program.
- The `program-max` thread (`program_max_worker.rs`): while
  `Wants::fhd` (the setting AND MAX on), the FHD OUTPUT (its
  sender of the compositor's primary, `Output`, built on the thread:
  `MaxGpu::fhd_sender(&c, TargetId::PRIMARY)`) is drawn by the same compose
  as MAX, from the same labelled
  composition and the same uploaded textures (#251 step 2). The FHD
  output is built AFTER MAX's send (#239, "MAX goes first, whole": its
  Spout open never delays that boundary's MAX, at most the next compose,
  `compose_late`; rare: the start, a backoff's end, a lost device) and
  drawn from the next compose; the
  compose waits for `compose_at` and draws both targets; at `due` MAX's
  send runs, then the FHD one's (its `send_at` meets a passed instant).
  When MAX's boundary is not drawn (its build failed, lost the device or
  waits a backoff) the FHD output is built first and goes out at the due
  instant alone.
  Pinned by
  `both_targets_are_composed_at_the_compose_instant_then_sent_max_first`
  (a grid, the fake clock's waits [compose_at, due, due] once both are
  built, the fake's `order` at each wait),
  `the_fhd_output_is_built_after_max_s_send_then_drawn_by_the_same_compose`
  and `the_fhd_build_never_delays_max_s_send`.
  #279 P1b: the FHD target is the primary, drawn by every compose whatever
  its sender does; it is MAX's target that exists only while MAX's sender
  lives: a refused or failed MAX build drops MAX's sender and takes its
  target off (`drop_max`), so nothing is drawn for MAX during its backoff.
  A refused or failed FHD sender drops that sender only (`drop_fhd`); a
  refused or failed MAX sender never stops the FHD output, and the other
  way round. Switched off (`Wants::fhd` false), the FHD sender goes at the
  next boundary and its backoff is forgotten (`release_fhd`); MAX off with
  NDI on the canvas releases everything (MAX's sender, the FHD sender, then
  the compositor), MAX off with NDI from the GPU only MAX's sender and
  target (`release_max`).
- After its first good boundary the thread reads Spout's registry entry of
  the FHD sender once (`MaxSender::listed_size` →
  `spout_sender_info(name)`, a receiver's read; `None` = read again next
  boundary), so `max.fhd.listed_width/height` is what Spout lists, not a
  constant. A dropped FHD sender clears it.
- Setting `program_spout_fhd_enabled` (`sp_core::config`, ON unless
  `"false"`), applied with MAX's (`start_max` first, then the 5 s task,
  `program_max_fhd::apply_fhd_setting`). The one enable rule is
  `fhd_off_reason(fhd_setting, max_enabled)`: MAX off first (`max_off`),
  then its own setting (`setting_off`).
- `GET /api/v1/program` → `max.fhd {enabled, state, reason, spout_name,
  listed_width, listed_height, submitted, failed, sender_backoffs,
  spout_relists, draw_us_p99, send_us_p99}` (`FhdStatus`;
  `program_max_fhd.rs`, a CHILD module of `program_max.rs`: it reads the
  queue and the private `Window`). #251 step 2: no `upload_us_p99` (the
  FHD target uploads nothing of its own: MAX's upload serves both, and
  `max.upload_us_p99` / `max.draw_us_p99` cover the whole compose);
  `draw_us_p99` is the FHD target's own part of the draw
  (`record_fhd_sent(draw_us, send)`). `fhdGateFailures` never read the
  upload figure; `max-gate.ts`'s `FhdStatus`, the mock and the post-deploy
  log no longer name it.
  `state` = MAX's `state_label` over the FHD side's own phase (attach /
  end / unsupported follow MAX's) with `reason.is_none()` as "on". Its
  own lock, never held with the queue's or MAX's stats lock.
- Nastavenia: fieldset `settings-spout` "Výstupy Spout (Resolume)":
  `settings-max-enabled` (`program_max_enabled`) and
  `settings-spout-fhd-enabled` (disabled while MAX is unchecked, with the
  note `settings-spout-fhd-hint`). A save sends a switch only when the
  checkbox differs from the value the page loaded
  (`sp_core::config::{program_max_to_send, program_spout_fhd_to_send}`,
  #229's `paid_ai_to_send` rule): a stale tab never flips one back. Mock spec
  `e2e/settings-spout.spec.ts`; the mock serves `max.fhd` with the same
  reason rule and `state: "unsupported"`.
- Live gate: `post-deploy-max.spec.ts` waits for both senders' first
  boundary and the listing, then for 30 more on BOTH counts, and applies
  `fhdGateFailures` (`max-gate.ts`, unit-tested in `max-gate.spec.ts`):
  setting on, no reason, `running`, listed 1920×1080, ≥ 30 out, `failed`
  and `sender_backoffs` +0. It runs on SNV and in PP's subset.
- Tests: `program_max_fhd_tests.rs` (rule, records, JSON names, setting,
  task), `program_max_worker_tests_fhd.rs` (the fake GPU: ONE
  `FakeCompositor` whose primary is the FHD target and which draws MAX's
  while one was added, `Log` `targets_added` / `targets_removed` /
  `fhd_drawn` / `fhd_*` / `order` ("fhd sender built", "fhd draw", "max
  draw" — the primary first —, "max send", "fhd send", "max target
  removed"; #279 P1b also "ndi on", "ndi read", "ndi off"),
  `Script.add_target` / `fhd_sender` / `fhd_send` / `listed`: the FHD output
  built after MAX's send, both targets at `compose_at`, MAX first, FHD off
  sends nothing, an FHD refusal or failed build never stops MAX, a failed
  MAX target or sender waits MAX's own backoff while the FHD output goes
  out),
  `program_max_worker_tests_fhd_lost.rs` (a lost device anywhere, building
  the FHD output first or after MAX's send included, rebuilds both; a loss
  after MAX's boundary went out leaves it sent and MAX's state and log name
  it; a MAX boundary sent before the loss never counts after it), WARP:
  sp-gpu `tests/warp.rs`
  (one compose fills both targets, one upload per picture, a 4:3 picture
  fills MAX's width and keeps FHD's bars, a removed target), `tests/spout.rs`
  (the `SP-program` sender of the FHD target listed at 1920×1080 and read
  back on a second device; two senders on one compositor, each listed at
  its own size) and sp-server `program_max_tests_warp.rs` (the real loop
  with both senders on one WARP compositor).

### The box gate after the deploy (the main session runs it)

- the post-deploy spec above passed (`running`, `submitted` rising,
  `max.adapter` names the RTX 3070 Ti), and Arena's `GET /api/v1/sources`
  lists `SPOUT_SP-program-MAX`
  (category "Spout Servers"); `spout_sender_info("SP-program-MAX")`:
  3840×2160, format 87, host path = the installed `SongPlayer.exe`;
- SP-program's `health.timing` (`ready_late_us_max`,
  `vban_feed_late_us_max`, `submit_us_max`, the `_over_*` counts) and
  FOH's VBAN telemetry (#233: `outputs[i].vban` of the entry whose
  `vban.targets[0].target` is `fohabl.lan:6980`; `late_sends`,
  `late_max_us`, `blocks_dropped`) are unchanged against the pre-deploy
  numbers over the same window;
- the MAX p99s are within budget: `upload_us_p99 + draw_us_p99 +
  send_us_p99 < 10 000` µs, `max.coalesced` +0 and `failed` +0 outside a
  restart;
- a scratch Arena layer showing the Spout source holds Arena's FPS
  (composition saved and restored; Bridge.avc saved and restored);
- the CI step proved the exe's embedded manifest (Common-Controls 6.0), and
  the installer carries `resources/THIRD-PARTY-NOTICES.txt`;
- #239: Arena lists `SPOUT_SP-program` too, `spout_sender_info("SP-program")`
  reads 1920×1080 format 87, and MAX's budget and `coalesced` +0 hold with
  both senders on (#251 step 2: `max.draw_us_p99` is then both targets'
  draw).
- #251 step 2: the same A/B as comment 6192123775 (`fhd_ab2.ps1`, 46
  one-minute blocks, a whole drift cycle) must read FHD on like FHD off,
  both at or under the off-rate of 3.09 jumps back per minute; then the
  continuous monitor on normal songs.

## Telemetry (`ComposeStats`, behind `max.*_p99`)

- `upload_us`: the CPU time of this call's uploads, texture creation
  included.
- `draw_us`: from the first draw command until an event query says the GPU
  has finished the frame. The wait spins with `yield_now` (SwitchToThread:
  any ready thread runs first), bounded at 10 s (`GpuError::Timeout`). A 4K
  quad on the RTX is well under a millisecond. If S2 measures the spin as a
  cost, it can read the query a boundary later instead: Spout's copy is
  ordered after the draw on the same context and needs no wait. #251 step
  2: every target drawn (one query per target, one flush, one wait;
  #279 P1a: a UYVY pass and its staging copy after them, its own query);
  `Compositor::draw_us(id)` = a target's own part (`target_draws_us`), the
  FHD one is `max.fhd.draw_us_p99`.
- `uploads`: 0, 1 or 2 pictures uploaded, whatever the targets.
- #279 P1a `uyvy_us`: the UYVY pass's own part of the draw (its pass and
  staging copy, after the targets' parts), `None` with no pass enabled.
  #279 P1b publishes it as `ndi.uyvy_us_p99` (over the last 900
  readbacks).
- #279 P0 `upload_bytes`: the plane bytes those uploads wrote
  (`Planes::upload_bytes`: luma a byte a texel, chroma two a U/V pair, the
  stride's padding not counted; 0 when every picture was resident). The
  worker records it with `uploads` for EVERY compose that ran, at the
  instant it served the boundary (`MaxWorker::draw` →
  `MaxOut::record_uploads`, `program_max_upload.rs`, a child of
  `program_max.rs`): a boundary whose Spout send then failed moved its
  bytes all the same; a failed compose is not counted. `max.upload_mb_s`
  (10^6 bytes) and `max.uploads_per_s` = everything after the oldest of
  the last 900 composes, at most `UPLOAD_RATE_SPAN` (30 s) before the
  newest, over the time since it, to a tenth. While the compositor
  composes nothing (MAX off with NDI on the canvas, quiesced, a backoff)
  they keep their last value: read them with `max.state` `running`. #279
  P1b: with MAX off and NDI from the GPU the FHD composes still upload, so
  they move while `max.state` reads `off`. One upload serves both targets: the FHD sender
  adds none (`max.fhd` has no upload figure). The GPU-resident path's
  number: `upload_mb_s` = 0 with a hardware-decodable song on program.
  Tests: the window on made-up instants (`program_max_upload_tests.rs`),
  the worker over the fake GPU (`program_max_worker_tests_upload.rs`; the
  fake's compose uploads every picture of its composition by sp-gpu's
  rule, it keeps no residency), the WARP pins in `tests/warp.rs`.
- `SpoutSendStats::send_us` (S1b, for `max.send_us_p99`): the shim's
  `SendSynced` (the sender-mutex wait, the copy, the flush, the wait until
  the GPU has DONE that copy, then the announce; at the first send also the
  shared texture's creation and the registration; #251 step 3 moved the
  #223 follow-up's wait from after `SendTexture` to before the announce), so
  `max.send_at_us_*` is when Spout's shared texture really holds the frame,
  relative to the program's offer: its spread is the GPU's delay of the
  copy (Arena reads the texture on its own 60 Hz clock).

## The capability probe (#279 P0, `probe.rs` + `win/probe.rs`)

The GPU-resident design of epic #279 (its plan §4.2–§4.3; the P0 design
comment 6215400662) shares NV12 textures from per-playlist producer devices
to the one compositor device. `probe_gpu()` asks the picked adapter whether
that holds, on TWO devices of it (A = a producer, made with the video API
like Media Foundation's decode device, `DeviceUse::VideoDecode`; B = the
compositor, `DeviceUse::Probe`: BGRA, adapter list at DEBUG; `probe_warp` and
`probe_on_listed_adapter` make A BGRA too, since WARP refuses the video
API), each step on its own (a step that needs the shared texture makes its
own):

1. `extended_resource_sharing`: `CheckFeatureSupport(D3D11_OPTIONS)`
   `.ExtendedResourceSharing` on A and B;
2. `nv12_shared_nthandle`: a 256×144 NV12 texture on A,
   `BIND_SHADER_RESOURCE`, `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`,
   `IDXGIResource1::CreateSharedHandle`, `ID3D11Device1::OpenSharedResource1`
   on B (the NT handle closed after the open);
3. `nv12_keyed_mutex_roundtrip`: A `AcquireSync(0, 1000 ms)`, copies an
   NV12 picture in (`probe_picture`: byte i = i mod 251), flushes,
   `ReleaseSync(1)`; B `AcquireSync(1, 0 ms)`, copies the texture to an NV12
   staging copy, maps it (the UV plane at `RowPitch × Height`), releases
   key 1, and its packed bytes must be A's (`content_step`): sharing proven
   by content, not only by an open. **`AcquireSync`'s `WAIT_TIMEOUT`
   (0x102) and `WAIT_ABANDONED` (0x80) are SUCCESS codes: the `windows`
   crate's `Result` reads them `Ok`.** The glue calls the vtable entry for
   the raw HRESULT and the pure `acquire_step` accepts only `S_OK`;
4. `nv12_srv_r8_r8g8`: R8_UNORM and R8G8_UNORM views on B's opened NV12;
5. `uyvy_target_and_staging_do_not_wait`: on B an R8G8B8A8 960×1080 render
   target, cleared, copied to staging, the copy waited for (an event query,
   `pipeline::wait_until_each_done`), mapped `READ` +
   `D3D11_MAP_FLAG_DO_NOT_WAIT`, its first texel read back as the clear.

Each step is `Ok` or a `ProbeError { call, hresult, detail }` (hresult 0 =
the call succeeded but answered no); `ProbeReport::verdict` =
`gpu_resident_ok`, or `needs_fallback` naming the first failed step in the
report's order (the design's R1 fallback). A device that cannot be made
fails the whole probe (`GpuError`). `GpuProbe` also carries the adapter,
every adapter DXGI lists and the adapter's local VRAM
(`IDXGIAdapter3::QueryVideoMemoryInfo`, `Vram {budget_mb, used_mb}`, or the
error). Pure + serde in `probe.rs` (Linux-tested, `probe_tests.rs`); the
stub's `probe_gpu` is `Unsupported`; `probe_warp` / `probe_on_listed_adapter`
are doc-hidden, Windows only. The box runs it through
`POST /api/v1/diag/decode-bench {"probe": true}` (`diag-bench.md`).

WARP (`tests/probe.rs`): two WARP devices, the listed Basic Render Driver
and the picked adapter (none on `windows-latest`: `NoAdapter`). What WARP
answers per step is NOT documented, so the test asserts only the proven
(the devices, their adapter, a definite answer naming its call per step, a
verdict consistent with the steps) and writes every answer, the verdict and
the report's JSON to stderr: read WARP's answer in the CI log.

## The UYVY pass (#279 P1a: `uyvy.rs`, `uyvy.hlsl`, `win/uyvy.rs`)

Design: #279 comment 6216639388; plan
`docs/superpowers/plans/2026-10-16-gpu-video-path.md` §4.4, §5.5. #279
P1b wires it: NDI `SP-program` from the readback, one slot late, the `UYVY`
FourCC, the burn on UYVY (next section), switched by `program_ndi_source`
(default: the CPU canvas, `program_canvas.rs`, `nv12_mix.rs`).

- **What it makes.** `Compositor::enable_uyvy(target)`, for a target of
  EVEN width (`uyvy::uyvy_size`, else `GpuError::UyvyOddWidth`; an id the
  compositor does not hold is `NoObject`): an `R8G8B8A8_UNORM` texture of
  width/2 × height (one texel per pixel pair) and its RTV, an SRV of the
  target's BGRA texture, an immutable 48-byte cbuffer
  (`uyvy::constants_bytes`), its own event query, and `UYVY_STAGING` = 3
  staging textures (`CPU_ACCESS_READ`). One pass per compositor: enabling
  again replaces it (a new ring: nothing to read before the next compose),
  `disable_uyvy` drops it, and `remove_target` of its target drops it too.
  A refused or failed enable leaves the compositor as it was.
- **The pass.** One full-target triangle (`SV_VertexID`, no vertex buffer,
  `pipeline::raster`: no culling). The pixel shader at texel (x, y)
  `Load`s the target's pixels (2x, y) and (2x + 1, y), no filter, and
  writes `saturate(float4(U, Y0, V, Y1))`: a texel's four bytes in memory
  are one UYVY pair. Y′ is each pixel's own; Cb/Cr are the conversion of
  the pair's AVERAGE RGB (the conversion is linear, so this equals the
  average of their Cb/Cr): a chroma sample centred between the pair, not
  BT.709's co-sited one (the design's choice: two exact `Load`s, no
  filter tap).
- **Binding order matters.** The pass binds its own RTV FIRST, which takes
  the program target off the output stage, and only then the target's SRV:
  D3D11 force-unbinds an input that is still bound as an output, and the
  pass would read zeros. Both are unbound after the draw. Its blend state
  is the default (`OMSetBlendState(None, ..)`): the compositor's blend
  adds onto the last frame AND writes R, G, B only, so the alpha byte
  (Y1) would never be written.
- **In `compose`.** After the targets' draws: the pass, a `CopyResource`
  into `staging[ring.slot_to_write()]` and its event query, all inside the
  ONE flush and wait (`wait_until_each_done`: the targets' queries, then
  the pass's). Only once that wait passed and `GetDeviceRemovedReason` is
  clean does the ring move on (`StagingRing::written`), so a failed compose
  leaves the newest copy readable. `ComposeStats::uyvy_us` is the pass's
  own part of the draw (the pure `stats::split_uyvy_part`: the part after
  the targets' in `target_draws_us`), `None` with no pass;
  `ComposeStats::draw_us` then covers the pass and its copy too.
- **`read_uyvy(&mut out)`.** `Ok(false)`, `out` untouched, before any
  compose finished since the enable. Otherwise it maps
  `staging[ring.newest()]` with `READ | D3D11_MAP_FLAG_DO_NOT_WAIT` (the
  copy is complete: the compose waited) through `textures::map_rows_into`
  (the compositor's mapped-rows reader, `read_back`'s too; it refuses
  more rows than the texture has: `out` cleared once mapped,
  `unpad_rows_into`, width × 2 bytes a row, packed, unmapped), `Ok(true)`.
  On `Ok(false)` or an error `out` keeps what it held: P1b must never
  send it then.
  `DXGI_ERROR_WAS_STILL_DRAWING` is a `GpuError::Api` whose call
  (`uyvy::STILL_DRAWING_CALL`) names it, never a wait (`uyvy::map_failed`;
  a lost device stays `DeviceLost`). No pass is `NoObject`.
- **The colour.** `uyvy::RGB_FULL_TO_BT709_LIMITED`: rows Y′, Cb, Cr over
  the full-range `(r, g, b, 1)`, already divided by 255 (the UNORM the
  target stores). It is derived from `KR` / `KB` and is the inverse of
  `color.rs`'s `BT709_LIMITED_TO_FULL`. It reaches the shader as f32
  (`uyvy::rows_f32`, `constants_bytes`); `uyvy.hlsl`'s `cbuffer Uyvy` is
  `to_y`, `to_u`, `to_v`, in that order.
- **The CPU model and its tolerance.** `uyvy::pack_pair(left, right)`
  (BGRA in, `[U, Y0, V, Y1]` out, f32 like the shader, then
  `reference::unorm8`) and `uyvy::reference(bgra, w, h)`. `UYVY_TOLERANCE`
  = 1 code: two exact `Load`s, f32 maths, one rounding per byte. The WARP
  pins compare with the reference computed from the SAME target's BGRA
  readback, so the composite's own tolerance never adds to it, and any
  picture works (smoothness is the composite's premise, not this one's).
- **Tests.** Linux `uyvy_tests.rs`, every pin from a scratch Python model
  with numpy f32:
  - the rows from KR/KB, the inversion within 1e-9 both ways round, the
    f32 pins and the cbuffer bytes;
  - the HLSL source read for the cbuffer order, the two `Load` offsets,
    the average and the output order;
  - the BT.709 bars → their limited-range codes (`color_tests.rs`'s table
    the other way), mixed pairs, alpha never read;
  - the reference's rows, a short frame and an odd last column;
  - the size rule, the map error, and the ring (wrap, read before any
    write, write after a read).

  WARP `tests/uyvy.rs`, on FHD targets: a 1440p picture; a 4:3 picture
  whose bar pairs are exactly `[128, 16, 128, 16]` and whose picture pairs
  are not, in rows 0, 540 and 1079 (a scratch model of the composite puts
  each ≥ 11 codes from it there); a 50 % fade; Black. Each is within ±1 of
  the reference of its own BGRA readback. Also: 10 composes in a row of
  THREE pictures in turn, each `read_uyvy` `Ok(true)` with that compose's
  picture (the ring wraps 3×, never `WAS_STILL_DRAWING`; with two pictures
  over a ring of three, a read of the slot being written would hold the
  same picture and pass); `Ok(false)` before a compose; an odd width
  refused; the production shape (MAX's compositor with `FHD_TARGET` added,
  the pass on it), where a removed target takes its pass; removing ANOTHER
  target keeps the pass and its newest copy; disable and re-enable start
  afresh. The worst difference goes to stderr: read it in the Build
  (Windows) log.
- **For P1b (review round 1, a property, not a defect).** The NDI picture
  then goes NV12 limited → full-range 8-bit BGRA (saturated) → limited
  UYVY. Sub-black, super-white and out-of-gamut values clip where today's
  CPU NV12 canvas passes them through, and the two roundings cost about
  ±1 code against today: P1b's colour and A/V gates must expect it.

## NDI `SP-program` from the GPU readback (#279 P1b)

Design: #279 comment 6218637444; plan
`docs/superpowers/plans/2026-10-16-gpu-video-path.md` §4.4, §5.1, §5.5,
§5.6, §5.8, §5.9, §8 R2, §9 P1b. The NDI picture was a CPU canvas picture
(a fit, or a fused fade on the band pool: ~6 MB of RAM traffic per
boundary, at the drifting offer phase). The compositor already draws the
same FHD program, so NDI takes ONE 4 MB UYVY readback of it, made at the
fixed compose phase. Switched by `program_ndi_source`; a deploy changes
nothing on air (default `cpu`).

### Topology

- The production compositor's PRIMARY target is the FHD program
  (`SpoutGpu::compositor` = `Compositor::with_size` of `FHD_TARGET`),
  there for the compositor's life. MAX's 3840×2160 target is ADDED
  (`add_target(MAX_TARGET)`, `MaxWorker::make_max`) only while MAX is
  wanted, with its sender `SpoutSender::for_target(&c, max,
  SPOUT_SENDER_NAME)`, and taken off with it (`drop_max`: the sender
  first, then `remove_target`). The FHD Spout sender is `new_fhd(&c,
  TargetId::PRIMARY)`.
- The thread takes jobs while ANYTHING wants the compositor. Who wants
  it is ONE rule, `program_max::Wants` from one read of the queue
  (`Queue::wants`, `MaxOut::wants`): MAX's own output by its setting
  alone, the FHD Spout sender by its setting AND MAX on
  (`fhd_off_reason`), NDI's readback by `program_ndi_source` = `gpu`.
  `Wants::any` is what the queue's acceptance, its release and the setting
  changes read (`set_enabled(false)` / `set_ndi_source(cpu)` drop the
  queue only when nothing wants it, and wake the thread; with nothing
  wanted it releases everything as before), and the worker reads the same
  snapshot per job (`serve_lines`). Review rounds 3-4: the worker used to
  restate it from three reads under three locks, and `fhd_wanted` /
  `ndi_wanted` (test-only wrappers then) are gone. A job served
  while NOTHING wants the compositor (MAX off and NDI on the canvas: the
  setting changed after the thread took the job) builds and composes
  nothing, and the next step releases. The lane's first cut inferred MAX
  from the job (`max_wanted` = enabled OR NDI not on the GPU, "a job
  means MAX on"); that inference is exact only under the dequeue's lock,
  and read later it built MAX's 4K target and the `SP-program-MAX` Spout
  sender for one stray boundary with MAX off (resume lane,
  `a_job_served_when_nothing_wants_the_compositor_builds_nothing`). A
  worker test that serves a job switches MAX on itself
  (`max.set_enabled(true)`).
- MAX off with NDI on the GPU: MAX's sender and target go (`release_max`),
  the compositor keeps composing the FHD target for NDI, `max.state` reads
  `off` and MAX counts nothing.
- MAX's order (built first, sent first, whole, #239), its phases, its
  backoffs and its telemetry are unchanged.

### The readback (`program_max_ndi.rs`, a child of the worker)

- `MaxWorker::ndi_pass(wanted)`: P1a's UYVY pass is enabled on the PRIMARY
  while NDI is wanted, disabled otherwise (the canvas mode pays no
  readback). A failed enable is tried again only after
  `MAX_RETRY_BACKOFF` (it makes textures and compiles a shader).
- **A device lost at the pass's enable is lost there** (review round 5
  B5-1): `ndi_pass` calls `lose_device` (the compositor goes,
  `device_resets` +1), and `MaxWorker::device_ready` (= `build_device` +
  `ndi_pass`, the first step of `attempt`) fails the boundary as after a
  failed build: MAX's build never runs on the gone compositor, which
  would arm a spurious MAX backoff. Before, with MAX off nothing composed
  until the pass was on, so nothing reached the compose's own device
  check: the removed compositor was kept and its enable failed every 3 s,
  NDI on the canvas for good. With MAX on the compose caught it one call
  later. Round 6: the loss's kind follows the compositor's age
  (`building` = `device_ready` built it for this boundary). The enable of
  a fresh compositor is part of its build and waits the backoff. A
  compositor that was up (NDI flipped to the GPU while MAX runs) is lost
  like one a compose finds: rebuilt at the next job unless nothing went
  out since the last loss. Round 5's fix first made every such loss wait
  3 s (about 90 MAX boundaries), where the compose had rebuilt at once
  (`with_max_off_a_device_lost_at_the_pass_s_enable_is_rebuilt_after_the_backoff`,
  `a_device_lost_at_the_enable_of_a_running_compositor_is_rebuilt_at_once`).
  Lesson: catch a device loss at a call after which nothing on the same
  path would see it. With MAX off that is the enable, since nothing
  composes until the pass is on. A READBACK's loss is left to the next
  compose, which always runs while the pass is on (`read_back` only WARNs,
  `a_failed_readback_pushes_nothing_and_costs_max_nothing`): never "fix"
  that one too.
- The pass has ONE log: a `StateLog<PassLine>` of its state (`pass_line`:
  `Off` while NDI is not wanted, `On` while the pass is enabled, else
  `Failing(why)` with the last enable error, an `Arc<str>` so a failing
  boundary compares and stores it with no allocation), observed at every
  boundary the thread holds a compositor; the line reaches
  `Lines::ndi_pass`. An INFO when it goes on or off, a WARN per failing
  spell, a change inside the 5 s window said after it as the state is
  then, and no line outside it. Review rounds 3-4: an enable gate
  observed only at an attempt never said a recovery inside its window
  and stayed on the old failure, so a later spell with the same error was
  never WARNed; and INFO lines written at every enable and disable,
  outside the gate, could leave the log's last line contradicting the
  pass ("on" while it failed) and flooded it under device churn.
  Round 5 F2: every release (`release(now)`: nothing wants the
  compositor, a quiesce, the thread's end) observes `Off` too
  (`pass_released`, the one `say_pass`): with MAX off `serve_lines`
  returns before the pass once NDI is on the canvas, so the log's last
  line stayed "on" (`with_max_off_the_release_says_the_pass_is_off`).
  Like `LogGate` it can stay one change behind: a release inside the
  5 s window, or a pass gone with the device while the rebuild waits its
  backoff (nothing observes it then).
  The pass goes with the compositor (`NdiPass::forget` in `drop_gpu`): a
  rebuilt compositor enables it again.
- **The thread never lets the feed's pictures go** (review round 5 F1):
  neither when its pass goes off nor at a release. It usually reads the
  setting's switch to the canvas BEFORE the `SP-program` sender serves
  the next boundary (with MAX off the switch wakes it at once to
  release), so its clear took the held pair's picture and the flush
  repeated the one before: review A's timing model put it at 99.6 % of
  switches with MAX off, ~34 % with MAX on. The sender alone clears
  (the delay line below). Lesson: when two threads share a hand-off and
  a state change ends it, ONE side owns the cleanup, the consumer, which
  alone knows when it no longer needs the last item; a producer-side
  clear races the consumer's last take whichever thread reads the
  change first (`program_output_tests_ndi_switch.rs`: the worker on the
  fake GPU and the sender on one hand-off, the thread first, MAX on and
  off). A residual repeat stays (review round 6, both passes, ~0.1-0.2 %
  of switches with MAX off, ~5 % with MAX on in their models): the
  thread reads `wants()` when it TAKES a job, the sender reads the
  setting after the offer and the quiet wait, so a flip between the two
  reads holds b as a GPU boundary that the thread never reads back, and
  the flush at b + 1 repeats one picture (no audio lost; ROZHODNUTÉ
  6232669201 item 1 counts it as a repeat). The structural fix, if a
  manual flip's one repeat ever matters: the sender decides the
  boundary's NDI source once, before `offer_max`, and the `MaxJob`
  carries "read me back".
- After each compose, at the compose instant and BEFORE the Spout sends
  (a 67 ms Spout lock wait never delays NDI): `read_back` maps the newest
  staging copy (`read_uyvy`, never a wait) into a `frame_pool` buffer of
  `uyvy_len(1920, 1080)` = 4 147 200 B and pushes it into `MaxOut::ndi`
  under the boundary's stamp, with the map + copy time and
  `ComposeStats::uyvy_us`.
- A `MaxJob::Black` maps nothing: the feed gets ONE cached constant UYVY
  black (`BlackUyvy`, every pair `[128, 16, 128, 16]`).
- Nothing is pushed for a boundary that is not composed (coalesced, a
  failed compose, a lost device, a backoff) or whose readback fails (one
  WARN per `MAX_LOG_EVERY_100NS`): the `SP-program` sender repeats.
- The compose runs with no Spout output to go out while the pass is ON
  (`attempt`: `max_ok || fhd_ready || self.ndi.on`). The design said
  `|| ndi_wanted`: a pass that could not be enabled reads nothing back, so
  that compose would draw for nobody.
- A readback that came proves the device like a sent boundary: it clears
  `lost_unsent` (with MAX off nothing else goes out), so a later loss
  rebuilds at once; a failed readback proves nothing.
- With MAX off, NDI's compose is the compositor's only one: a failed build
  or compose is logged like MAX's (`MaxWorker::finish_ndi`, a third
  `LogGate`, `Lines::ndi`: one WARN `the compositor does not compose for
  NDI (MAX off)`, one INFO when it composes again), never hidden behind
  MAX's `off`. With MAX on, MAX's own line names it.

### The feed (`ndi_feed.rs`, pure, `MaxOut::ndi`)

- `FeedCore`: a stamp-keyed map of at most `NDI_FEED_DEPTH` = 3 pictures,
  over it the OLDEST goes (`feed_coalesced`); `take(stamp)` returns that
  stamp's picture and drops every older one; a push at or before the last
  stamp served is dropped (`readback_late`); `newest()` = the stamp of the
  last picture KEPT (the readback's freshness). A late picture never
  entered the feed, so a readback that only ever arrives late leaves it
  behind and NDI falls back to the canvas after 30 slots (review round 1:
  counting late pushes kept a frozen NDI "fresh" for good).
- The p99s are MAX's `Window` (`program_max::Window`, the last 900); a
  Black boundary's constant black is no readback and adds to neither.
- `NdiFeed` = the core + NDI's telemetry under ONE short lock per call:
  neither thread waits on the other.

### The delay line (`program_output_ndi.rs`, an impl split of `ProgramOutput`)

- `serve` = `split` → `limit` → `feed_outputs` (VBAN, b) → `offer_max`
  (b) → the quiet wait → `submit_ndi`.
- `ndi_source_for(setting, newest, b)` (pure): `gpu` while the setting
  says `gpu` AND the feed's newest picture is at most
  `NDI_FEED_STALE_SLOTS` = 30 grid slots (1 s) behind b; else `cpu` with
  its why (`setting`, `no_readback`, `stale`). A compositor that cannot
  run (off Windows — every Linux test and the CI box —, no adapter, a 3 s
  device backoff) never freezes NDI. A readback that keeps arriving late
  goes `stale` too: a late picture never enters the feed.
- **The way back after a fallback** (ROZHODNUTÉ 6232669201 item 3,
  `ndi_feed::ReturnGate`, `NDI_RETURN_ON_TIME` = 30). A fallback from the
  GPU for any reason but the setting arms it. While armed, every boundary
  on the canvas takes the boundary before's readback from the feed exactly
  as the GPU path would (`take_on_time`: whole UYVY for the canvas; the
  take at the returning boundary is the last picture there), and counts
  the on-time ones in a row; a late or a
  missing one starts the count again. A GPU pick before 30 in a row stays
  on the canvas (`CpuWhy::Settling`). So a fallback never flaps boundary
  by boundary: before the gate, the readback's first kept push after a
  `stale` fallback brought NDI back at once (nobody took pictures on the
  canvas, so every push was "fresh"), and a readback that stayed late
  fell back again 30 slots later, round after round. The return to the
  GPU and the setting's canvas disarm it; the setting's flip to `gpu` is a
  first entry, not a return: it goes to the GPU as soon as the newest
  picture read back is fresh, as designed. A quick gpu → cpu → gpu flip
  within 1 s enters at once on the freshness of the pictures from before
  (every canvas boundary under the setting let them go, `newest` stays), so
  its switch's take finds nothing and a missing picture is the black
  until the readback comes back (or the stale fallback after 30 slots).
  On the canvas after a fallback `readback_late`
  keeps counting the late pictures and `feed_coalesced` no longer climbs
  (the canvas takes them).
- cpu: `submit_video` as before (the NV12 canvas), stamped b.
- gpu: the pair held at b − 1 goes out: its audio after the limiter
  (moved in, no copy; the standby silence for a standby pair), its stamps
  (a forwarded pair's audio stamp; the program's own block on its
  boundary), its item frame (the burn), and `feed.take(b − 1)` as UYVY,
  the canvas's size, stride `width · 2` (`PixelFormat::Uyvy`). Then b is
  held. A picture that did not come, or is not whole UYVY for the canvas,
  repeats the last one sent (`ndi.repeats`), the constant black before
  any: never a wait. No canvas fit, no fade picture, no band-pool work: a
  fade's audio is crossfaded in `split`, its picture is the compositor's
  fade; a mixed boundary joins `MixRun.boundaries` when it goes out
  (`fitted` and `max_picture_us` stay 0).
- A switch to the GPU holds b and sends nothing (one missing frame).
- **A switch to the canvas never drops an audio block** (ROZHODNUTÉ
  6232669201 item 1; the owner's rule: popping audio is the worst
  failure). The held pair goes out FIRST through `send_held` (its
  readback, else the last picture, else the black: a repeat), then b on
  the canvas: two submits back to back, contiguous stamps. The lane's
  first cut dropped the held pair, which lost b − 1's limited 33 ms block
  on `SP-program` (cg OBS, the stream) at every setting flip and every
  automatic fallback; VBAN / FOH never lost it.
- **The feed's pictures are let go by the sender alone** (review rounds
  4-5): at EVERY canvas boundary under the setting (`CpuWhy::Setting`),
  right after the held pair's flush took its picture (`NdiFeed::clear`;
  pinned with `repeats == 0` in
  `the_setting_s_switch_lets_the_feed_s_pictures_go`). Every boundary,
  not only the switch: the thread can push one more picture after the
  switch (it read the setting late), and nothing else would let it go.
  An ARMED fallback (from the GPU) does not clear: the gate takes the
  boundary before's picture at every canvas boundary, and every older one
  goes with it. A first entry's `stale` / `no_readback` canvas neither
  clears nor takes: a picture kept there is fresh, and moves NDI to the
  GPU at the next boundary.
- The last picture (what a missing one repeats): every boundary on the
  canvas forgets it (after the held pair's flush; no 4 MB picture stays
  pinned while NDI is on the canvas). A switch to the GPU takes the
  boundary before's readback as the last picture (`take_on_time`; after a
  fallback the gate already took it at that boundary), so a picture
  missing right after any switch to the GPU repeats the boundary before,
  the black only when the boundary before was not read back (an older
  picture the take dropped is not kept), or was let go by a canvas
  boundary between (a gpu → cpu → gpu flip within one boundary, which the
  5 s settings poll never makes; review round 5 B5-4). Review round 3: a first entry
  sent the black there, and round 2's "an automatic fallback keeps it"
  had no effect left once the gate's takes set it before every return.
- The lines (a `stat_window::StateLog<NdiSource>`, pure, the same
  mechanism as the worker's `LogGate`, which keeps a
  `StateLog<Option<String>>`; review round 3 deleted a second copy,
  `SwitchLog`): a change of source is said at
  most once per `NDI_SWITCH_LOG_EVERY_100NS` (5 s) of the timeline, and a
  change held back is said by the first boundary after the window, as the
  source is then (`held_back`), so the log never ends on a source NDI
  left. An INFO to the GPU or for the setting; a WARN for an automatic
  fallback (`reason`, `on_time_needed`): ROZHODNUTÉ 6233825414 (the
  fallback is a fault the operator should see), pinned by a log capture
  (`the_fallback_is_a_warn_and_the_way_back_an_info`, its lines spaced
  past the 5 s window). A fallback that heals inside the window after
  the last line is never WARNed (the log says only the state at the
  window's end): read `ndi.switches` and `repeats`, which count every
  switch, the setting flip included.
- `health.timing.submit_us` is the NDI submit alone from the GPU (at a
  switch to the canvas, the held pair's submit and b's canvas picture).
  The trace's `wire_100ns` is the wire stamp of the stamp NDI got
  (`TraceWriter::record_sent`; b's own when nothing went out, and b's own
  at a switch to the canvas, where b − 1 went out just before it).
- The quiet wait (`program_video_quiet.rs`) runs only while MAX is on
  (`MaxOut::enabled`): with MAX off nothing goes out over Spout for Arena,
  though the compositor still paces for NDI (round 2).
- The canvas is `FHD_TARGET`'s size in production (a const assert in
  `program_output_ndi.rs`); the tests use small canvases and fill the feed
  themselves.

### sp-ndi, the setting, the API

- `FourCCVideoType::UYVY` = `0x5956_5955` (= `receive::FOURCC_UYVY`),
  `PixelFormat::Uyvy` (`PixelFormat::four_cc`, one mapping for the sync
  and the async sends), `FrameSubmitter::submit_frame_at_boundary_owned(..,
  format)` (every caller passes `Nv12` but the delay line);
  `MockNdiBackend::last_async_video_fourcc` (asserted in sp-ndi too: the
  per-package mutation gate).
- `program_ndi_source` (`sp_core::config::program_ndi_source`): exactly
  `gpu` (trimmed) is the GPU, anything else the canvas; read with MAX's
  settings every 5 s (`program_max_phase.rs`, `load_ndi_source`), no
  restart. Temporary: deleted in P5.
- `GET /api/v1/program` (and the cut answer) → `ndi {setting, source,
  delay_slots, fourcc, repeats, readback_late, feed_coalesced, switches,
  readback_us_p99, uyvy_us_p99}` (`NdiStatus`; `source` = the last
  boundary's, `delay_slots` 1 from the GPU, `fourcc` `UYVY` / `NV12`; the
  p99s over the last 900 readbacks). The mock serves `setting` from its
  store and `source: "cpu"` (no GPU).
- The burn on UYVY: `program_burn::burned_uyvy` (`test-item-burn.md`).

### Tests

`ndi_feed_tests.rs` (the depth, take and drop-older, late, coalesce,
freshness, the 30-slot edge both ways, the gate's arming / count /
restart / disarming, the switch lines, the status and its 900-window),
`program_output_tests_ndi.rs` (over the mock NDI with a feed the test
fills: b's audio to VBAN at b and to NDI at b + 1 with b's stamps, the
repeat and the black, a short picture never sent, both switches with the
held pair sent first and contiguous audio stamps, stale, a readback that
stays late never flapping back, the 30-in-a-row return and its restart,
the first entry at once, a fade, the trace's stamp,
the first switch repeating the boundary before, the held pair sent
before the setting's switch forgets the last picture — read inside a
later submit through `HookedNdi`, whose hook sees the mock —, the switch's
`submit_start`, the feed let go at the setting's switch and at every
canvas boundary under it, the last picture back in the frame pool after a
fallback),
`program_output_tests_ndi_switch.rs` (round 5: the worker on the fake
GPU and the sender on ONE hand-off, the thread reading the setting's
switch first, MAX on and off — no repeat; the switch lines' levels by a
log capture), `program_output_tests_order.rs` (VBAN before any NDI call
from the GPU; `HookedNdi`), `program_max_tests_ndi.rs` (the queue's
rules, the setting), `program_max_worker_tests_ndi.rs` (the fake's
`order`: the readback after the compose and before MAX's send, Black,
MAX off, the pass off, the enable's backoff, a device lost at the
enable (a fresh compositor waits the backoff, a running one is rebuilt
at once), the pass log's on / off / failing lines, a quick recovery and
the release's "off", a failed readback, a lost device, `lost_unsent`, a
coalesce's gap, a job with nothing wanted, the feed left to the sender),
`stat_window_tests.rs` (`StateLog`),
`api/program_tests_ndi.rs`,
`program_burn_tests_uyvy.rs`, sp-ndi's FourCC tests, and the WARP proof
above.

### The gates on the box (the main session)

1. Deploy in cpu mode: every existing gate holds; the wall measured
   before and after.
2. Tell camera-box what changes on `SP-program` (plan §8 R2): the FourCC
   NV12 → UYVY, a constant +33.3 ms arrival lag, its genlock FIFO
   `shallow_depth` +1 expected and stable.
3. The delay's measurement (ROZHODNUTÉ 6232669201 item 2: the one-slot
   delay stays, and only a measured miss justifies a bounded wait or a
   2-slot delay; ROZHODNUTÉ 6233825414: NO `shadow` value). In `cpu`
   mode nothing reads back and nothing is taken, so `ndi.repeats` /
   `readback_late` stay 0: they say nothing before the flip. So the box
   check flips SNV to `program_ndi_source=gpu` for its measurement
   window, outside events, with the automatic fallback and its
   30-readback hysteresis as the safety net, and reads `ndi.repeats` /
   `readback_late` against `health.timing.ready_late_us_*` over a whole
   ~45 min drift cycle, binned by offer phase. In it: an NDI receiver
   reads 1920×1080 UYVY; `ndi.repeats` and `readback_late` +0 over songs
   and fades; the A/V gate (`post-deploy-av-sync.spec.ts`), `max-gate`
   and cg OBS's `genlock-fifo audit` hold. A miss flips it back to `cpu`.
4. The plan's §7 A/B: `program_ndi_source` gpu vs cpu, ≥ 46 one-minute
   blocks, binned by offer phase.

The colour (P1a review round 1): NV12 limited → full-range BGRA (clipped)
→ limited UYVY, about ±1 code against the canvas; sub-black and
super-white clip. The A/V and colour gates must expect it.

## The picture ring, `GpuFrame` and `Side::Gpu` (#279 P2)

Design: #279 comment 6237269720; plan
`docs/superpowers/plans/2026-10-16-gpu-video-path.md` §5.3, §5.4, §9 P2.
Each playlist will decode on its own producer device and copy every
picture into one slot of its pipeline's RING: an NV12 texture shared with
the compositor's device by an NT handle and a keyed mutex (the P0 probe
proved it). The picture then travels as a `GpuFrame` and MAX samples the
slot in place: no readback, no upload. P2 builds the ring and the
plumbing; NOTHING writes a ring in production until P3a (no runtime
behaviour change).

- **The decisions are pure (`ring_state.rs`, Linux-tested, gated):**
  - slots: `RING_CAPACITY` = 20 (the decode queue's 12 + 8); a slot is in
    use while a frame refers to it, written again only once free;
  - size classes (`SizeClass::of`: the format and the picture's sides
    rounded up to even): at most `MAX_SIZE_CLASSES` = 2 kept, the most
    recently written first; a free slot of a class no longer kept is
    emptied at once, an in-use one at its last drop; with no empty slot a
    write takes a free slot of the other class;
  - `RingState::take` → `Reuse {slot, alloc_gen, key}` / `Create {slot,
    alloc_gen}` / `Full` / `Dead`; `release(slot, alloc_gen)` →
    `Freed` / `Dropped` / `Stale` (a slot made again is another
    allocation: a stale drop changes nothing);
  - keys: a new texture's mutex holds `KEY_PRODUCER` (0), every release
    leaves `KEY_CONSUMER` (1) (`producer_key(written)`); the writer
    acquires the last key (`WRITER_ACQUIRE_MS`), the compositor key 1
    with 0 ms (`CONSUMER_ACQUIRE_MS`: never blocks);
  - `Acquired::of(raw HRESULT)` (the `windows` crate reads 0x102 / 0x80 as
    `Ok`, P0);
  - `copy_box(visible, surface)`: the even box inside the surface (a
    decoder's 16-row padded texture), none for an empty picture or a side
    over 16384;
  - the writer: `writer_step` (its free slot's key within
    `WRITER_ACQUIRE_MS` = 100 ms, = `RING_WAIT`; the compositor lets a key
    go before the frame it draws drops, so any answer but held kills the
    ring, with why), `make_texture` (a made slot, or no texture of the
    slot's allocation) and `stale_texture` (a slot emptied or made again);
  - the compositor: `slot_draw(SlotOpen, device_removed)` → `Draw` (held),
    `Black` (busy), `Gone` (a dead ring's slot: black, closed), `KillRing`
    (abandoned: a device was lost holding the key, the producer's or the
    compositor's own before its rebuild — a compose cut short by a GPU
    reset leaves the keys it held abandoned; the design's rule, P4
    decides whether such a slot is made again instead; or a failed
    acquire or open on a HEALTHY compositor), `DeviceLost` (a failed
    acquire or open while the compositor's own device is removed, read
    only then: the compose fails, the compositor is rebuilt), `why_killed`
    for the kill's WARN (with `Acquired::hresult`, the raw HRESULT; a
    failed open WARNs its error first); and `OpenedKeys` (a slot opened once per
    `SlotKey {ring, slot, alloc_gen}`, an older allocation of the same
    slot closed the moment the new one is drawn, at most `OPENED_MAX` =
    40 = 2 × `RING_CAPACITY` open, the least recently drawn closed first,
    and `begin_compose` closes every slot no compose drew for
    `OPENED_IDLE_COMPOSES` = 90: the compositor keeps no texture alive that
    its ring let go — a dead or dropped ring, a slot made again; an
    off-program ring's own textures stay its producer's).
- **`ring.rs`: the shared state and the frames.** `RingCore` = the state
  under a poison-tolerant `Mutex` + a `Condvar` a drop notifies; a write
  waits while every slot is in use (`wait_timeout_while(!has_room)`,
  `RING_WAIT` = 100 ms, counted `waits`), then `GpuError::RingFull
  {waited_ms}` (counted `full`); a dead ring is `RingDead` and wakes its
  waiter; its first kill WARNs once with why and the HRESULT that told it
  (`RingCore::kill`, from the writer or `GpuFrame::kill_ring`;
  `ring_state::hresult_text`). `hand_over` marks the slot written and
  makes the `GpuFrame(Arc<FrameInner>)`: a process-wide id (never reused: MAX's
  label), the picture's size, its slot texture's even size, `key()`, a
  doc-hidden `shared_handle()`; the LAST clone's drop frees the slot;
  `keep` (Windows: the NT handle) lives exactly as long. `FakeRing`
  (doc-hidden, every platform) takes and frees slots exactly like the real
  ring, its handle `FAKE_HANDLE_BASE` (`isize::MIN`) `+ 4·(slot + 1)`, a
  value no Windows handle has: sp-server's tests use it.
- **`win/ring.rs` `PictureRing` (glue, out of the gate):**
  `copy_from(source, subresource, visible)` refuses, before any slot is
  taken, a source that is not NV12 (`RingSourceFormat`), a subresource
  the source does not have (`missing_subresource` → `RingSubresource`:
  Direct3D would drop the copy silently) or a picture with no copy box
  (`RingPicture`); then it takes a
  slot, makes its texture when told (`BIND_SHADER_RESOURCE`,
  `MISC_SHARED_NTHANDLE | MISC_SHARED_KEYEDMUTEX`, `CreateSharedHandle`),
  acquires the slot's key (the raw HRESULT through the vtable,
  `writer_step`), `CopySubresourceRegion`s the even box (a texture-array
  slice: the WARP test
  `a_slot_is_copied_out_of_a_slice_of_a_decoder_like_texture_array`),
  `Flush`es, `ReleaseSync(1)`, hands over. A key not held,
  or a failed release, kills the ring; a failed write gives the slot
  back.
- **The compositor (`win/sides.rs` `RingSlots`, glue).** Each compose
  first closes the idle slots (`begin_compose`), then every ring slot of
  `Composition::frames()` (each slot once, a fade's two sides of one slot
  too): a dead ring's is `SlotOpen::Dead`; else opened ONCE on its device
  (`OpenSharedResource1`, R8 + R8G8 views, `gpu_opens`) and acquired with
  key 1 / 0 ms; `slot_draw` decides, `ComposeStats::count_slot` counts:
  `gpu_sides` (drawn), `km_acquire_timeouts` (busy), `abandoned` (a dead
  ring's, abandoned, a failed acquire), `open_failures` (not opened);
  held slots are released after the GPU wait. Only `Composition::uploads()`
  (the CPU sides) are uploaded; residency ids are the CPU labels only (a
  ring frame's id is another numbering: never compared with them).
- **`Side { Cpu(Nv12Picture), Gpu(&GpuFrame) }`** is a layer's picture
  (`Composition::Picture(Side)`, `Fade {from, to}`). `quad::uv_in_texture`
  scales a layer's uv by picture / texture size, so the quad spans the
  picture's own texels in its even slot (the identity for an uploaded
  picture); the bilinear filter clamps at the TEXTURE's edge, so an
  enlarged odd picture's last column (row) blends up to half a texel of
  the slot's extra one. `reference.rs` reads `None` for a ring layer: model
  the CPU picture written into the slot.
- **sp-server: `playback/picture.rs`.** `Picture { Cpu(SharedFrame),
  Gpu(GpuFrame) }` is the `video` of `PacedFrame`, `SubmitJob`,
  `FrameBlend`, `MaxPicture`, the producer's `Held`, the pre-roll's
  `StandbyBlack` / `FillFrame`, `Standby::Black` and the paced consumer's
  `FillPicture`; every hop clones it by `Arc`, `ptr_eq` is an identity per
  kind (never across kinds). `MaxPicture::side(id)` = an upload or the
  frame itself; `PictureIds` labels a ring frame by `GpuFrame::id` and
  spends no counter id on it (only CPU pictures are held by allocation).
  The CPU-only consumers read through `Picture::cpu_for(CpuUse, &mut
  CpuMisses)`: `SP-program`'s canvas shows black (a blend partner's side
  black), a held step's preview tap gets no picture but its audio
  (`DecodeTaps::offer_audio`); each miss is counted per consumer and
  WARNed at most once a minute of the timeline with the count
  (`stat_window::WarnLimiter`, `CPU_MISS_WARN_EVERY_100NS`), never a
  panic (`ProgramOutput::canvas_misses` is the tests' view,
  `DecodeProducer::preview_misses`). The standby blacks stay
  CPU. Tests read CPU bytes through the test-only `Picture::expect_cpu`.
- Not on the API yet: `ComposeStats::{gpu_sides, gpu_opens,
  km_acquire_timeouts, abandoned, open_failures}`, `RingStats` and the
  CPU misses (P3a's telemetry: nothing writes a ring before it, so they
  read 0 until then; the ring's death and the misses WARN meanwhile).
  `RING_CAPACITY` = `pipeline_paced::DECODE_QUEUE_BOUND` + 8 is pinned by
  a Windows test (`picture_tests.rs`).
- **Tests.** sp-gpu: `ring_state_tests.rs`, `ring_tests.rs` (a write
  waits for a drop, threads with a safe-direction window), the `Side` /
  `uv_in_texture` / reference / `count_slot` pins, and WARP
  `tests/ring.rs` (its NV12 sources shader-resource bound and made with
  initial data, the two NV12 textures P0 made on WARP, in one): a slot written on one device drawn on another
  within S1a's reference, opened once and uploading nothing; slice 1 of
  a 2-slice 640×368 array (WARP makes an NV12 ARRAY of at most 2 slices,
  at any size: 3 or more is E_INVALIDARG — the first CI of P2,
  39066250195, failed on a 4-slice one; measured on SNV with a WARP
  device through Python ctypes, `diag\wall-bridge\warp_probe2.py`; a
  refusal fails that test, never skips it — read the Build (Windows) log); a fade of an upload and a slot, and of two
  slots; a slot held by a third device drawn black and counted, then
  drawn; a full ring's `RingFull` then a reuse; a new size written over
  a free slot of the other (made again, drawn); the refusals; an odd
  picture's even texture. sp-server (`FakeRing`): `picture_tests.rs`,
  `pacer_tests_gpu.rs` (emit / repeat / blend / frozen / seek hold carry
  the same frame, every slot free after), the consumer's fill, the bus,
  `ProgramOutput`'s MAX offer and canvas miss, `PictureIds` and the
  worker (`program_max_worker_tests_gpu.rs`), a held ring frame's
  preview.

## Arena loses its OWN frames while it receives `SP-program-MAX` (#251, 19.10.2026)

The owner saw the wall judder through the 18.10 service, while every A/B
since #282 read 0–2 `exact_backs`. The judder was **missing frames**, not
backs, and it hit every layer: the worship NDI background, Arena's clips.

Measured on SNV (`diag\wall-bridge\sp_onoff_ab.ps1`, `max_ab.ps1`,
`recv_ab.ps1`). Setup: L4 = Arena's own clip "Botanical Calm 4K", 30 fps,
1200 pictures per 40 s when clean; L5 receives `SP-program-MAX` with blend
Add; SongPlayer on Blank.

| Condition | `pictures` per 40 s |
|---|---|
| SongPlayer running | 538–746 |
| SongPlayer stopped | 1200–1203 |
| MAX on (FHD on or off) | 509–740 |
| `program_max_enabled` false | 1201–1203 |
| MAX on, **no Arena layer receiving it** (L5 cleared) | 1201–1202 |
| MAX on, L5 receiving it | 562–723 |

A receiving Arena is the cause. The first suspect was the hand-off: since
0.82.0 (PP, no reports) the shim's `SendSynced` holds Spout's texture-access
mutex while it spins for the GPU copy, and every receiver takes that mutex
to read. **Ruled out the same day** (`modes_ab.ps1`, `size_ab.ps1`): the
three hand-offs (`send_us_p99` 0.2–2 ms) read 624 / 631 / 663 pictures, and
a 1920×1080 MAX (`program_max_size`, below) reads 652 against the 4K's 632.
Neither the mutex hold nor the texture size moves it; what does is open
(`recv2_ab.ps1`: SongPlayer's FHD `SP-program` and an independent sender,
`spout_test_sender.py`, received instead).

**ANY receive, not SongPlayer's sender** (`recv3_ab.ps1`, `recv4b_ab.ps1`,
19.10.2026, #251 comments 6275818478 / 6276709434). An independent SpoutGL
sender (`diag\wall-bridge\spout_test_sender.py`, `black` sends an all-black
picture) costs Arena the same ~45–55 % on the wall as `SP-program-MAX` or
the FHD `SP-program`:
- the sender's rate does not matter: 1 picture/s reads like 30/s, so the
  cost is the receive being active every frame, not each copy;
- Arena's own Composition Monitor in its UI keeps ~1140–1168 of ~1200
  while the wall reads 520–720: the pictures are lost between Arena's
  composition and the Advanced Output's `Display` window on the RTX's
  Surround output, not in SongPlayer. Read it with
  `diag\wall-bridge\cadence_out.py 0 100 1400 2200 1660 40` (dxcam output 0
  = the primary, the VDD through the RTX); the wall is output 1;
- in the normal state (L4/L5 receiving MAX with a song playing) the wall
  read 1201 pictures in 40 s, 1196 runs of 2 (20:35Z): SongPlayer's moving
  picture was clean; the loss shows on Arena's own content under a
  receive;
- Arena reports `FPS 60.0` throughout ("Show FPS & Stats" in its layout).
  PresentMon 2.6.0 (`diag\tools\PresentMon-2.6.0-x64.exe --process_name
  Arena.exe --output_file x.csv --timed 20 --terminate_after_timed`, ETW,
  read only) sees three Arena swapchains at 60 presents/s, Composed: Flip,
  every present displayed; the sync-1 one carries ~14.6 ms of GPU work per
  16.7 ms frame. A receive changes none of that (`pm_ab.ps1`, #251 comment
  6279484457: 600 vs 285 wall pictures, the same presents, modes and GPU
  times): Arena hands its output window a stale composition picture, the
  loss is inside Arena, not in DWM / the driver / the flip;
- minimizing Arena's UI window does not bring the wall back (`ui_min_ab.ps1`
  v2: 254 / 205 / 292 / 375 of 600), so drawing the UI is not the cause.
- **`Get-Process Arena`'s `MainWindowHandle` is the `Display` window — the
  wall output**, not the UI. Never minimize / move / focus a window found
  that way: take the UI ("Resolume Arena - …") by its own handle
  (`EnumWindows` + `GetWindowText`) and check the title before acting
  (v1 minimized the wall twice, 19.10.2026 ~23:06Z).
- A SongPlayer-side change cannot fix this; never ship one "for #251"
  without a wall measurement that moves.

**The size switch:** `program_max_size` = `WxH` (even sides, 640..=3840 ×
360..=2160; anything else = the canvas 3840×2160), read with MAX's settings
(`program_max_phase.rs`); a change drops MAX's sender, then its target, and
the same boundary builds both at the new size (`program_max_release.rs::
resize_max`; a change inside a refused sender's backoff keeps the backoff).
`max.width/height` report it; the post-deploy MAX gate still requires
3840×2160, so a forgotten A/B setting fails CI.

**The switch:** `program_max_spout_send` = `synced` (default, unchanged) |
`plain` (0.82.0's `SendTexture`) | `release_then_wait` (the mutex let go
before the wait, a frame announced only once copied).
- It is read with MAX's settings every 5 s (`apply_phase_settings`, an INFO
  line on a change) and kept on MAX's queue (`MaxOut::spout_send`).
- The worker gives it to both senders before every send
  (`MaxSender::set_send_mode`), and the shim's mode is
  `spout::shim_send_mode` (0 / 1 / 2, `kSend*` in `spout_shim.cpp`).
- Tests: the parse (sp-core), the shim codes (sp-gpu), the apply
  (`program_max_tests_ndi.rs`), and both senders per boundary
  (`program_max_worker_tests_fhd.rs`).

**How to judge any wall change from now on:**
- count `pictures` against the content's own rate, next to the backs;
- measure on NON-SongPlayer content too: an Arena clip on one layer while
  another layer receives SongPlayer's Spout;
- a test that switches the bridge Arena puts every clip and setting back in
  a `finally` and honours the rig lease (`lease_free.ps1` exit code).

## HLSL at runtime

`compose.hlsl` (`include_str!`) is compiled by `D3DCompile`
(d3dcompiler_47.dll, which ships with Windows 10/11 and windows-latest) once
per `Compositor`, `vs_5_0` / `ps_5_0` at feature level 11.0+. #279 P1a:
`uyvy.hlsl` is compiled the same way when a UYVY pass is enabled
(`pipeline::compile` takes the source and its name). No fxc/dxc
build step, and WARP and the RTX run the same source. A compile error is
`GpuError::Shader` with the compiler's log.

## Tier-0 and the gates

- `src/win/` is `#[cfg(windows)]` and excluded from the Linux mutation gate
  (`.cargo/mutants.toml`). Every decision lives in a pure, Linux-tested module:
  `adapter`, `picture`, `composition`, `quad`, `residency`, `color`,
  `reference`, `error`, `readback` (the mapped rows packed), `spout` (the
  sender name rule, the registry parsers, the shim's codes), `spout_state`
  (the sender's registration: every refuse / confirm decision), `spout_lock`
  (#243: the lock names, the releases owed, the holder's text and program
  name, the list sentinel's steps), `vblank`
  (the output pick, the refresh count and fit, the staleness bound),
  #251 step 2's `targets` (a compositor's targets: ids, draw order, the
  primary kept), `stats::target_draws_us` (each target's part of the
  draw; #279 P1a: `split_uyvy_part`, the UYVY pass's part), #279 P0's
  `probe` (the report, the verdict, `acquire_step`,
  `feature_step`, `pixel_step`, `content_step`, `probe_picture`, `Vram`)
  and #279 P1a's `uyvy` (the conversion rows and their constant buffer,
  the pair model and the reference, the size rule, the map error, the
  staging ring), #279 P2's `ring_state` (the ring's slots, classes,
  keys, the writer's and the compositor's answers, stale textures and
  open slots), `ring` (its shared state, the frames, `FakeRing`) and
  `stats::count_slot`; `win/ring.rs` and `win/sides.rs` only call
  D3D11.
  Keep
  it that way: logic added inside `win/` is untested by the gate.
  `win/vblank.rs` only calls DXGI / Win32 (`EnumOutputs`, `GetDesc`, which
  needs the `Win32_Graphics_Gdi` feature, `WaitForVBlank`,
  `SetThreadPriority`); the stub's `VblankTracker::start` is
  `Unsupported`. The tracker starts inside the `program-max` thread
  (`spawn_max_thread`, Windows glue), never in a unit-tested fn:
  `run_max_loop` takes the source as a parameter (tests pass `None` or a
  fake), so no test on the Windows job waits on a real vblank.
- The off-Windows `Compositor` (`stub.rs`) is an uninhabited struct (it
  holds an `Infallible`) with a `PhantomData<*const ()>`, so it is neither
  `Send` nor `Sync`, like the Windows type: code moving it across threads
  fails the Linux build too. `new` / `new_warp` return `Unsupported`; its
  methods are `mutants::skip`, since no value exists to call them on. The
  off-Windows `SpoutSender` is the same (its `new` takes a `&Compositor`,
  which cannot exist); the registry readers return `Unsupported`.
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
  - Any binding that ends in `.ok()` reads EVERY non-negative HRESULT as
    `Ok`: `IDXGIKeyedMutex::AcquireSync`'s `WAIT_TIMEOUT` (0x102) and
    `WAIT_ABANDONED` (0x80) included (#279 P0). Where a success code
    means "not acquired", call the vtable entry
    (`(Interface::vtable(x).AcquireSync)(Interface::as_raw(x), ..)`) and
    judge the raw HRESULT in a pure fn (`probe::acquire_step`).
- `tests/warp.rs` is `#![cfg(windows)]`. The `Build (Windows)` job runs it
  (`cargo test --workspace`). Linux compiles it to an empty test binary.
- S2's sp-server side (`playback/program_max.rs`, `program_max_worker.rs`)
  is NOT excluded from the mutation gate: every decision runs on Linux over
  the fake GPU. Only the Windows glue is `mutants::skip`: `spawn_max_thread`
  (`#[cfg(windows)]`), `SpoutGpu::sender` and the two one-line trait impls
  on `Compositor` / `SpoutSender` (off Windows no compositor exists to call
  them on). `SpoutGpu::compositor` stays gated: off Windows it is
  `Unsupported`, which `off_windows_the_production_gpu_is_unsupported`
  pins through the worker. #239: `SpoutGpu::fhd_sender` and the
  `listed_size` impl on `SpoutSender` are glue too (`mutants::skip`).
  #251 step 2: the traits and `SpoutGpu` live in `program_max_gpu.rs`;
  `MaxCompositor for sp_gpu::Compositor`'s `add_target` /
  `remove_target` / `draw_us` are glue too (`mutants::skip`; the WARP
  real-loop test drives them on Windows); `SpoutGpu::fhd_compositor` is
  gone (no second compositor). #279 P1b: its `enable_uyvy` /
  `disable_uyvy` / `read_uyvy` are glue too (the decisions are
  `program_max_ndi.rs`'s, over the fake), and `SpoutGpu::sender` takes
  the MAX target. #243
  follow-up: `SpoutListLock`'s `holder`, `sentinel_ended` and
  `stop_sentinel` are glue too (`mutants::skip`); its `start_sentinel`
  stays gated (`off_windows_the_production_list_is_neither_read_nor_waited_on`),
  and `ListWatch` / `ListSide` carry every decision
  (`program_max_list_tests.rs`, over `FakeList`).
