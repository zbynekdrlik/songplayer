---
paths:
  - "crates/sp-decoder/src/video/**"
  - "crates/sp-decoder/src/hw_decode*.rs"
  - "crates/sp-decoder/tests/mf_*.rs"
  - "crates/sp-gpu/src/win/video_device.rs"
  - "crates/sp-gpu/tests/video_device.rs"
  - "crates/sp-server/src/playback/video_decode*.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
---

# The Media Foundation video reader: software and opt-in hardware decode (#223 S3b)

Design: #223 revision 3, R3-4 point 2 (comment 5979609879); the S0 result
that asked for it (comment 5981771378: 4K AV1 95 %, 4K VP9 93 % of the
frame period in software); the anchors and MF facts (comment 5990523303).

## The two modes

`MediaFoundationVideoReader::open_with(path, DecodeMode)`:

- **`Software`** (the default, and `open(path)`): the reader as it always
  was. A source reader with `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` and NO
  Direct3D device, NV12 negotiated, each sample
  `ConvertToContiguousBuffer` + `Lock` into a `frame_pool` buffer, stride
  `MF_MT_DEFAULT_STRIDE`. Without a device manager Microsoft's decoder MFTs
  get no Direct3D device and decode on the CPU: S0 measured exactly that on
  the box. (A vendor's own hardware decoder MFT could decode on its GPU into
  system memory without a device; the reader would still report
  `software` for its pictures: system memory is all it can see.)
- **`Hardware`**: `sp_gpu::VideoDevice` (the compositor's adapter rule,
  `pick_adapter`: the RTX 3070 Ti; created with
  `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` and multithread-protected) behind a
  DXGI device manager (`MFCreateDXGIDeviceManager` + `ResetDevice`), set
  on the source reader as `MF_SOURCE_READER_D3D_MANAGER`. The decoder MFT
  then decodes with DXVA (NVDEC) and hands each picture over as a sample
  with ONE `MFCreateDXGISurfaceBuffer` buffer over a slice of its
  `D3D11_BIND_DECODER` texture array. One device per reader: a lost device
  never touches another song.

## Reading a GPU picture back (`video/dxgi_frame.rs`)

- The buffer is an `IMFDXGIBuffer` (`GetResource` → the texture →
  `GetDesc`: its `Height` = the surface rows, its `Format`) and an
  `IMF2DBuffer2`.
- `Lock2DSize(MF2DBuffer_LockFlags_Read, …)` copies the slice to the CPU
  and maps it: scanline 0, the pitch, and the mapping's bounds. A READ lock
  (Microsoft: a read/write lock "can cause an extra copy between CPU
  memory and GPU memory"). The guard unlocks on every path.
- The mapped surface is Direct3D's NV12 layout: `Height` luma rows `pitch`
  apart, then the UV plane at `pitch × Height`. The texture may be taller
  than the picture (decoder alignment), so the UV offset comes from the
  TEXTURE's height, never from `MF_MT_FRAME_SIZE`.
- `hw_decode::SurfaceLayout::check` refuses, before any byte is read: a
  format other than NV12 (103; a 10-bit stream decodes into P010, 104), a
  picture of 0 or over 16384 on a side, a texture narrower than the picture
  (`surface_cols` = its `Width`: past it the copy would read a row's
  padding as pixels), a pitch shorter than a row, a surface with fewer rows
  than the picture, and a mapping (from scanline 0,
  `mapped_from_scanline0`) that does not reach the last UV row's last
  byte; an overflowing size is "short". A refusal is a decode error, so the
  file goes on in software.
- `SurfaceCopy::copy` packs the picture in the SOFTWARE path's layout:
  stride = one chroma row (`nv12_chroma_row(width)`), `height` luma rows,
  then ⌈height/2⌉ UV rows, into a `frame_pool` buffer (`try_take`; a host
  out of memory is `FrameAlloc`, as in software). The rows are packed by
  `sp_gpu::unpad_rows_into`, the compositor readback's own routine. A
  slice shorter than the checked `needed` is an error, never a panic.
  Nothing downstream can tell the paths apart; `width` / `height` come from
  the current media type per picture, as in software.

## Which path really decoded (read from the pictures, not from the mode)

- The path is INFERRED from each picture (never from the mode): a picture
  in a DECODER texture is taken to have come out of the GPU decoder. CI
  never sees a real decoder texture (no GPU, and WARP refuses the video
  device, below); the box bench is the proof (`diag-bench.md`: cross-check a
  `"software"` label against
  the timings). In detail: a DXGI surface whose texture carries
  `D3D11_BIND_DECODER` (0x200; Microsoft: a DXVA decoder's output array
  "should include the D3D11_BIND_DECODER flag") counts as the GPU
  decoder's. A system-memory picture, or one a software decoder uploaded
  into a texture without that flag, does not (`DecodePath::of_picture(bind
  flags)`; the flags come from the `GetDesc` the readback reads anyway).
  Microsoft's "Supporting Direct3D 11 Video Decoding in Media Foundation":
  a decoder that finds no configuration on the device "must fall back to
  software decoding" (the device manager is withdrawn, the type
  renegotiated). So a `Hardware` reader can decode in software without any
  error: the box's GPU without a decoder for a codec. (Not `windows-latest`'s
  WARP: it refuses the video device at feature level 11.x, so a reader on it
  falls back at open.)
- `decode_path()` = the last picture's path, `None` before the first;
  `path_changes()` = how often a picture's path differed from the one
  before it, a fall back's too (`PathTracker::changes`; the bench's
  `path_changes`); `hw_adapter()`, `hw_fallback()`. `hw_decode::PathTracker::observe` notes,
  while the reader runs on the D3D path: its FIRST picture, counted
  (`hw_counters().first_picture`) and logged (INFO `mf_reader: hardware
  decode active (decoder surfaces, D3D11_BIND_DECODER)`, or WARN
  `mf_reader: the D3D11 path is set up, but Media Foundation decodes this
  file in software`), and any later CHANGE of path with no error (MF's
  decoder changing its mind mid-file), counted (`path_changed`) and
  logged (WARN `mf_reader: the decode path changed mid-file with no
  error`, `from` / `to`).

## Never a dead song

- A `Hardware` open that fails (no hardware adapter: `GpuError::NoAdapter`,
  no device, MF refusing the manager) opens the file in software: WARN
  `mf_reader: hardware decode did not open; …`, `hw_fallback` `open: …`.
  Only a file that does not open in software either is an error.
- A decode error on the D3D path (a lost device, a surface the readback
  refuses, anything but `FrameAlloc`: `hw_decode::on_decode_error`)
  reopens the file in software ONCE (`hw_decode::FallbackGate`: armed when
  the reader opens on the D3D path, used up by the reopen): WARN
  `mf_reader: a decode error on the
  D3D11 path; …`, `hw_fallback` `mid-stream: …`. `hw_decode::Resume` seeks
  to the last picture handed over (or the last seek's target when none was
  since), and the pictures through it are dropped (MF lands on the keyframe
  before the position): no picture lost or handed over twice. They are
  dropped by timestamp BEFORE any readback (`Resume::skips`), but each is
  still decoded, in software: up to one GOP, several seconds for a 4K AV1
  file, during which the paced output repeats its last picture AND plays
  silence: the producer pulls audio with the video (`next_synced`), so a
  blocked `next_frame` starves the audio too and the pacer's underrun
  path fills it. Expect a multi-second A/V gap after a lost device. A
  second
  error goes to the caller, as before S3b.
- `HwCounters` (`sp_decoder::hw_counters()`, process-wide, every
  `Hardware` reader, the bench's too): `requested`, `gpu_decodes`,
  `mf_software`, `open_fallbacks`, `mid_stream_fallbacks`,
  `path_changes`, `last_fallback`. A fall back is counted once the file
  has opened in software (a file that opens nowhere is only the error).

## The setting and the telemetry (`sp-server/src/playback/video_decode.rs`)

- `video_hw_decode` (`sp_core::config::video_hw_decode`): ON only for an
  exact `"true"` (trimmed), OFF by default. It flips to on only after the
  main session's box gate (`diag-bench.md`: each 4K sample on the hardware
  path passes D2, 1440p not slower).
- `start` (lib.rs, before the pipelines) applies it to the process value
  (`global()`), then a task re-reads it every 5 s. The PACED producer
  (`pipeline_paced.rs::run_decode_producer`) opens each song with
  `global().mode()`: a change applies from the next song, a playing song
  keeps its reader. (#221 lane 3 deleted the SDK-clocked path, which
  stayed software.)
- Toggle: `PATCH /api/v1/settings {"video_hw_decode": "true"}`; no restart.
- `GET /api/v1/status` → `video_decode {hw_decode, hw_requested,
  gpu_decodes, mf_software, open_fallbacks, mid_stream_fallbacks,
  path_changes, last_fallback}` (the setting + the counters; counts of FILES since the
  process started, not live state; `#[serde(default)]`, so a missing key
  reads as zero).
- `POST /api/v1/diag/decode-bench` takes `"hw": true` and reports
  `decode_path` / `adapter` / `hw_fallback` (`diag-bench.md`).

## Tests and gates

- `src/video/` is `cfg(windows)` and OUT of the mutation gate
  (`.cargo/mutants.toml`). Every decision is `src/hw_decode.rs` at the
  crate root (in the gate), Linux-tested in `hw_decode_tests.rs`. Keep it
  that way: logic added inside `video/` is untested by the gate. Never put
  a pure helper under `video/` (excluded), nor among
  `audio/symphonia_reader.rs`'s wrapper methods (excluded by type name).
- **WARP refuses the video device on CI, so no WARP reader runs on the D3D
  path there.** On `windows-latest`, `D3D11CreateDevice` on WARP with
  BGRA and `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` at feature level 11.1 / 11.0
  (the call `VideoDevice` makes) returns DXGI_ERROR_UNSUPPORTED (0x887A0004, run
  37293259981). Microsoft's `D3D11_CREATE_DEVICE_FLAG` page says a WARP
  device (and the Basic Render Device) with the flag succeeds; the same
  entry limits video on a pre-WDDM-1.2 driver to feature levels 9.x, which
  may be why an 11.x request is refused. Three S3b tests built on that page
  failed on correct code. A runner capability is proven in CI before a test
  rests on it.
- **What proves the hardware path.** DXVA decode and the readback of a
  decoder's texture are left to the main session's box bench
  (`decode-bench` with `"hw": true` on the box's GPU: `decode_path:
  "hardware"`, `path_changes: 0`, `diag-bench.md`; record the result on
  #223). A seek and a decode error (the mid-stream fall back, a lost
  device) ON the D3D path are proven nowhere: the bench neither seeks nor
  forces a fall back. Their code is shared with what CI runs: the seek
  parity and the injected fall back below, and `hw_decode_tests.rs`.
- `tests/mf_hw_decode.rs` (Windows): a `Hardware` open of the H.264
  fixture must decode every picture byte-identical to the software reader
  over the visible 160×120 (H.264 decoding is bit-exact). On the picked
  adapter it reports `hardware` or `software` (`assert_reported`: whatever
  `pick_adapter` finds on the runner, not pinned; with no GPU an open fall
  back; its line goes straight to stderr, past libtest's capture, so every
  CI log shows what the runner did). On WARP (`open_hardware_on_warp`,
  doc-hidden) it must fall back at OPEN with exactly `no video device:
  D3D11CreateDevice failed (HRESULT 0x887a0004)`
  (`assert_fell_back_at_open_on_warp`: mode `Hardware`, no adapter),
  `requested` and `open_fallbacks` +1 and `last_fallback` this one, every
  picture `software` with `path_changes` 0: the CI test of a refused video
  device. A WARP that one day makes the device fails it; then write the
  WARP tests for the D3D path they reach. The counters are process-wide and
  a binary's tests run in parallel, so every test that opens a `Hardware`
  reader or injects a fall back holds the file's `COUNTERS` lock and pins
  its deltas exactly (`> before` would pass on another test's count). Seek
  parity: each reader decodes 60 pictures
  (past 1 900 ms), then seeks BACK to the middle; the fixture's one
  keyframe is at 0, so a real seek hands over a picture at or before the
  target (a seek that did nothing would hand over the 61st, ~2 000 ms), and
  the `Hardware` reader's picture (one that fell back at open, asserted)
  equals the software reader's.
- The DXGI readback runs on a REAL surface, since no decoder reaches it on
  CI: `a_dxgi_surface_is_read_back_into_the_software_layout` makes an NV12
  texture of 160×128 (taller than the 160×120 picture, as decoders align
  theirs) on a PLAIN WARP device (`plain_warp_device`: BGRA, feature level
  11.1 / 11.0, the compositor's call; NO video flag: making and mapping an
  NV12 texture should not need the video device, and this test is the CI
  proof), whose bytes say where they are, wraps it as a decoder wraps its
  output (`read_texture_as_decoded_sample`, doc-hidden:
  `MFCreateDXGISurfaceBuffer` in an `MFCreateSample` sample) and checks the
  packed picture byte for byte: the UV plane really starts after ALL 128
  texture rows in `Lock2DSize`'s mapping. It asserts WARP's NV12 Texture2D
  support (a refusal fails, never skips), pins the pure crate's
  `DXGI_FORMAT_NV12` and `D3D11_BIND_DECODER` against the SDK's, and that
  a texture without `D3D11_BIND_DECODER` reads `software`.
- The mid-stream fall back from the D3D path cannot be reached on CI. Its
  code is covered by `fail_next_read_for_test` (doc-hidden: one injected
  decode error, the gate armed) on a software reader, the same on a
  `Hardware` reader that fell back at open (asserted first; the hook arms
  the gate such a reader never arms itself:
  `a_hardware_reader_that_fell_back_at_open_goes_on_after_an_injected_decode_error`),
  and `hw_decode_tests.rs` on Linux. The fixture has ONE keyframe (at 0),
  so the reopen decodes pictures 0..=9 again and must drop them; the
  sequence must equal the uninterrupted software one.
- `sp-gpu/tests/video_device.rs` (Windows): WARP makes a BGRA device but
  refuses the same call with the video flag, and `VideoDevice::new_warp`
  reports `GpuError::Api { D3D11CreateDevice, 0x887A0004 }`. Whether the
  listed Basic Render Driver makes the video device is NOT proven (the
  same Microsoft entry), so that test asks the driver with the same
  `D3D11CreateDevice` call and asserts `new_on_listed_adapter` agrees (a
  device with the video API and multithread protection on that adapter,
  or the driver's own HRESULT). A missing video flag fails either way; a
  wrong adapter fails only when the driver accepts (`create_on`'s adapter
  choice is the compositor's, proven by `tests/warp.rs`). It writes the
  driver's answer straight to stderr (past libtest's capture), so every CI
  log shows it. `new()` is the picked adapter or `NoAdapter` (never WARP
  on its own).
- Both CI test jobs run `cargo test --no-fail-fast` (#223 S3b: the failing
  `mf_hw_decode` binary had stopped the run before sp-gpu's and
  sp-server's tests ran). The release-mode sp-decoder step also runs after
  a failed debug test step, never on a cancelled run (`!cancelled()`).
- `windows` 0.58 traps met here: `IMF2DBuffer2::Lock2DSize` takes
  `MF2DBuffer_LockFlags_Read` and five out-pointers; `IMFDXGIBuffer::
  GetResource(&ID3D11Texture2D::IID, &mut raw)` returns an AddRef'd raw
  pointer (`ID3D11Texture2D::from_raw` takes it); `ID3D11Texture2D::GetDesc`
  and `CreateTexture2D` need `Win32_Graphics_Dxgi_Common`;
  `SetMultithreadProtected` and `MFCreateDXGISurfaceBuffer` take a `BOOL`
  (`TRUE` / `FALSE`), not a `bool`; `CreateTexture2D(&desc, Some(&init),
  Some(&mut tex))` works because `Some(&x)` coerces into the
  `Option<*const T>` the binding asks for; an interface goes into a
  `Param<IUnknown>` as `&iface`.
