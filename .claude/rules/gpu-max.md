---
paths:
  - "crates/sp-gpu/**"
  - "crates/sp-core/src/fit*.rs"
  - "crates/sp-server/src/playback/program_max*.rs"
  - "crates/sp-server/src/playback/program_output_tests_max.rs"
  - "crates/sp-server/src/api/program_tests_max.rs"
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
of each picture, both targets drawn at the compose phase).

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
    primary). sp-server adds `FHD_TARGET` to MAX's compositor. The ids and
    the draw order (primary first, then as added; an id never reused) are
    the pure `targets.rs` `Targets<T>`, which `win/mod.rs` holds its
    targets in.
  - `layers()` is `layers_on(MAX_TARGET)`; `layers_on(target)` =
    `layers_in(w, h, fit)`, which places into any target.
  - `pictures()` = the (slot, picture) pairs one compose UPLOADS, in draw
    order, whatever the targets; every target's layers sample exactly
    those (`composition_tests.rs::every_target_draws_the_pictures_one_compose_uploads`).
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
- The slot is per side, as dispatched. At a fade's end the incoming picture
  is drawn as a plain picture from the `Outgoing` slot, so a held incoming
  picture is uploaded once more (one upload; a live song brings a new id
  every boundary anyway).
- #251 step 2: the slots belong to the COMPOSITOR, not to a target.
  `compose` checks and uploads `pictures()` once, then draws every target
  from those textures (each through its own viewport, `QuadConstants` and
  render target), ends each target's event query after its draw, flushes
  ONCE and waits for them in order (`pipeline::wait_until_each_done`).
  `ComposeStats::draw_us` covers every target; `draw_us(id)` is a target's
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
- The consumer's `next(holding)`: `Stop` wins; while off, `Release` when it
  holds GPU objects; else the next job. `set_enabled(false)` drops the
  queued jobs and wakes it; `attach()` returns the guard whose drop stops
  the offers (a dead thread never leaves a stale "running").
- The queue and the stats are two locks: an API read takes the queue lock
  only to copy two fields (`enabled`, `coalesced`), copies the p99 windows
  under the stats lock and sorts them after it.

### The `program-max` thread (`program_max_worker.rs`)

- `run_max_loop(out, gpu)`: `attach`, then `next` → `serve` / `release` /
  exit. The GPU is the `MaxGpu` trait (`compositor()`, `sender(&c)`,
  `fhd_sender(&c, target)`; `MaxCompositor::{compose, adapter, add_target,
  remove_target, draw_us}`, `MaxSender::{send, listed_size}`; since #251
  step 2 in `program_max_gpu.rs`, re-exported by the worker), so every
  decision is Linux-tested on a fake (`program_max_worker_tests.rs`: ONE
  `FakeCompositor` minting target ids with the real `sp_gpu::Targets`).
  Production is `SpoutGpu` (`Compositor::new()` on the picked adapter,
  `SpoutSender::new` = `SP-program-MAX`), spawned by `start_max` on Windows
  as thread `program-max`.
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
  draw_us_p99, send_us_p99, send_at_us_p50/p99/max, send_late,
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
`Compositor::new_warp()` + `SpoutSender::with_name(…, "SP-program-MAX-s2-warp-test")`.
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
and 1680. #251 step 2: the FHD sender is `for_target` of the target the
worker adds to the same WARP compositor (the production `add_target` of
`sp_gpu::Compositor`), so both read-backs come from one compose.

### The FHD Spout sender `SP-program` (#239)

Design: #239 "## Design" (the last comment). The FHD program goes out over
Spout too, so a local consumer (Arena, OBS with the Spout plugin) takes the
1920×1080 program without NDI.

- sp-gpu: `SpoutSender::new_fhd(&c, target)` = `SPOUT_FHD_SENDER_NAME` =
  `SP-program` (Arena: `SPOUT_SP-program`), the sender of the compositor's
  target `target`. #251 step 2: that target is `FHD_TARGET` (1920×1080,
  `FHD_FIT`) ADDED to MAX's compositor (`Compositor::add_target`), never a
  second compositor: one device, one upload of each picture, one compose.
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
  `MaxOut::fhd_wanted` (the setting AND MAX on), the FHD OUTPUT (its
  target on MAX's compositor + its sender, `Output`, built on the thread:
  `MaxCompositor::add_target(FHD_TARGET)`, then `MaxGpu::fhd_sender(&c,
  target)`) is drawn by the same compose as MAX, from the same labelled
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
  The FHD target exists only while its sender lives: a refused FHD sender
  or a failed FHD build drops its sender and takes the target off the
  compositor (`drop_fhd`), so nothing is drawn for it during its own
  backoff; a refused or failed MAX sender never stops the FHD output, and
  the other way round. Switched off (`fhd_wanted` false), the FHD sender
  and target go at the next boundary and its backoff is forgotten
  (`release_fhd`); MAX off releases everything (MAX's sender, the FHD
  sender, then the compositor).
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
  `FakeCompositor` that draws the FHD target when one was added, `Log`
  `targets_added` / `targets_removed` / `fhd_drawn` / `fhd_*` / `order`
  ("fhd target", "fhd sender built", "max draw", "fhd draw", "max send",
  "fhd send", "fhd target removed"), `Script.add_target` / `fhd_sender` /
  `fhd_send` / `listed`: the FHD output built after MAX's send, both
  targets at `compose_at`, MAX first, FHD off draws no FHD target, an FHD
  refusal or failed build never stops MAX and takes its target off),
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
  2: every target drawn (one query per target, one flush, one wait);
  `Compositor::draw_us(id)` = a target's own part (`target_draws_us`), the
  FHD one is `max.fhd.draw_us_p99`.
- `uploads`: 0, 1 or 2 pictures uploaded, whatever the targets.
- `SpoutSendStats::send_us` (S1b, for `max.send_us_p99`): the shim's
  `SendSynced` (the sender-mutex wait, the copy, the flush, the wait until
  the GPU has DONE that copy, then the announce; at the first send also the
  shared texture's creation and the registration; #251 step 3 moved the
  #223 follow-up's wait from after `SendTexture` to before the announce), so
  `max.send_at_us_*` is when Spout's shared texture really holds the frame,
  relative to the program's offer: its spread is the GPU's delay of the
  copy (Arena reads the texture on its own 60 Hz clock).

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
  sender name rule, the registry parsers, the shim's codes), `spout_state`
  (the sender's registration: every refuse / confirm decision), `spout_lock`
  (#243: the lock names, the releases owed, the holder's text and program
  name, the list sentinel's steps), `vblank`
  (the output pick, the refresh count and fit, the staleness bound),
  #251 step 2's `targets` (a compositor's targets: ids, draw order, the
  primary kept) and `stats::target_draws_us` (each target's part of the
  draw). Keep
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
  gone (no second compositor). #243
  follow-up: `SpoutListLock`'s `holder`, `sentinel_ended` and
  `stop_sentinel` are glue too (`mutants::skip`); its `start_sentinel`
  stays gated (`off_windows_the_production_list_is_neither_read_nor_waited_on`),
  and `ListWatch` / `ListSide` carry every decision
  (`program_max_list_tests.rs`, over `FakeList`).
