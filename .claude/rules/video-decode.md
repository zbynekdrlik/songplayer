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
  `MF_MT_DEFAULT_STRIDE`. Without a device manager the decoder MFT gets no
  Direct3D device and decodes on the CPU, whatever that hardware-transforms
  flag says.
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
  picture of 0 or over 16384 on a side, a pitch shorter than a row, a
  surface with fewer rows than the picture, and a mapping (from scanline 0,
  `mapped_from_scanline0`) that does not reach the last UV row's last
  byte; an overflowing size is "short". A refusal is a decode error, so the
  file goes on in software.
- `SurfaceCopy::copy` packs the picture in the SOFTWARE path's layout:
  stride = one chroma row (`nv12_chroma_row(width)`), `height` luma rows,
  then ⌈height/2⌉ UV rows, into a `frame_pool` buffer (`try_take`; a host
  out of memory is `FrameAlloc`, as in software). Nothing downstream can
  tell the paths apart; `width` / `height` come from the current media type
  per picture, as in software.

## Which path really decoded (never assumed)

- A DXGI-surface picture came out of the GPU decoder; a system-memory one
  came out of a software decoder (`DecodePath::of_picture`). Microsoft's
  "Supporting Direct3D 11 Video Decoding in Media Foundation": a decoder
  that finds no configuration on the device "must fall back to software
  decoding" (the device manager is withdrawn, the type renegotiated). So a
  `Hardware` reader can decode in software without any error: the box's
  GPU without a decoder for a codec, or WARP (no decoder profiles).
- `decode_path()` = the last picture's path, `None` before the first;
  `hw_adapter()`, `hw_fallback()`. The first picture on the D3D path is
  counted (`hw_counters().first_picture`) and logged: INFO `mf_reader:
  hardware decode active (DXGI surfaces)`, or WARN `mf_reader: the D3D11
  path is set up, but Media Foundation decodes this file in software`.

## Never a dead song

- A `Hardware` open that fails (no hardware adapter: `GpuError::NoAdapter`,
  no device, MF refusing the manager) opens the file in software: WARN
  `mf_reader: hardware decode did not open; …`, `hw_fallback` `open: …`.
  Only a file that does not open in software either is an error.
- A decode error on the D3D path (a lost device, a surface the readback
  refuses, anything but `FrameAlloc`: `hw_decode::on_decode_error`)
  reopens the file in software ONCE: WARN `mf_reader: a decode error on the
  D3D11 path; …`, `hw_fallback` `mid-stream: …`. `hw_decode::Resume` seeks
  to the last picture handed over (or the last seek's target when none was
  since), and the pictures through it are dropped (MF lands on the keyframe
  before the position): no picture lost or handed over twice. A second
  error goes to the caller, as before S3b.
- `HwCounters` (`sp_decoder::hw_counters()`, process-wide, every
  `Hardware` reader, the bench's too): `requested`, `hardware`,
  `mf_software`, `open_fallbacks`, `mid_stream_fallbacks`,
  `last_fallback`.

## The setting and the telemetry (`sp-server/src/playback/video_decode.rs`)

- `video_hw_decode` (`sp_core::config::video_hw_decode`): ON only for an
  exact `"true"` (trimmed), OFF by default. It flips to on only after the
  main session's box gate (`diag-bench.md`: each 4K sample on the hardware
  path passes D2, 1440p not slower).
- `start` (lib.rs, before the pipelines) applies it to the process value
  (`global()`), then a task re-reads it every 5 s. The PACED producer
  (`pipeline_paced.rs::run_decode_producer`) opens each song with
  `global().mode()`: a change applies from the next song, a playing song
  keeps its reader. The SDK-clocked path (`pipeline.rs`, `genlock_pacing`
  off, unused on the box) stays software.
- Toggle: `PATCH /api/v1/settings {"video_hw_decode": "true"}`; no restart.
- `GET /api/v1/status` → `video_decode {hw_decode, hw_requested,
  hw_decoding, mf_software, open_fallbacks, mid_stream_fallbacks,
  last_fallback}` (the setting + the counters).
- `POST /api/v1/diag/decode-bench` takes `"hw": true` and reports
  `decode_path` / `adapter` / `hw_fallback` (`diag-bench.md`).

## Tests and gates

- `src/video/` is `cfg(windows)` and OUT of the mutation gate
  (`.cargo/mutants.toml`). Every decision is `src/hw_decode.rs` at the
  crate root (in the gate), Linux-tested in `hw_decode_tests.rs`. Keep it
  that way: logic added inside `video/` is untested by the gate. Never
  name a pure helper under `video/` or `audio/` (both excluded dirs).
- `tests/mf_hw_decode.rs` (Windows; `windows-latest` has no GPU): a
  `Hardware` open of the H.264 fixture on the picked adapter (an open fall
  back on CI) and on WARP (`open_hardware_on_warp`, doc-hidden: the D3D
  manager is set up, MF decodes in software) must report `hardware` or
  `software` and decode every picture byte-identical to the software
  reader over the visible 160×120 (H.264 decoding is bit-exact); a
  hardware reader seeks. The DXGI readback itself runs only on a GPU: the
  box measures it with the bench, and `SurfaceLayout` is pinned on Linux.
- The mid-stream fall back is proven on CI by
  `fail_next_read_for_test` (doc-hidden): one injected decode error, even
  on a software reader. The fixture has ONE keyframe (at 0), so the reopen
  decodes pictures 0..=9 again and must drop them; the sequence must equal
  the uninterrupted one.
- `sp-gpu/tests/video_device.rs` (Windows): the device has the video API
  (creation flags + `ID3D11VideoDevice`) and is multithread-protected, on
  WARP and on the listed Basic Render Driver; `new()` is the picked adapter
  or `NoAdapter` (never WARP on its own).
- `windows` 0.58 traps met here: `IMF2DBuffer2::Lock2DSize` takes
  `MF2DBuffer_LockFlags_Read` and five out-pointers; `IMFDXGIBuffer::
  GetResource(&ID3D11Texture2D::IID, &mut raw)` returns an AddRef'd raw
  pointer (`ID3D11Texture2D::from_raw` takes it); `ID3D11Texture2D::GetDesc`
  needs `Win32_Graphics_Dxgi_Common`; `SetMultithreadProtected` takes a
  `BOOL` (`TRUE`), not a `bool`.
