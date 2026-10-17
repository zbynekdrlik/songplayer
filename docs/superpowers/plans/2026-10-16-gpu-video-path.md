# GPU-resident program video path: design

- **Issue:** #251 (PP: 25 fps songs judder on the 30 fps program). Owner ruling 16.10.2026 (comment 6214891622), verbatim: "Riesenie ma byt sota … Ma to byt robene od zaciatku poriadne".
- **Tree read:** `dev` at `07a5f674` (`0.84.0-dev.2`, the slot trade is back in: each picture is uploaded once).
- **Status:** a design only. No code was changed, and no box was touched while writing it.
- **Method:** the repo's framework-first / investigate-existing-first rule. Every candidate below was read at its source: the repo, the `windows` 0.58 crate, Microsoft Learn, NVIDIA, NDI, and the source of FFmpeg, mpv, Chromium, OBS, CasparCG, VLC, GStreamer, Wine and makepad.

---

## 0. Summary

- **Decode.** Each playlist pipeline decodes on the GPU through the Media Foundation Source Reader (NVDEC via DXVA). The reader runs on that pipeline's own long-lived D3D11 "producer" device. Each decoded slice is copied in VRAM into a slot of a per-pipeline **picture ring**: shared NV12 textures with an NT handle and a keyed mutex. From then on the picture travels as a `GpuFrame` handle (`Arc`) with its pts, through the pacer and the program bus, exactly like today's `SharedFrame`.
- **Compose.** ONE compositor device on the `program-gpu` thread (today's `program-max`) samples ring slots directly. It draws MAX (3840×2160), the FHD target (1920×1080), fades, transitions and the #251 24/25→30 fps blend. In the steady state there is no CPU decode and no CPU→GPU upload.
- **NDI and other CPU consumers.** The FHD target is converted on the GPU to UYVY (NDI's native format) and read back once per boundary through a staging ring. `SP-program` NDI sends that buffer one grid slot later, a constant delay. The burn is painted into it. The per-playlist preview gets its own small (640×360) on-demand readback on the producer device.
- **Unchanged.** Audio, VBAN, genlock stamps, A/V alignment and the MAX vblank pacing.
- **Fallback.** A file the GPU cannot decode stays a CPU picture (`Picture::Cpu`). The compositor uploads it once, as today, so it costs exactly what every file costs today.

---

## 1. Problem and evidence

All counts are exact jumps back per 40 s on SNV's wall (#251 comments 6212436810, 6213507561, 6214465904).

| What SongPlayer does | Jumps back |
|---|---|
| Blank: nothing decoded or uploaded, MAX sends black | 0–3 |
| MAX off: CPU decode, no upload, no draw | 5–14 |
| CPU decode + upload, `wall-25` (blend) | 53–115 |
| CPU decode + upload at bad offer phases (8–11 ms after the vblank) | up to 655 |
| NDI `SP-program` on the clip instead of Spout MAX (11:15Z) | 0 per 10 s (MAX: 38–65) |

- The disturbance follows SongPlayer's memory/PCIe traffic, not its GPU time:
  - GPU priority class, HAGS, CPU priority and cores, and NVIDIA threaded optimization changed nothing (comment 6212436810).
  - A synthetic memory-bandwidth load (`paced_burner.py`) reproduces the jumps.
  - Arena shows its older buffer for any Spout source, an independent sender included.
- The two traffic sources are CPU software decode and the upload:
  - A 4K NV12 picture is 3840·2160·1.5 = 12.4 MB.
  - Media Foundation writes it, `lock_video_buffer` copies it into a `frame_pool` buffer (`crates/sp-decoder/src/video/mf_reader.rs:390-396`), and `UpdateSubresource` copies it again into driver memory and DMAs it (`crates/sp-gpu/src/win/textures.rs:174-182`).
  - The opt-in `video_hw_decode` path reads every NVDEC picture back to RAM (`crates/sp-decoder/src/video/dxgi_frame.rs:124-157`, `Lock2DSize`) and then uploads it again.
- The `SP-program` NDI canvas adds a CPU fit or fade per boundary on the band pool (`crates/sp-server/src/playback/program_output.rs:663-675`, `program_canvas.rs:143,171`). That reads one or two native pictures (up to 2 × 12.4 MB) and writes 3.1 MB, at the drifting genlock phase.

**Estimated CPU-side traffic today** (computed from frame sizes, not measured), for a 25 fps 4K song blended on the 30 fps grid:

| Stage | MB/s |
|---|---|
| MF software decode output (plus the decoder's own reference reads, not counted) | ~310 |
| Copy into `frame_pool` (read + write) | ~620 |
| MAX upload: CPU copy into driver memory + PCIe h2d (25 pictures/s since `07a5f674`) | ~620 + 310 h2d |
| `SP-program` canvas fade, 4K→FHD, 30/s (read 2 × 12.4 MB, write 3.1 MB) | ~840 |
| NDI SpeedHQ encode input (NV12 FHD) | ~93 |
| **Total** | **~2.5 GB/s** of RAM traffic + ~0.3 GB/s PCIe upload |

The target design is in §5.12: about 0.4 GB/s of RAM traffic, no upload, and ~0.12 GB/s of readback.

---

## 2. Requirements

**Functional**

1. **R1:** hardware decode straight into GPU textures. In the steady state, no CPU decode and no CPU→GPU upload of program pictures.
2. **R2:** MAX (3840×2160, `MAX_FIT` fill-width) and the FHD program (1920×1080, `FHD_FIT` aspect) composed on the GPU from those textures, including:
   - fades;
   - #215 transitions;
   - the #251 blend (`SubmitJob::blend`).
3. **R3:** every CPU consumer of program pictures fed from ONE asynchronous readback of the composed FHD program:
   - NDI `SP-program`;
   - the 911014 burn (#228);
   - anything else that needs CPU program pixels.
4. **R4:** genlock pacing, A/V sync and every audio path unchanged in behaviour.
5. **R5:** runs on SNV (RTX 3070 Ti) and PP (RTX 3070 Ti Laptop GPU, Win 11 IoT LTSC 24H2, per `.claude/skills/win-resolume-ops/SKILL.md:462`).
6. **R6:** a defined path for any codec or file the hardware cannot decode. Never a dead song (`video-decode.md`, "Never a dead song").

**Non-functional**

- Deterministic GPU work at the vblank-locked compose phase (#251 compose phase). No new GPU work at a drifting phase.
- Every decision lives in a pure, Linux-tested, mutation-gated module. The Windows glue is thin (`gpu-max.md`, "Tier-0 and the gates").
- Each step is measured on the real wall before and after (owner ruling).

**Non-goals**

- HDR/PQ tone mapping. HDR content stays shown as BT.709, as today; it is flagged in telemetry.
- NDI MAX (#223 R3-3) and 4K downloads policy.

---

## 3. The current path, mapped (file:line)

### 3.1 Decode

- **`MediaFoundationVideoReader`** (`crates/sp-decoder/src/video/mf_reader.rs`):
  - `create` 203–286: `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` (219), optional `MF_SOURCE_READER_D3D_MANAGER` (224–228), NV12 negotiated (236–243), the visible size from the native type (250–254).
  - `read_sample` 434–477.
  - `read_picture` 483–533: DXGI surface → `DxgiSurface::read` readback (493–507), else `lock_video_buffer` (363–431).
  - `reopen_in_software` 575–597, `set_position` 600–647, `next_frame` 690–711 (the fallback gate).
- **`hw_session.rs:34-46`:** one `VideoDevice` + `IMFDXGIDeviceManager` per reader. That means a new D3D11 device per opened file.
- **`sp-gpu` `win/video_device.rs:43,74-78`:** the picked adapter, `VIDEO_SUPPORT`, `SetMultithreadProtected(TRUE)`.
- **`dxgi_frame.rs:110-163`:** `Lock2DSize` readback into `frame_pool`. This is the GPU→RAM→GPU round trip.
- **`hw_decode.rs`** (pure): `DecodePath::of_picture` (`D3D11_BIND_DECODER` = GPU), `FallbackGate`, `Resume`, `visible_size`.
- **`split_sync.rs`:**
  - `next_synced` 330: audio-as-master pairing, the 250 ms lead (`pacer_av_align.rs:79`).
  - `seek` 302, `next_target_video_frame` 426: discards pre-target frames, bounded by `MAX_SEEK_DISCARD_FRAMES` (21).

### 3.2 Pipeline (per playlist)

- **`pipeline_paced.rs`:**
  - `DECODE_QUEUE_BOUND = 12` (40).
  - `run_decode_producer` 183, which opens with `video_decode::global().mode()` (197–198).
  - `decode_and_send_paced` 375 spawns `paced-decode-<pid>` per play (401, via `decode_thread.rs:27`).
  - The emit thread pops at the boundary (634).
- **`paced_producer.rs`:**
  - `next_item` 113–156 offers the preview taps (119, 145).
  - `to_paced_frame` 221–238 wraps the NV12 buffer ONCE as a `SharedFrame`.
- **`frame_buf.rs:34`:** `SharedFrame = Arc<PooledBuf>`, recycled to `sp_decoder::frame_pool` on the last drop (`frame_pool.rs:43,143,209`).
- **`pacer.rs`:** `PacedFrame` 59–72; `pending` 252 (the blend partner) and `last_frame` 261 (the starvation repeat).
- **`pacer_blend.rs`:** `FrameBlend` 38, `blend_weight_q8` 48.
- **`submit_handoff.rs`:** `SubmitJob` 59–90 (`video`, `blend`, `live`, `media_pts_100ns`), `SUBMIT_HANDOFF_BOUND = 2` (39).
- **`paced_output.rs`:** `emit_blended` 342 and `offer_to_bus` 390. The bus is reached by an `Arc` move, with no pixel copy.

### 3.3 Program bus and output

- **`program_bus.rs`:** `PROGRAM_QUEUE_BOUND` (117), the cut segments and the reorder buffer (`program-bus.md`).
- **`program_output.rs` `serve`** (485–520) runs `split` → `limit` → `feed_outputs` (VBAN first) → `offer_max` (452) → `wait_video_quiet` (522, #251 step 4) → `submit_video` (606–660).
  - `Pair::max_job` 273–300 maps a source pair to `MaxJob::Picture`, a blended pair to `MaxJob::Fade{shown, next}`, and a `Mix` to `MaxJob::Fade{from, to}`.
  - `source_picture` 663–675 calls `Canvas::fit` / `Canvas::fade` on the CPU.
  - `paint_mix` 709–719 is the transition picture on the CPU.
  - `burn` 428–450 calls `program_burn::burned` (`program_burn.rs:204`), which paints a COPY.
  - `send` 240–258 calls `FrameSubmitter::submit_frame_at_boundary_owned` (`submitter.rs:137`): NV12, async, with the holdover.
- **`program_canvas.rs`, `nv12_mix.rs`, `band_pool.rs`:** the CPU FHD canvas (1920×1080 NV12), its one-pass fade and its persistent band workers.

### 3.4 MAX and the FHD Spout target (`program-max` thread)

- **`program_max.rs`:** `MaxPicture` 110 (a `SharedFrame` + layout), `MaxJob` 143, `MAX_HANDOFF_BOUND = 2` (93), `offer_with` 581, `start_max` 888, `spawn_max_thread` 909.
- **`program_max_worker.rs`:** `attempt` 491, `pace` 742, the compose at `compose_at` 716–720 and `compose_wait` 761, `send_max` 771, `send_fhd` 798, `run_max_loop` 890.
- **`program_max_ids.rs:18-76`:** `PictureIds` labels pictures by allocation identity (`SharedFrame::ptr_eq`).
- **`program_max_gpu.rs:17-56`:** the `MaxCompositor` / `MaxSender` / `MaxGpu` traits, with a fake GPU on Linux.
- **`program_max_vblank.rs`:** `COMPOSE_BUDGET` 14 ms (52), `compose_offset` (62), `VblankPacer::due` (224).

### 3.5 sp-gpu

- **`win/mod.rs` `Compositor::compose`** (298–375):
  - checks the pictures;
  - `swap_slots` (314);
  - `upload_for` + `PlaneTextures::write` = `UpdateSubresource` (320–335);
  - draws every target (358);
  - waits once (`wait_until_each_done`, 362, `win/pipeline.rs:191`);
  - checks the device-removed reason (368).
- **`read_back_target`** (386): a blocking staging map, today used only by tests ("later the NDI readback, R3-3").
- **`compose.hlsl`:** samples R8 luma + R8G8 chroma, BT.709 limited→full, additive per weight.
- **`win/textures.rs`:** `RenderTarget` 81–116 (BGRA, `MISC_SHARED`), `staging` 190, `read_back` 213.
- **`residency.rs`:** `upload_for` (43), `swap_slots` (63).

### 3.6 Every consumer of decoded CPU pictures today

| # | Consumer | Where | What it reads |
|---|---|---|---|
| 1 | JPEG thumbnail tap (≤5 fps, on demand) | `preview.rs:295` from `paced_producer.rs:119` | full NV12 frame |
| 2 | Live preview stream video tap (640×360 letterbox, on demand) | `preview_stream.rs:34,710,723` | full NV12 frame |
| 3 | Pacer repeat / blend partner / held tail / pre-roll | `pacer.rs:252,261`, `paced_producer.rs:44-61`, `pacer_preroll.rs:40-50` | handle only (`Arc`) |
| 4 | Handoff → bus → `ProgramOutput` | `submit_handoff.rs:66`, `paced_output.rs:390` | handle only |
| 5 | MAX + FHD Spout compositor | `program_max_ids.rs:56-76` → `textures.rs:174` | **full frame, uploaded** |
| 6 | `SP-program` canvas fit/fade (NDI) | `program_output.rs:663-675,709-719` | **full frame(s), CPU fit** |
| 7 | 911014 burn | `program_output.rs:428`, `program_burn.rs:204` | the FHD canvas (copy) |
| 8 | NDI submit holdover | `submitter.rs:137` | the FHD canvas |
| 9 | NDI input "OBS manuál" (a producer, CPU) | `ndi_input.rs:353,680` | UYVY → NV12 on the CPU |
| 10 | Standby / idle black | `frame_buf.rs:101`, `paced_output.rs:663` | constant |
| 11 | `diag` fit bench | `diag/fit_bench.rs:113` | synthetic |
| 12 | Decode bench (`"hw": true`) | `diag::decode_bench` | timings and the readback |

Offline readers are NOT on the program path: the video-upgrade verify and the repair check (`video-upgrade.md`), and `early_end` (timestamps only).

---

## 4. Candidates (read at source)

### 4.1 Decode API

**(a) Media Foundation Source Reader + D3D11 device manager. CHOSEN.**

- It is already integrated: `MF_SOURCE_READER_D3D_MANAGER` (mf_reader.rs:224); seek with discard; `visible_size`; the fallback gate; the box decode bench.
- Microsoft: "Setting this attribute enables the decoder to use DXVA" (learn.microsoft.com/…/mf-source-reader-d3d-manager).
- The output sample is `IMFDXGIBuffer` → `GetResource(ID3D11Texture2D)` + `GetSubresourceIndex()` (the `windows` 0.58 `IMFDXGIBuffer`, mf.rs bindings 6972/6975). The decoder's texture is an ARRAY with `D3D11_BIND_DECODER`.
- **Microsoft forbids an SRV on such an array:** "you cannot use texture arrays that are created with this flag in calls to ID3D11Device::CreateShaderResourceView" (learn.microsoft.com/…/ne-d3d11-d3d11_bind_flag).
  - mpv's default is therefore a GPU→GPU `CopySubresourceRegion1` (`video/out/d3d11/hwdec_d3d11va.c`). Its `--d3d11va-zero-copy` warns that D3D11 "technically does not allow sampling from a decoder surface (though most drivers support it)" and that padding can corrupt the edges (mpv `DOCS/man/options.rst`).
  - **We copy too.** A 12.4 MB VRAM-local copy is ~20–40 µs of GPU time (an estimate from VRAM bandwidth). It needs no PCIe or RAM traffic, and it gives an exact visible-size texture, so no padding is ever sampled (the #223 green-strip class).
- **Holding the `IMFSample` holds its slice:** Microsoft, "Supporting Direct3D 11 Video Decoding in Media Foundation", `IMFTrackedSample`. We release it as soon as the copy is submitted, so MF's pool never starves. Our queue depth is decoupled from `MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT`; Chromium had to size that pool, `dxva_video_decode_accelerator_win.cc:1656-1658` (Chromium 100).
- **Codecs on Ampere NVDEC:** H.264, HEVC 8/10/12, VP9 8/10/12, AV1 8/10 (NVIDIA support matrix; NVDEC application note 13.0). VP9 and AV1 go through the Store VP9 / AV1 Video Extensions, which SNV and PP have (`video-decode.md` §#229, ops skill PP section). Pascal and Turing have no AV1.
- **Pros:** frame-accurate seek (existing), audio stays FLAC/Symphonia, no new dependency, MF falls back to software per codec silently and detectably (`DecodePath::of_picture`).
- **Cons:** codec coverage depends on the Store extensions; we see MF's software fallback only per picture.

**(c) NVIDIA Video Codec SDK (NVDEC) + CUDA–D3D11 interop. Rejected.**

- It needs our own MP4/WebM demuxer.
- `cuvidMapVideoFrame` "is a blocking call", and `cuGraphicsD3D11RegisterResource` is "potentially high-overhead" (docs.nvidia.com Video Codec SDK 13.0; CUDA driver API D3D11).
- It locks us to NVIDIA. The Rust crate `nvidia-video-codec-sdk` 0.4.0 has a safe wrapper for the ENCODER only.
- The license asks for "material additional functionality".
- No gain over (a) on NVIDIA, where MF already drives NVDEC.

**(d) FFmpeg d3d11va hwaccel. Rejected for now; kept as the named second backend.**

- What it offers:
  - `AVD3D11VADeviceContext.device` accepts our device;
  - `lock`/`unlock` callbacks;
  - frames are `AV_PIX_FMT_D3D11` with (`ID3D11Texture2D*`, index) in `data[0]`/`data[1]`;
  - pools of 1+16 (H.264/HEVC) or 1+8 (VP9/AV1) + `extra_hw_frames` (`libavutil/hwcontext_d3d11va.h`, `libavcodec/dxva2.c`).
- Pros: no Store-extension dependency; dav1d for software AV1.
- Cons: a new LGPL C dependency, which needs dynamic linking with matching sources (ffmpeg.org/legal.html), a CI build of shared libs, and a re-implementation of the hardened reader (seek, held tail, early end, visible size).
- It becomes worth it only if a codec MF cannot hardware-decode on the boxes matters. The design keeps the seam: a reader produces `GpuFrame`s into the ring, whichever API decodes.

**(e) D3D12 Video Decode. Rejected.**

- "The application manages the reference frames" and the fences (learn.microsoft.com, Direct3D 12 video overview). That means our own bitstream parsing, or FFmpeg's d3d12va.
- D3D11On12 "has not been optimized for performance".

**(f) `IMFMediaEngine` + `TransferVideoFrame`. Rejected.**

- It is a player object that owns the clock, the audio and the play/pause/rate.
- `TransferVideoFrame` "scales and letterboxes the video" into our surface: a blit, not a decoder handle.
- That is incompatible with our pacer (we pace by pts on the genlock grid) and with our separate FLAC audio.

**Established apps (public sources)**

| App | What it does |
|---|---|
| OBS media source | `av_hwframe_transfer_data` downloads every hardware frame to RAM (`shared/media-playback/media-playback/decode.c`). Not zero-copy. |
| CasparCG | No hwaccel; host buffers uploaded to OpenGL (`av_producer.cpp`, `ogl/util/device.cpp`). |
| mpv | Decodes on the renderer's own device, then a GPU copy (zero-copy optional). |
| Chromium DXVA decoder | Its OWN D3D11 device, `SetMultithreadProtected(TRUE)` (`:1093`). Checks `ExtendedResourceSharing` before sharing NV12 (`:1073`). Copies each decoded slice into a shared NV12 texture under a keyed mutex (`CopyTextureOnDecoderThread`, `:2815-2888`), which ANGLE's device opens (`dxva_picture_buffer_win.cc:737`). Acquires with a 0 ms timeout: "AcquireSync should always return immediately". **This is our model.** |
| vMix | No official documentation found. |

### 4.2 Device model: (b)

**Rejected: one shared, multithread-protected device for decode and compose** (mpv's, FFmpeg's and VLC's choice).

- Every MF decoder thread, every producer copy and the compositor would share ONE immediate context and ONE device lock.
- The compositor's `wait_until_each_done` and Spout's `SendSynced` wait for the context's queue (`gpu-max.md`: "#251 step 3"). With a shared device, that queue holds other pipelines' copies and preview work. Compose and send timing would then depend on decode activity: exactly the phase coupling #251 measured.
- A decoder's bad call removes the device for MAX too.

**Chosen: a producer device per pipeline, plus one compositor device. They share textures cross-device.**

- The compositor's queue holds only its own work, and a decoder fault stays in its pipeline. Today's rule "a lost device never touches another song" (`video-decode.md`) still holds.
- Producer devices are long-lived (one per pipeline, not one per file as today's `hw_session.rs:34`).

### 4.3 Cross-device sync

- **Microsoft's recommendation.** "Starting with Windows 8, we recommend … SHARED_NTHANDLE and SHARED_KEYEDMUTEX". Without NTHANDLE "the runtime does not strictly validate shared resource parameters … might vary from driver to driver" (learn.microsoft.com/…/ne-d3d11-d3d11_resource_misc_flag).
- **Visibility needs a Flush.** "If a shared texture is updated on one device ID3D11DeviceContext::Flush must be called on that device" (`OpenSharedResource` docs).
- **Keyed mutex: CHOSEN.**
  - A key protocol (`IDXGIKeyedMutex::AcquireSync`/`ReleaseSync`; dxgi bindings 1028/1031).
  - Initial key 0. "AcquireSync will succeed if you specify the same key that was specified when ReleaseSync was last called", and `WAIT_ABANDONED` means the surface must be recreated (learn.microsoft.com/…/idxgikeyedmutex-acquiresync).
  - The documented pairing, the one Chromium ships. It works on Win 8+ drivers; the WARP proof comes in P0.
- **Shared `ID3D11Fence` (D3D11.4, `ID3D11Device5::CreateFence` / `OpenSharedFence`, `ID3D11DeviceContext4::Wait`; d3d11 bindings 1025/1032/1936/1942): rejected as the primary.**
  - A GPU-side `Wait` on a foreign fence that is never signalled (its producer device is lost) stalls the compositor's queue: the one GPU queue the wall depends on.
  - The keyed mutex with a 0 ms acquire never blocks the compositor.
- **NV12 sharing** needs `D3D11_FEATURE_DATA_D3D11_OPTIONS::ExtendedResourceSharing` ("extended support for shared Texture2D resource types and formats"). Chromium checks the same flag. P0 probes it on WARP, the RTX and the PP laptop.

### 4.4 The NDI side

- **NDI takes CPU buffers only.** `p_data` + `line_stride_in_bytes`; no GPU send API was found in the Standard or Advanced SDK (docs.ndi.video).
- **Format.** NDI's performance guide: "Use UYVY or UYVA color, if possible, as this avoids internal color conversions"; "If the data … is on the GPU … have the GPU perform the color conversion before downloading" (docs.ndi.video/…/performance-and-implementation). **UYVY is chosen**, converted on the GPU.

| Format | Bytes per FHD picture | Bytes per second at 30 fps |
|---|---|---|
| BGRA | 8.3 MB | 249 MB/s |
| **UYVY (chosen)** | **4.15 MB** | **124 MB/s** |
| NV12 (today) | 3.1 MB | 93 MB/s, but NDI converts it internally on the CPU |

- **Async lifetime.** The buffer is in use "until a synchronizing API call" (`send_video_v2_async`, docs.ndi.video/…/ndi-send). Today's holdover (`frame_buf.rs`, `submitter.rs`) already honours it.
- **Readback without a stall.**
  - Map with `D3D11_MAP_FLAG_DO_NOT_WAIT` returns `DXGI_ERROR_WAS_STILL_DRAWING` rather than blocking. Read maps only on the immediate context.
  - The compositor ALREADY waits for each compose's completion (`win/mod.rs:362`). A staging copy issued inside the compose is therefore complete when `compose` returns, so the map never stalls. A 3-deep staging ring keeps a slot free if a map is ever deferred.

---

## 5. The design

### 5.1 Topology, threads, devices

```
playlist pipeline p (one per playlist)                     program (one per process)
┌──────────────────────────────────────────┐   GpuFrame   ┌───────────────────────────────────────────┐
│ ProducerGpu(p): D3D11 device (VIDEO,      │   (Arc,      │ program-output thread (unchanged role)    │
│   multithread-protected), PictureRing(p), │   pts)       │   VBAN first, offer MaxJob, NDI submit of │
│   PreviewStage(p)                         │──pacer──bus──│   boundary b−1 (one-slot delay line)      │
│ paced-decode-p thread (STA):              │              │         ▲ NdiFeed (UYVY, stamp)           │
│   MF Source Reader (DXVA on ProducerGpu)  │              │ program-gpu thread (was program-max):     │
│   → CopySubresourceRegion slice→ring slot │              │   compositor device: opens ring slots     │
│   → keyed-mutex release → GpuFrame        │              │   (NT handle, keyed mutex), draws MAX +   │
│   preview taps: 640×360 on demand         │              │   FHD BGRA + UYVY pass + staging copy at  │
└──────────────────────────────────────────┘              │   compose_at; readback; Spout sends at due │
                                                           └───────────────────────────────────────────┘
```

- **Devices.**
  - One compositor device: today's `Compositor` on `pick_adapter`.
  - One producer device per pipeline that decodes on the GPU: `VideoDevice`, created lazily at its first GPU open and kept across songs, released after 10 min idle or on pipeline end.
  - Both use the same `pick_adapter` rule, so they are on the same adapter.
- **Threads.**
  - `paced-decode-<pid>` (STA) owns the reader. It does the slice copy and the preview stage on its pipeline's device.
  - `program-gpu` owns the compositor and the Spout senders. They are not `Send`, as today.
  - `program-output` does VBAN and the NDI submit.
  - MF's own threads use the producer device under multithread protection. Chromium says the same: "multiple threads accessing the context, the media foundation decoder threads…" (`:1088-1093`).
- **The compositor always runs** while the program runs, because NDI needs it. `program_max_enabled` now switches only the MAX target and its sender. `program_spout_fhd_enabled` switches only the FHD Spout sender; the FHD target itself always exists.

### 5.2 Decode into the ring (sp-decoder)

- **`DecodeMode::GpuResident(Arc<ProducerGpu>)`** replaces `Hardware`. `Software` stays.
- **The reader's `HwSession`** is a per-reader `IMFDXGIDeviceManager` reset to the pipeline's producer device (`MFCreateDXGIDeviceManager` + `ResetDevice`, mf bindings 444/7029), instead of a new `VideoDevice` per file.
- **Per sample**, in `read_picture`:
  1. If the sample is a DXGI surface (`DxgiSurface::of`, unchanged): `GetResource` + `GetSubresourceIndex`.
  2. `ring.copy_from(texture, index, visible)`:
     - take a free slot of the visible size, rounded up to even (planar copies need even boxes);
     - `AcquireSync(slot.key, ACQUIRE_TIMEOUT)`;
     - `CopySubresourceRegion(slot, 0, 0,0,0, decoder_array, index, &box(visible))` (d3d11 bindings 1270);
     - an explicit `Flush` (the `OpenSharedResource` docs require it on the writing device), then `ReleaseSync(1)`;
     - drop the `IMFSample`.
  3. The frame becomes `DecodedVideoFrame { gpu: Some(GpuFrame), data: empty, … }`. The new field is mechanical churn per `rust-workspace.md`, "A field type change on the TIER-0 box".
  4. A system-memory sample, i.e. MF's own software decode, stays `lock_video_buffer` → CPU, as today.
- **Pre-target frames after a seek** are copied and dropped at once; the slot returns to the free list. `split_sync` is unchanged.
- **Attributes we do NOT set:**
  - `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`: it inserts a video processor.
  - `MF_SOURCE_READER_D3D11_BIND_FLAGS` (undocumented; mingw `mfreadwrite.h`) and `MF_SA_D3D11_SHARED*`: we copy, so the decoder's own textures need neither.
- **Formats.** `DXGI_FORMAT_NV12`, or `P010` for 10-bit (P4). The ring slot takes the decoder's format. The shader reads P010 through R16/R16G16 views with the same normalized maths (DXGI_FORMAT docs: "Supported view types are SRV, RTV, and UAV").

### 5.3 The picture ring and `GpuFrame` (sp-gpu)

- **`PictureRing`** (`win/ring.rs` glue + pure `ring_state.rs`):
  - NV12 `Texture2D` slots: `BIND_SHADER_RESOURCE`, `MISC_SHARED_NTHANDLE | MISC_SHARED_KEYEDMUTEX`, created on the producer device.
  - The NT handle comes from `IDXGIResource1::CreateSharedHandle` (dxgi 1472), once per texture, closed when the texture goes.
  - Slots are pooled by (format, width, height), like `frame_pool` keyed by capacity. At most 2 size classes are kept. Capacity = `DECODE_QUEUE_BOUND` + 8 (= 20).
- **`GpuFrame(Arc<FrameInner>)`** carries:
  - `{ring_id, slot, alloc_gen, id, format, width, height, handle}`;
  - `id` from a process-wide counter: it is also the compositor's residency id, so no address labels are needed;
  - its `Drop` returns the slot to the free list (an atomic flag, `ring_state` decides).
  - Clone = an `Arc` bump, exactly the `SharedFrame` contract (`frame_buf.rs:11-25`).
- **Back-pressure.** A copy with no free slot waits, bounded (`RING_WAIT` 100 ms, counted). Past the bound it is a decode error, which takes the existing fallback gate. Slots are freed by the pacer and bus dropping frames, so the wait is a full queue in another form.
- **The key protocol** (pure `ring_state::key`):
  - A slot starts at key 0.
  - The producer acquires the slot's last key, writes, and releases 1.
  - The compositor acquires 1 with a **0 ms** timeout, draws, waits for completion, and releases 1.
  - A slot never read by the compositor (an off-program pipeline) is re-acquired by the producer with key 1.
  - A slot is reused only once no `GpuFrame` refers to it (the CPU refcount). The keyed mutex adds cross-device visibility and ordering.
- **Lost producer device.**
  - `WAIT_ABANDONED`, or a failed acquire on the compositor, makes that picture "missing": the compositor draws the side as black, like a missing fade side today. It is counted (`abandoned`).
  - The ring is marked dead and dropped.
  - The reader takes its mid-stream fallback (software reopen, `hw_decode::Resume`).

### 5.4 Frame ownership through the pacer and the bus (sp-server)

- **`playback/picture.rs`:** `pub enum Picture { Cpu(SharedFrame), Gpu(GpuFrame) }` with `ptr_eq` / `len` / `is_gpu`.
- **It replaces the `video: SharedFrame` fields** of `PacedFrame` (`pacer.rs:65`), `SubmitJob` (`submit_handoff.rs:66`), `FrameBlend` (`pacer_blend.rs:39`), `MaxPicture` (`program_max.rs:114`), `Held` (`paced_producer.rs:48`) and the pre-roll.
  - The pacer, the handoff, the bus, the cut / reorder / fill logic and the held tails are agnostic: they only clone and drop.
  - The pts stays where it is (`PacedFrame::pts_ns`, `SubmitJob::media_pts_100ns`, stamps).
- **Standby and fills keep the cached CPU black** (`BlackNv12`). The compositor treats a canvas-black CPU picture as `Composition::Black` (no upload).
- **`PictureIds`:** a `Gpu` picture is labelled by `GpuFrame::id`; a `Cpu` picture keeps today's allocation-identity labels (`program_max_ids.rs:56-76`).
- **VRAM in flight per decoding pipeline:**

| Holder | Frames |
|---|---|
| Decode queue (`DECODE_QUEUE_BOUND`) | 12 |
| Pacer (`pending` + `last_frame`) | 2 |
| Handoff | ≤ 3 |
| MAX queue | 2 |
| Compositor | 2 |
| **Total** | **≈ 20** |

That is ≈ 240 MB of ring at 4K, plus MF's decoder pool (~9–17 surfaces). §8 covers the budget.

### 5.5 The compositor (sp-gpu, `program-gpu` thread)

- **`Composition` sides** become `Side::Cpu(Nv12Picture)`, uploaded through today's path, or `Side::Gpu(&GpuFrame)`, sampled in place.
- **For a `Gpu` side:**
  - open the slot once with `ID3D11Device1::OpenSharedResource1(handle)` (d3d11 binding 816);
  - cache the texture and its two SRVs (R8_UNORM luma, R8G8_UNORM chroma; Microsoft's NV12 view rule) under `(ring_id, slot, alloc_gen)`, evicting an entry when its slot's `alloc_gen` changes or after 64 entries (LRU);
  - `AcquireSync(1, 0)` per distinct slot before drawing, `ReleaseSync(1)` after the completion wait.
  - `compose.hlsl` is unchanged: it already samples a luma and a chroma plane.
- **What one compose does** at `compose_at` (#251 compose phase; at once when no Spout target is live):
  1. uploads only `Cpu` sides (today's residency and `swap_slots`);
  2. draws MAX (if on) and the FHD BGRA target;
  3. runs the **UYVY pass**: FHD BGRA → an `R8G8B8A8_UNORM` 960×1080 target, each texel = U Y0 V Y1, BT.709 limited range from the full-range RGB, chroma = the two pixels' average (`Texture2D.Load`, no filtering);
  4. `CopyResource` into `staging[k]` (R8G8B8A8, `CPU_ACCESS_READ`, a 3-deep ring);
  5. ends the event queries, flushes once, waits once (today's `wait_until_each_done`);
  6. maps `staging[k]` with `DO_NOT_WAIT` (already complete), copies the rows into a `frame_pool` buffer (`unpad_rows_into`, `readback.rs:341`), unmaps, and pushes `(stamp, SharedFrame)` to `NdiFeed`;
  7. **then** waits for `due` and sends MAX, then FHD, over Spout, as today.

  The readback sits BEFORE the Spout sends, so a 67 ms Spout lock wait never delays NDI.
- **Fades, transitions, the #251 blend.** Unchanged in shape: `MaxJob::Fade{from, to, weight_q8}` from `Pair::max_job` (`program_output.rs:273-300`). The same compose now also yields the NDI picture, so the CPU `Canvas::fade` / `paint_mix` are no longer used. Transition audio stays on the CPU (`split`, `mix_audio_block`). The cue gate, the windows and the scene-off logic are untouched (`program-transition.md`).
- **Black:** `Composition::Black` is still drawn. Its UYVY readback is constant: one cached `SharedFrame`, no map.

### 5.6 NDI `SP-program` from the readback

- **`playback/ndi_feed.rs` (pure):**
  - a stamp-keyed slot of ≤ 3 read-back pictures from `program-gpu`, coalescing the oldest;
  - a **one-slot delay line** on `program-output`.
- **`serve(b)` now does:**
  1. VBAN(b);
  2. offer `MaxJob`(b);
  3. NDI submit of boundary **b−1**: audio block (b−1), kept in the delay line with its `shown` item frame, plus the UYVY picture stamped b−1;
  4. stamps unchanged (`video_tc` = b−1's floored boundary, `audio_tc` as served).
- **Why one slot.** `compose_at − offer` ∈ [0, period + 3 ms] ≈ [0, 19.7] ms (§3.4, `program_max_vblank.rs`). Compose + map ≈ 2–3 ms with no uploads. The readback is therefore ready ≤ ~23 ms after the offer, under the 33.3 ms to the next serve.
- **A late or missing readback** (coalesced MAX job, device loss) sends the audio with the previous picture: `ndi.repeats` +1, never a wait.
- **The burn** paints into the read-back buffer in place (it is exclusively ours): Y 16/235, U = V = 128 in UYVY. The rqrr tests are re-pinned on UYVY.
- **sp-ndi** gains `PixelFormat::Uyvy` (FourCC `'U','Y','V','Y'`).
- **The trace and `health.timing.submit_us`** now time the NDI submit alone. The trace records which stamp went out at each serve.
- **Cost:** +33.3 ms constant arrival lag of `SP-program` at its receivers (cg OBS, Presenter). Picture and audio move together, so the A/V relation on NDI is unchanged. camera-box's FIFO latches one frame deeper, and the #228 CG-segment measurement reads +33 ms. Tell camera-box before the rollout (§8 R2).

### 5.7 Preview and JPEG (per playlist, on demand)

- **The preview is per PLAYLIST**, off program too (`preview.md`, #266 `Offered`). It cannot come from the program readback.
- **The A/V invariant stays.** Audio and video are offered together at the decode seam (`preview.md`, "The offer seam").
- **`PreviewStage`** on the producer device, called from `paced_producer::next_item` ONLY when a tap wants a picture (the stream tap has a viewer, or the JPEG rate gate passes):
  1. draw the frame aspect-fitted into a 640×360 BGRA target, with the same `aspect_fit` (`sp_core::fit`);
  2. run the UYVY pass, adapted to NV12 output for the encoder (or feed UYVY to ffmpeg);
  3. read back synchronously on the producer thread, which runs ~400 ms ahead of the pacer.
- Cost while watched: ~0.35 MB × 25/s ≈ 9 MB/s. With nobody watching: zero.

### 5.8 Fallback matrix (R6)

| Case | Detected by | What happens | Cost |
|---|---|---|---|
| Codec or profile NVDEC lacks (H.264 Hi10P, 4:2:2, AV1 on a pre-Ampere GPU) | MF hands a system-memory sample (`DecodePath::of_picture`) | `Picture::Cpu`; the compositor uploads once (today's path) | today's cost for that file; `path=software`, amber health |
| Hardware open fails | `HwSession` / reader error | Software open (`FallbackStage::Open`, existing) | as above |
| Decode error mid-stream | `FallbackGate` (existing) | Software reopen once, resume at the last picture (`Resume`) | ≤ one GOP of held picture + silence (`video-decode.md`) |
| Producer device lost | `DXGI_ERROR_DEVICE_REMOVED` at copy, or `WAIT_ABANDONED` | Ring dropped; reader reopens in software for the song; the next song rebuilds the device after a 30 s backoff | missing frames drawn as black, ≤ queue depth |
| Compositor device lost | existing (`lose_device`) | Rebuild; ring slots reopened lazily; NDI repeats meanwhile | counted, as today |
| Ring exhausted | `RING_WAIT` bound | Decode error → fallback gate | counted |
| VRAM budget exceeded | `IDXGIAdapter3::QueryVideoMemoryInfo` | New GPU opens refused → software (P4) | counted |
| No Store VP9/AV1 extension | the open fails in both modes | Existing open-failure handling (#229); the FFmpeg backend is the named remedy | — |
| 10-bit | decoder format P010 | P010 ring slot + R16 views (P4) | none |

### 5.9 Settings and API

- **`video_decode`** = `software` | `gpu`, re-read per song like `video_hw_decode` (`video-decode.md`). The default flips to `gpu` after P4's gates. `video_hw_decode` (the readback mode) is deleted in P5.
- **`program_ndi_source`** = `gpu` | `cpu`: temporary, for P1b's wall A/B, deleted in P5.
- **`program_max_enabled` / `program_spout_fhd_enabled`:** new meaning as described in §5.1.

### 5.10 Telemetry

- **`GET /api/v1/program` → `gpu`:**
  - `adapter`, `caps` (P0);
  - `vram_budget_mb` / `vram_used_mb` (`QueryVideoMemoryInfo`);
  - `upload_mb_s` / `uploads_per_s` (bytes of `Cpu` sides uploaded);
  - `gpu_sides_per_s` / `cpu_sides_per_s`;
  - `km_acquire_timeouts`, `abandoned`;
  - `readback_mb_s`, `readback_us_p50/p99`.
- **`GET /api/v1/program` → `ndi`:** `{source, delay_slots: 1, repeats, readback_late, fourcc: "UYVY"}`.
- **`GET /api/v1/status.video_decode.pipelines[]`:** `{playlist_id, decoding, path: gpu|software|none, codec, width, height, ring{slots, in_use, waits, copy_us_p99, vram_mb}}`. It also exposes stray decoding pipelines (the 11-playlists finding, comment 6214465904).
- **Logs:** one INFO per song (path, codec, size, ring); WARN on a fallback and on an abandoned slot.
- **Health bar:** an amber `Video: CPU dekódovanie (<codec>)` while the on-program source decodes in software.
- **The one number to read on the box:** `gpu.upload_mb_s` = 0 with a hardware-decodable song on program.

### 5.11 Invariants kept (R4)

- **The pacer and genlock stamps are unchanged.** They never look at pixels (`genlock.md`).
- **Audio** goes decoder → pacer → VBAN at `serve(b)` exactly as today. NDI audio leaves one slot later together with its picture.
- **MAX's wall timing** (the compose phase, the send phase, the slot re-picks) is unchanged. The compose gets shorter, since there are no uploads, which lets P6 shrink `COMPOSE_BUDGET`.
- **#257 held tails, #277 early end, #217 real-seek reports and #229 open failures:** unchanged. They use timestamps and handles.

### 5.12 Byte budget after (estimate)

For a hardware-decodable 4K source:

| Stage | Traffic |
|---|---|
| Decode, ring copy, compose | VRAM only |
| UYVY readback | 124 MB/s PCIe d2h |
| Row copy into the pool (read + write) | ~250 MB/s |
| NDI encode input | ~124 MB/s |
| **Total** | **≈ 0.4–0.5 GB/s of RAM traffic, 0 h2d** |

That is against ≈ 2.5 GB/s + 0.3 GB/s h2d today. All of it happens at the vblank-locked compose phase or on the NDI thread; none happens in decode-driven bursts.

---

## 6. Test strategy

**Linux, pure, mutation-gated** (the repo's split: decisions out of `win/`):

- `ring_state.rs`:
  - slot allocation and reuse;
  - size classes (2 kept);
  - generations;
  - the key protocol;
  - back-pressure;
  - "never reused while referenced";
  - `copy_box(visible, surface)` (even rounding, within the padded surface).
- `uyvy.rs`: the CPU reference of the UYVY pass (BT.709 limited range, pair average, rounding) and the row packing.
- `ndi_feed.rs`:
  - the delay line pairs audio(b−1) with picture(b−1);
  - a late or missing picture gives a repeat;
  - coalescing;
  - stamps unchanged.
- `Picture` plumbing through the pacer and bus tests, driven by `sp_gpu::FakeRing` (doc-hidden, on every platform). Its `GpuFrame`s carry ids and sizes but no texture, so all pacer, bus and blend tests run with `Gpu` pictures.
- The `program-gpu` worker on the fake GPU:
  - the readback happens before the sends;
  - a late compose produces a repeat;
  - a device loss;
  - MAX off still composes FHD + UYVY.
- Fallback decisions (`hw_decode.rs` extended): the `path` table.

**Windows CI on WARP** (`windows-latest`; a runner capability is proven before a test rests on it, per `rust-workspace.md`):

- **P0 probes, which fail and never skip:**
  - `ExtendedResourceSharing`;
  - an NV12 `Texture2D` with `SHARED_NTHANDLE | SHARED_KEYEDMUTEX` created on WARP device A and opened on WARP device B;
  - R8/R8G8 SRVs on the opened NV12;
  - the keyed-mutex round trip (key 0 → 1, then a 0 ms acquire);
  - an R8G8B8A8 RTV plus a staging map with `DO_NOT_WAIT`.
- **The ring's real copy code** (`copy_from`) on a *decoder-like* source: an NV12 `Texture2DArray` (ArraySize 4) with initial data per slice (the existing CI pattern, `video-decode.md` "The DXGI readback runs on a REAL surface"). It copies slice 2 into a slot, so it exercises the subresource index and the visible-size box.
- **Two-device compose:** slot from device A → compositor on device B → read back MAX, FHD BGRA and UYVY. Pin against `reference.rs` (one code per quad, smooth pictures) and `uyvy.rs` (±1 code against the actual BGRA read back).
- **The real loop** (`program_max_tests_warp.rs` style): mock NDI. The NDI wire bytes are the UYVY of the previous boundary, one slot late, with unchanged stamps.

**Not testable in CI** (WARP refuses the video device, `video-decode.md`):

- DXVA/NVDEC decode into the ring;
- decoder-surface copy timing;
- a real device-loss cascade;
- Spout into Arena;
- the wall.

These are proven by the box decode bench and the post-deploy gates:

- **Decode bench** `"mode": "gpu_resident"`: `path: gpu`, `copy_us`, decode mean ≤ 50 % of 1/f for 4K AV1/VP9 (the S0 D2 gate), `vram_mb`.
- **Post-deploy gates:**
  - `gpu.upload_mb_s == 0` while a hardware-decodable song is on program;
  - `ndi.repeats` +0;
  - `km_acquire_timeouts` +0;
  - an NDI receiver reads 1920×1080;
  - the A/V gate passes;
  - `max-gate.ts` unchanged.

---

## 7. The real-wall measurement protocol (fixed for every phase)

**Box and preconditions**

- SNV first; PP for smoke and the monitor.
- Record: build, settings (`video_decode`, `program_ndi_source`, phases), HAGS state, Arena uptime (no Arena restart inside a take), heavy-child state.
- Only the on-program pipeline decodes. Check every pipeline's `transport` and `video_decode.pipelines[].decoding`, and pause strays (comment 6214465904).

**Content (fixed ids)**

- `wall-25` (blend) and `wall-30` frame-ID clips (#272, `test_item_clips.rs`).
- Blank (the floor).
- One named catalog 4K AV1 song and one 1440p VP9 song.

**Feeds**

- **(A)** The operator's clip on `SP-program-MAX`: production.
- **(B)** The independent Spout sender `spout_test_sender.py` on the clip: the control that isolates SongPlayer's disturbance (the method of comment 6212436810).
- **(C)** NDI `SP-program`: only for P1b.

**Metric**

- Exact jumps back per 40 s (`wall_frames.py` / `wall_analysis.py`, `--refresh-hz 60`, `test-item-burn.md`).
- One-refresh runs (`ops\wall_runs.py`).
- Each block binned by the offer and compose phase after the primary vblank (`max.send_phase_us_*`, `compose_phase_us_*`, `backs_vs_offer.py`).

**Design**

- A/B interleaved in one-minute blocks by PATCHing the phase's switch, over **≥ 46 blocks**: one full ~45 min drift cycle per side, like `fhd_ab2.ps1`. Then A again.
- Never conclude from a short take (owner, #272 comment 6170760353).

**Alongside**

- `gpu.upload_mb_s`, `readback_mb_s`, `ndi.repeats`, Arena CPU and FPS.
- cg OBS `genlock-fifo audit` for the `SP-program` input (`shallow_depth`, `late_holds`, `underruns`).

**After each production change**

- The 72 h continuous monitor (`wall_monitor5.py`) on normal services.

**Acceptance**

- **P3a end state:** for (A) with `wall-25`, `wall-30` and the 4K song, every phase bin within the Blank band (0–3 per 40 s, + one standard deviation of Blank); no bin worse than the A side.
- **Any phase:** a worse A/B means revert the same day (`autonomous-quality-discipline.md`, #963).

**Record**

- The table and the raw log paths on the epic, per phase.

---

## 8. Risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | NV12 cross-device sharing, NT handles or keyed mutex refused on WARP (no CI proof) or on a box GPU or driver | P0 probes first. If WARP refuses NV12 sharing, CI pins the ring's mechanism on an R8/BGRA test ring and the NV12 specifics on one device; the box probe decides. Chromium shipped the same mechanism on NVIDIA for years. |
| R2 | NDI one-slot delay: +33 ms `SP-program` arrival lag; cg OBS FIFO one frame deeper; the CG-segment measurement (#228) +33 ms | A constant (never drifting) delay, stamps unchanged. Tell camera-box before P1b (their issue 1302 tracks `shallow_depth`). P1b's A/B setting allows a same-day revert. |
| R3 | VRAM: ~0.4 GB per decoding 4K pipeline on an 8 GB GPU shared with Arena | `QueryVideoMemoryInfo` telemetry and a budget guard (P4); ring capacity tied to the queue; P6 lowers `DECODE_QUEUE_BOUND` for GPU frames (12 → 6, also halves the #184 fader latency); idle producer devices released after 10 min. |
| R4 | The NDI program now depends on the `program-gpu` thread (coalesced jobs, builds, a lost device mean NDI repeats) | Readback before the Spout sends; repeats counted and gated at +0; the FHD build stays after MAX's send (#239). |
| R5 | Keyed-mutex acquire cost or a driver quirk on NVIDIA under multithread protection | 0 ms acquire on the compositor (Chromium); `km_acquire_timeouts` telemetry; bounded producer acquire. |
| R6 | MF hardware decoder quirks: a silent software fallback, padded surfaces, P010 | Path detected per picture (existing); visible-size box copy; P010 slots (P4). |
| R7 | Device loss cascade (TDR removes every device on the adapter) | Every holder rebuilds from durable state; producer rebuild with backoff; missing sides drawn black; one fallback per song. |
| R8 | PP is a hybrid-graphics laptop: Arena and SongPlayer must stay on the dGPU (Spout across adapters copies through RAM) | P0 reads PP's adapters and outputs (`gpu.caps`, `max.adapter`); the gate already requires a non-Basic adapter. |
| R9 | Confounds in the wall data: Arena restarts, the NDI-vs-MAX flip after restarts (comment 6214465904), stray decoding pipelines | The protocol's preconditions (§7); the independent-sender control; interleaved blocks. |
| R10 | Tier-0: no local compile, so Windows-only code is proven only in CI | Thin glue, pure decisions, the WARP pins listed in §6; `cargo mutants --list` before review (`rust-workspace.md`). |

---

## 9. Phased plan (issue-sized, serial, measured on the wall)

Order: the NDI readback must exist before decode may stay on the GPU. Otherwise NDI would need a full-size readback per native frame, which is today's bad `video_hw_decode` path. So the wall's big step is P3a; P1b is the first real wall improvement.

| Phase | Issue | Code (prod) LoC | Wall measurement |
|---|---|---|---|
| P0 | Baseline telemetry + capability probes | ~300 | baseline table (current build) |
| P1a | sp-gpu UYVY pass + staging ring + reference | ~400 | none (not wired); WARP pins |
| P1b | NDI `SP-program` from the GPU readback (one-slot delay) | ~500 | A/B `program_ndi_source` gpu vs cpu; NDI and A/V gates |
| P2 | Picture ring + `GpuFrame` + `Picture` plumbing (all `Cpu` in production) | ~550 | A/B identical (no runtime change) |
| P3a | GPU-resident decode into the ring (`video_decode=gpu`) | ~450 | **the main A/B**, expected ≈ Blank |
| P3b | Preview + JPEG from GPU frames (`PreviewStage`) | ~300 | no regression with a viewer open |
| P4 | Robustness: device loss, VRAM guard, P010, gates, health, default flip | ~500 | 72 h monitor SNV + PP |
| P5 | Delete the superseded paths | −2 000 / +150 | A/B identical |
| P6 | Tune with the headroom: queue depth, `COMPOSE_BUDGET`, phases | ~200 | phase sweep over a drift cycle |

### P0: baseline and probes

- `ComposeStats.upload_bytes` → `gpu.upload_mb_s`; the `hw_counters` readback bytes; `status.video_decode.pipelines[]` (reveals stray decoding).
- `sp-gpu` `probe.rs` (pure verdict) + `win/probe.rs`; the WARP probe tests (§6); `diag decode-bench "probe": true` on the box GPU (caps, adapters and outputs, VRAM budget).
- **Gate:** CI green; probe results for SNV and PP recorded on the epic.
- **Wall:** the protocol (§7) on the current build, both feeds (A) and (B). This is the reference for every later phase.
- **Prerequisite:** the stray-decoding bug from comment 6214465904 (playlists left playing off program; `test-item/stop` not pausing). It is not filed as an issue yet; file and fix it first. It is the cheapest wall win and a measurement precondition.

### P1a: the UYVY pass (sp-gpu)

- A UYVY target on the compositor (960×1080 R8G8B8A8), the pass shader, the staging ring (3), `Compositor::read_uyvy(&mut Vec<u8>)`; the CPU reference of the pass is the pure `uyvy.rs` (`uyvy::reference`), next to `reference.rs` (design comment 6216639388).
- **WARP:** UYVY within ±1 of the reference computed from the BGRA read back; `DO_NOT_WAIT` after the compose wait never returns `WAS_STILL_DRAWING`.

### P1b: NDI from the GPU

- The compositor runs while MAX or NDI wants it: its PRIMARY target is the FHD program, MAX's target is added only while MAX is on (design comment 6218637444).
- The worker (`program_max_ndi.rs`): `read_uyvy` → `MaxOut::ndi` (the pure `ndi_feed.rs`), after the compose and before the sends; a Black boundary pushes the cached UYVY black.
- `program-output` (`program_output_ndi.rs`): the delay line (`ndi_source_for`: the canvas again past 30 stale slots; the switches); `FourCCVideoType::UYVY`; `program_burn::burned_uyvy`; `program_ndi_source` (default `cpu`); `ndi` on `GET /api/v1/program`.
- ROZHODNUTÉ 6232669201 (17.10.2026): a switch from the GPU back to the canvas sends the held pair first, then the boundary on the canvas (no audio block lost); after an automatic fallback NDI returns to the GPU only after 30 on-time readbacks in a row (`ReturnGate`); the one-slot delay stays, and the box check of `ndi.repeats` / `readback_late` against `health.timing.ready_late_us_*` over a whole drift cycle runs before SNV is flipped to `gpu`.
- The fake GPU on Linux (`FakeCompositor`'s `enable_uyvy` / `read_uyvy`); WARP: the real loop's UYVY within ±1 of `uyvy::reference` of the FHD target.
- **Gates:** NDI receiver 1920×1080; `ndi.repeats` +0; A/V gate; `max-gate`; `genlock-fifo audit` stable (`shallow_depth` +1 expected and stable).
- **Wall:** A/B `program_ndi_source`, ≥ 46 blocks. The CPU canvas fit and fade leave the drifting phase, so expect fewer jumps at the bad phases for 4K and blended content.

### P2: the picture ring

- `PictureRing`, `ring_state.rs`, `GpuFrame`, `FakeRing`; the compositor's `Side::Gpu` with the open-once SRV cache and the keyed mutex; the `Picture` enum through sp-server (mechanical).
- **WARP:** two-device pins, the decoder-like array copy.
- **Wall:** A/B identical. Production still uses `Cpu` only.

### P3a: GPU-resident decode

- `DecodeMode::GpuResident`, `ProducerGpu` per pipeline (lazy, kept across songs), `copy_from` in `read_picture`, setting `video_decode`.
- Preview taps get no picture from `Gpu` frames until P3b. The Player shows the preview as unavailable; the main session enables `gpu` on SNV for the measurement window only.
- **Box:** decode bench `gpu_resident` (4K AV1 / VP9 within the D2 gate); `gpu.upload_mb_s == 0` on the 4K song.
- **Wall: the main A/B** (`video_decode` gpu vs software), then the 72 h monitor.

### P3b: preview from GPU frames

- `PreviewStage` (640×360 aspect-fit, UYVY→NV12 or a UYVY encoder input), only when a tap wants a picture.
- The PP preview gate (`post-deploy-pp-preview.spec.ts`) passes with `gpu`.
- **Wall:** no regression with the preview open (A/B viewer on and off).

### P4: robustness and rollout

- Producer device loss (abandoned slots, rebuild backoff), the VRAM budget guard, P010 slots, the HDR flag, the health segment, the post-deploy gates (§6).
- The `video_decode` default becomes `gpu` on SNV, then on PP.
- **Wall:** the 72 h monitor on both sites.

### P5: delete the superseded paths

Owner rule: superseded paths are deleted, not kept as a fallback (memory "Delete legacy directions").

- `program_canvas.rs`, `nv12_mix.rs`, `band_pool.rs` and the CPU fit;
- `diag/fit_bench.rs`;
- `dxgi_frame.rs`'s `Lock2DSize` readback and `video_hw_decode`;
- `program_ndi_source`;
- the #251 step 4 video quiet window, if P1b's data confirms the earlier "no effect" A/B (comment 6213507561).
- The rules docs (`gpu-max.md`, `video-decode.md`, `program-bus.md`, `program-transition.md`, `preview.md`, `test-item-burn.md`) are rewritten.
- With no GPU at all, `SP-program` then sends black with live audio and a red health state. Arena needs the same GPU, so the wall is down in that case anyway.

### P6: tuning

- `DECODE_QUEUE_BOUND` for GPU frames (12 → 6: VRAM, and the #184 fader latency).
- `COMPOSE_BUDGET` (14 → ~6 ms, since there are no uploads; less MAX latency).
- Re-sweep the compose and send phases over a drift cycle.
- An optional zero-copy NDI lease (send the mapped staging directly) if `readback_us_p99` matters.

---

## 10. Decisions taken in this design (the owner may overrule)

1. **NDI `SP-program`** goes out one grid slot later (+33 ms, constant). This keeps all GPU work at the vblank-locked phase, which is what #251 measured as decisive. camera-box is told beforehand.
2. **NDI pixel format** becomes UYVY, per NDI's own guidance. Receivers see no format change: they receive SpeedHQ.
3. **The preview** stays per playlist from the decode seam, with its own tiny GPU readback, not from the program readback. That keeps preview A/V and the off-program cards working ("one app, one behaviour").
4. **The CPU canvas** is deleted in P5, with no CPU-compositor fallback (owner's "delete legacy directions").

---

## 11. Sources

**Repo** (at `07a5f674`):

- Every `file:line` cited in §3 and §5.
- The rules `.claude/rules/gpu-max.md`, `video-decode.md`, `program-bus.md`, `program-transition.md`, `genlock.md` (#251 blend, #203 pool), `preview.md`, `test-item-burn.md`.
- `.claude/skills/win-resolume-ops/SKILL.md:462` (PP machine).
- `windows` 0.58 bindings (`~/.cargo/registry/.../windows-0.58.0/src/Windows/Win32/`):
  - `Media/MediaFoundation/mod.rs`: `IMFDXGIBuffer` 6971–6979, `IMFDXGIDeviceManager` 7005–7029, `MF_SA_D3D11_*` 19732–19744, `MF_SOURCE_READER_D3D11_BIND_FLAGS` 19813, `IMFSourceReaderEx` 12927;
  - `Graphics/Direct3D11/mod.rs`: `ID3D11Device1::OpenSharedResource1` 816, `ID3D11Device5` 1024–1032, `CopySubresourceRegion` 1270, `ID3D11DeviceContext4` 1935–1942, `ID3D11Fence` 1980–1992, `ID3D11Multithread` 2573, `MISC_SHARED_KEYEDMUTEX` / `NTHANDLE` 6869–6870;
  - `Graphics/Dxgi/mod.rs`: `IDXGIKeyedMutex` 1027–1031, `IDXGIResource1::CreateSharedHandle` 1472.
- #251 comments 6212436810, 6213507561, 6214465904, 6214891622; #223 revision 3 (comment 5979609879, R3-3).

**Microsoft Learn**

- https://learn.microsoft.com/en-us/windows/win32/medfound/mf-source-reader-d3d-manager
- https://learn.microsoft.com/en-us/windows/win32/medfound/supporting-direct3d-11-video-decoding-in-media-foundation
- https://learn.microsoft.com/en-us/windows/win32/medfound/mf-sa-d3d11-bindflags
- https://learn.microsoft.com/en-us/windows/win32/medfound/mf-source-reader-enable-advanced-video-processing
- https://learn.microsoft.com/en-us/windows/win32/api/mfobjects/nf-mfobjects-imfdxgibuffer-getsubresourceindex
- https://learn.microsoft.com/en-us/windows/win32/api/mfobjects/nf-mfobjects-imfdxgidevicemanager-resetdevice
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_bind_flag
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_resource_misc_flag
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ns-d3d11-d3d11_feature_data_d3d11_options
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11device-opensharedresource
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiresource1-createsharedhandle
- https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgikeyedmutex-acquiresync
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11_4/nf-d3d11_4-id3d11device5-createfence
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11_3/nf-d3d11_3-id3d11devicecontext4-wait
- https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_map_flag
- https://learn.microsoft.com/en-us/windows/win32/api/dxgiformat/ne-dxgiformat-dxgi_format
- https://learn.microsoft.com/en-us/windows/win32/medfound/direct3d-12-video-overview
- https://learn.microsoft.com/en-us/windows/win32/direct3d12/direct-3d-11-on-12
- https://learn.microsoft.com/en-us/windows/win32/api/mfmediaengine/nf-mfmediaengine-imfmediaengine-transfervideoframe

**NVIDIA and NDI**

- https://developer.nvidia.com/video-encode-and-decode-gpu-support-matrix-new
- https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-application-note/index.html
- https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-video-decoder-api-prog-guide/index.html
- https://docs.nvidia.com/cuda/cuda-driver-api/group__CUDA__D3D11.html
- https://docs.ndi.video/all/developing-with-ndi/sdk/performance-and-implementation
- https://docs.ndi.video/all/developing-with-ndi/sdk/ndi-send
- https://docs.ndi.video/all/developing-with-ndi/sdk/frame-types

**Source of established apps**

- mpv: https://raw.githubusercontent.com/mpv-player/mpv/master/video/out/d3d11/hwdec_d3d11va.c, `DOCS/man/options.rst`
- FFmpeg: https://raw.githubusercontent.com/FFmpeg/FFmpeg/master/libavutil/hwcontext_d3d11va.h, `libavcodec/dxva2.c`, https://ffmpeg.org/legal.html
- OBS: https://raw.githubusercontent.com/obsproject/obs-studio/master/shared/media-playback/media-playback/decode.c
- CasparCG: https://raw.githubusercontent.com/CasparCG/server/master/src/modules/ffmpeg/producer/av_producer.cpp
- Chromium 100: https://raw.githubusercontent.com/chromium/chromium/100.0.4896.127/media/gpu/windows/dxva_video_decode_accelerator_win.cc (lines 1073, 1093, 1656–1658, 1751–1752, 2815–2888), `dxva_picture_buffer_win.cc:737`
- Wine `mfreadwrite/reader.c`, makepad `windows_mf_source_reader.rs` (the undocumented `MF_SOURCE_READER_D3D11_BIND_FLAGS`)
- Spout2 `SpoutDirectX.cpp` (legacy `MISC_SHARED` sender texture)
