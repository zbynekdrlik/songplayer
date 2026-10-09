---
paths:
  - "crates/sp-gpu/**"
  - "crates/sp-server/src/playback/program_max*.rs"
  - "crates/sp-server/src/playback/program_output_tests_max.rs"
  - "crates/sp-server/src/api/program_tests_max.rs"
  - "src-tauri/resources/THIRD-PARTY-NOTICES.txt"
  - "e2e/post-deploy-max.spec.ts"
  - "e2e/max-gate*.ts"
---

# The `SP-program-MAX` GPU compositor and Spout sender: `crates/sp-gpu` (#223 S1a, S1b) and its runtime wiring (S2)

Design: #223 revision 3, R3-2 (comment 5979609879). `SP-program-MAX` is a
FIXED 3840×2160 canvas (owner, 3.10.2026: "4k", "both outputs static"). A 4K
fade costs ~40–45 ms on the CPU, over the 33.3 ms slot, so MAX is composed on
the GPU. This crate is the compositor (device, upload, draw, readback) and,
since S1b, its Spout sender (below); S2 wires both into the program output
(sp-server `playback/program_max.rs` + `program_max_worker.rs`, "Runtime
wiring" below).

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
  `send` (`SendTexture`), `state` (`IsInitialized`, the name kept),
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
    registered (`SendTexture` failed or threw before the registration) →
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
  program's standby → `Black`. Every job carries `SP-program`'s stamp (Spout
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
  exit. The GPU is the `MaxGpu` trait (`compositor()`, `sender(&c)`, each
  object `MaxCompositor::compose` / `MaxSender::send`), so every decision is
  Linux-tested on a fake (`program_max_worker_tests.rs`). Production is
  `SpoutGpu` (`Compositor::new()` on the picked adapter, `SpoutSender::new`
  = `SP-program-MAX`), spawned by `start_max` on Windows as thread
  `program-max`.
- Both objects are built ON the thread at the first job (neither is `Send`)
  and dropped sender first (it holds references into the compositor's
  device and render target).
- **Picture ids (`PictureIds`).** S1a's residency skips the upload of an id
  the slot holds, so an id must never name other bytes: ids come from a
  counter. A picture that is the same allocation (`SharedFrame::ptr_eq`) as
  one of the last composed boundary's keeps its id; those `Arc`s are held
  by `PictureIds`, so none can be freed and its address reused while
  compared. A held / paused / repeated frame is not uploaded again into its
  slot (sp-gpu's residency is per slot: a fade's incoming picture moves to
  the outgoing slot when the fade ends and is uploaded there once). Never
  key an id on an address or on address + stamp (the latter re-uploads
  every held picture).
- **Failures** (`serve` → `recover`, never a panic):
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
  `SendClock`, then `SendTexture`; a compose that ran past it sends at once
  and counts `send_late`. Production waits on `SpinClock` (sleep to
  `SEND_SPIN_MARGIN` = 2 ms short, then spin; the decision is the pure
  `send_wait_step`); `MaxWorker::new` uses `NoWait` (tests pass made-up
  instants), `run_max_loop` gives it `SpinClock`.
- **The due instant is a slot of the WALL's refresh** (#223 follow-up,
  9.10.2026, `program_max_vblank.rs` + sp-gpu `vblank.rs` /
  `win/vblank.rs`). A constant 12 ms after the offer was not enough: the
  wall (60.000 Hz, 0 missed vblanks) still showed bursts of
  single-refresh pictures (`wall_runs.py`: 5–87 one-refresh runs per 15 s,
  minutes apart), because our 30 fps PTP grid drifts a few ppm against
  Arena's render and for minutes at a time each send lands next to the
  instant Arena reads Spout. Spout's own advice is to match the sender's
  rate to the receiver's; an ordinary sender renders in the display's
  rhythm. So: `sp_gpu::VblankTracker` (a `program-max-vblank` thread,
  time-critical) waits `IDXGIOutput::WaitForVBlank` on the output
  `pick_output` chooses on `pick_adapter`'s adapter (attached, not the
  primary desktop at (0,0), the larger area, then DXGI's order: SNV's
  7680×1080 wall beats the equal-area 3840×2160 primary), and
  `VblankFit` turns the wake-ups into a grid: the median of the first 15
  intervals boots the count, each wake-up is counted to a refresh index
  (n = round(gap / period), a gap of n counts n − 1 missed, a wake-up
  under half a period is `Early` and left out), and a least-squares fit
  over the last 240 refreshes gives the grid (reported from 60 counted,
  periods 4–50 ms only, stale after 100 ms). `VblankPacer` then sends each
  boundary in a slot `vblank + phase` (setting
  `program_max_vblank_phase_ms`, default 8 ms) and KEEPS its lead (due −
  offer) from one boundary to the next while it stays in
  [`LEAD_MIN` 12 ms, 12 ms + one period + `LEAD_HYSTERESIS` 3 ms]: the
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
  vblank_output, vblank_tracking, vblank_period_ns, vblank_phase_us,
  send_off_grid, send_phase_us_p50/p99, slot_repicks, device_resets,
  sender_backoffs, spout_name, adapter}` (`MaxStatus`;
  `send_at_us_*` = send done − offer over the window, `send_late` = composes
  that ran past their due instant; `vblank_output` = the tracker's output
  (`\\.\DISPLAY2 7680x1080`, `null` with no tracker), `vblank_tracking` =
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
  thread) wins, then `off` (the setting), then the thread: `running` (it
  takes jobs and its last boundary went out), `error: <why>` (its last
  boundary did not; `error: the program-max thread is not running` before
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
boundary on the wall's grid), the median start after the vblank
(`send_phase_us_p50`) in [`vblank_phase_us`, + `SEND_PHASE_SLACK_US`
1 500 µs], and `slot_repicks` +1 at most (`MAX_SLOT_REPICKS`). The
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
after the stop the sender is unlisted. Each picture is served TWICE and read
after the second went out: the readback takes no Spout mutex, and the second
draw's wait for the GPU proves the first send's copy done (the second copy
writes the same bytes).

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
  the installer carries `resources/THIRD-PARTY-NOTICES.txt`.

## Telemetry (`ComposeStats`, behind `max.*_p99`)

- `upload_us`: the CPU time of this call's uploads, texture creation
  included.
- `draw_us`: from the first draw command until an event query says the GPU
  has finished the frame. The wait spins with `yield_now` (SwitchToThread:
  any ready thread runs first), bounded at 10 s (`GpuError::Timeout`). A 4K
  quad on the RTX is well under a millisecond. If S2 measures the spin as a
  cost, it can read the query a boundary later instead: Spout's copy is
  ordered after the draw on the same context and needs no wait.
- `uploads`: 0, 1 or 2 pictures uploaded.
- `SpoutSendStats::send_us` (S1b, for `max.send_us_p99`): `SendTexture`
  (the sender-mutex wait, the copy, the flush; at the first send also the
  shared texture's creation and the registration) and, since the #223
  follow-up, the wait until the GPU has DONE that copy (an event query on
  the compositor's context, `pipeline::wait_until_done_on`), so
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
  (the sender's registration: every refuse / confirm decision), `vblank`
  (the output pick, the refresh count and fit, the staleness bound). Keep
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
  pins through the worker.
