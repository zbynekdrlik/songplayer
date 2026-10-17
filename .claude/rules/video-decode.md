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
  Nothing downstream can tell the paths apart.
- **The picture's size is the STREAM's, never the current type's on the
  D3D11 path** (#223 follow-up, 9.10.2026: a green strip at the bottom of
  some songs on the wall and in Presenter). NVDEC pads the surface AND the
  current type's `MF_MT_FRAME_SIZE` to 16 rows: the decode bench with
  `"hw": true` gave 1920×1080 AV1 → 1920×1088, 2048×858 → 2048×864,
  2560×1350 → 2560×1360 (software: the stream's size), and those rows are
  no picture (Y = U = V = 0 = green). The reader reads the native
  (compressed) type's `MF_MT_FRAME_SIZE` at open (`native_frame_size`) and
  hands every D3D11 picture over at `hw_decode::visible_size(decoded,
  native)`: the top-left native-size rect when it fits, else the decoded
  size. Every height that is not a multiple of 16 was hit (AV1 1080p, the
  1350 / 1098 / 1182 / 858 widescreen songs); 1440 / 1280 / 1072 / 720
  were not. Check a fix on the box with the bench (`hw` true and false
  must report the same size).

## The decode cap (#282): a picture larger than `max_resolution` is decoded scaled

Design: #282 comment 6233842555 (Approach 1); the lane's implementation
note, comment 6233930202 (its "+15 rows on the D3D path" and "a scaled
D3D picture counts as hardware" were superseded by review rounds 1–2,
below). Asked as a lever until #279 (the GPU-resident path): the wall's
jumps back follow the bytes SongPlayer moves (#251 comment 6232632646).
The owner then made it a permanent switch per site (ROZHODNUTÉ on #282,
17.10.2026: "musi byt sposob to zmenit"), so `max_resolution` caps
download AND decode for good. SNV runs it at 1440 since 17.10.2026 15:09Z
(the box A/B on #282: 10-15× fewer jumps back under NVENC, from two
ABA rounds of 40 s takes on one AV1 song, a regression check and not a
proof of a frame-exact path). The
follow-up lane's design is #282 comment 6241829405.

**Automatic.** Setting `max_resolution` back to automatic restores
3840×2160 with no code change, WITH hardware decode on (SNV, PP). The rule
is the download's, so automatic still scales three kinds of file (review
rounds 1–2):

- a picture wider than 16:9 at 2160 rows (DCI 4096×2160 → 3840×2026) is
  scaled on the GPU;
- anything taller than 2160 rows (a portrait 2160×3840 → 1216×2160) is
  scaled too;
- on a box with `video_hw_decode` off (automatic = 1440), every 4K file is
  scaled on the CPU.

- **One knob.** `max_resolution` (Nastavenia "Najvyššie rozlíšenie
  (sťahovanie aj prehrávanie)", with the hint `settings-max-resolution-hint`:
  the owner's per-site switch when 4K is a problem) caps decode through the download's own rule
  `downloader::format::max_resolution(raw, hw_decode)`: automatic = 2160
  with hardware decode, else 1440; a number clamped to 480..=2160. The 5 s
  settings task (`video_decode.rs`) applies it with `video_hw_decode`
  (`HwDecodeSetting::max_height`); the paced producer opens each song with
  `open_with(path, mode, Some(cap))`. A playing song keeps its reader.
- **The size** (`hw_decode::scaled_size`, pure, mutation-gated): the box is
  cap × 16/9 wide and cap tall, both rounded down to even (`decode_box`;
  1080 → 1920×1080, 480 → 852×480). A picture inside it → `None`: it opens
  EXACTLY as before. A larger one is scaled by min(box w / w, box h / h):
  the side that binds takes the box's size, the other is rounded to the
  nearest even (a tie upwards), each ≥ 2. 3840×2160 at 1080 → 1920×1080;
  3840×1606 → 1920×804; 4096×2160 at 2160 → 3840×2026. The cross products
  are u128 (no overflow at any `u32`).
- **The open** (`video/mf_reader.rs::create`, the scaled half in
  `video/mf_reader_scaled.rs` and the Media Foundation reads in
  `video/mf_scaled.rs`): the first source reader is made as before and only
  read for the native type (no output type set yet, so no decoder MFT is
  loaded). A scaled open makes a SECOND reader (`open_scaled` →
  `source_reader(.., advanced)`) with
  `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` = 1
  (`MF_READWRITE_DISABLE_CONVERTERS` must stay unset). Microsoft: the
  source reader then inserts a video processor that matches the output
  type's frame rate, frame size, interlace mode, pixel aspect and subtype.
  On the D3D path it is D3D11-aware and scales on the GPU before the
  readback, so the readback, the frame pool, MAX's upload and the canvas
  all carry the smaller picture. In software it scales on the CPU.
- **The scaled output type** (`mf_scaled::ScaledType::apply`) carries:
  - `MF_MT_FRAME_SIZE` = the scaled size;
  - `MF_MT_PIXEL_ASPECT_RATIO` = `hw_decode::scaled_pixel_aspect`. This
    keeps the native display aspect exactly (3840×1606 → 1920×804 is
    804:803), so a processor that keeps the aspect letterboxes no one-pixel
    edge;
  - the native type's `MF_MT_YUV_MATRIX` and `MF_MT_VIDEO_NOMINAL_RANGE`
    when it has them. The processor then converts no colour; a size-based
    default, e.g. BT.601 for an SD output, would shift the hues the
    compositor reads as BT.709.

  It carries NO frame rate (review round 2). A rate of ours could start
  the processor's frame-rate conversion when the decoder reports another
  rate, putting a periodic repeated picture on the wall. The reader reports
  the processor type's rate, else the native type's
  (`hw_decode::scaled_frame_rate`), never the 29.97 guess for a scaled open.
- **Never a dead song, no green strip.** Each of these is a refusal:
  - the negotiated type is not EXACTLY the requested size
    (`hw_decode::scale_took`), on the D3D path too. Behind the processor
    the output type is ours and the picture fills it, so a padded type
    would be picture the readback cuts off;
  - the decoder's padding could reach the processor
    (`hw_decode::scaled_open_safe`, the green strip; review round 3). The
    decoder's output type is read from the reader's chain at open
    (`IMFSourceReaderEx::GetTransformForStream` → the
    `MFT_CATEGORY_VIDEO_DECODER` → `GetOutputCurrentType(0)`: its
    `MF_MT_FRAME_SIZE` and `MF_MT_MINIMUM_DISPLAY_APERTURE`):
    - padding seen at open without exactly the native aperture refuses;
    - on the D3D path NVDEC may learn its padded size only at the first
      frame, AFTER this check (a VP9 / AV1 sample entry carries no frame
      header). So there a stream that is not whole 16-pixel blocks
      (3840×1606, 2560×1350) is scaled only when the decoder already shows
      the native aperture at open, and an unreadable chain refuses it.
      NVDEC's 16-row padding cannot reach whole blocks (3840×2160,
      2560×1440, 4096×2160), so the 16:9 majority keeps the lever UNLESS
      the decoder shows padding at open without the native aperture: that
      refuses on any size. Seen on SNV (17.10.2026, AV1 3840×2160, D3D11):
      the decoder showed `3840×2160`, no aperture, at open;
    - in software only the padding seen at open refuses, and an unreadable
      chain is let through with a WARN naming the file (the same WARN
      lets a D3D11 stream of whole blocks through; it names `path`). What
      is PROVEN of the software decoders: CI run 39040435639 printed
      the runner's H.264 software decoder (the line names no MFT)
      showing `160×120`, no aperture, AT OPEN for
      the fixture coded 160×128. The VP9 / AV1 software decoders and any
      decoder's type after its first frame are not proven; the CI log's
      `at the first picture` line (below) shows H.264's;
  - Media Foundation refuses the attribute or the type.

  A refusal logs WARN `mf_reader: the scaled open was refused; this file
  decodes at its own size` (the error names the negotiated size or the
  decoder's output), counts `scale_refusals` +1, and configures the first
  reader unscaled. A padded TEXTURE with an exact type is fine: the
  readback takes the UV offset from the texture's height.
- One INFO per scaled open: `mf_reader: opened scaled to the decode cap
  (#282); the decoder's own path is not visible behind the video
  processor`, `scaled_opens` +1. Its fields:
  - `from`, `to`, `pixel_aspect`, `path` (D3D11 / software);
  - `decoder`: what the decoder hands the processor, `Ok(DecoderOutput
    {size, aperture, colour})` or why the chain could not be read;
  - `chain`: the source reader's transforms in order (`Decoder`,
    `VideoProcessor`, `Other(<category GUID>)`, or the `Error` that ended
    the walk);
  - `native_aperture`: the native type's `MF_MT_GEOMETRIC_APERTURE`;
  - `colour_native` / `colour_out` / `colour_decoder`: the matrix and the
    nominal range (`MF_MT_YUV_MATRIX`: 1 = BT.709, 2 = BT.601;
    `MF_MT_VIDEO_NOMINAL_RANGE`: 1 = full, 2 = 16..235) of the native type
    (put on the scaled type), the negotiated scaled type and the
    decoder's output. `colour_out` mostly echoes the native type (ours).
    Only two STATED, unequal values of `colour_out` and `colour_decoder`
    suggest a conversion; an unstated (`None`) side says nothing, as the
    processor then takes its own default. The proof stays the hue check
    against the uncapped song.
- **The first scaled picture** (#282 follow-up): the decoder MFT found at
  the open is kept and its output type read again ONCE there, then let go,
  with no `GetTransformForStream` call mid-stream (Microsoft: drain the
  reader before calling it). One INFO `mf_reader: the decoder's output at
  the first scaled picture (#282)` with `at_open`, `at_first_picture` and
  `padding_in_reach` (`hw_decode::padding_reaches_processor` on the new
  type against the stream's own size; also in the facts,
  `first_picture_padding_in_reach`). On the box, `padding_in_reach=true`
  is where the green strip is expected (NVDEC padded after the open's
  check; that the processor crops only by `MF_MT_MINIMUM_DISPLAY_APERTURE`
  was reasoned in review, not measured): look at that file's bottom rows
  on MAX.
- The reader's `scaled_from()` is the stream's own size;
  `scaled_decoder_output()` and `scaled_open_facts()` (doc-hidden,
  `ScaledOpenFacts`, `video/mod.rs` re-exports it) are what the open read.
- **Where the code lives:** the reader's scaled half (`open_scaled`, its
  log lines, the first picture's re-read, the accessors) is the CHILD
  module `video/mf_reader_scaled.rs` (`#[path = "mf_reader_scaled.rs"]
  mod scaled;` inside `mf_reader.rs`, resolved next to it). As a child it
  reaches the reader's private fields and `source_reader` / `configure`,
  so `mf_reader.rs` (900 lines) stayed under the cap with no field made
  `pub(super)`. The chain walk and the Media Foundation reads are
  `mf_scaled.rs` (`read_chain`: every transform up to
  `MF_E_INVALIDINDEX`, the first decoder's output; Microsoft: a decoder
  sits at index 0).
- **The reference size.** With scaling, `visible_size`'s reference is the
  REQUESTED size (`hw_decode::visible_reference`), never the native one
  (it never fits a scaled surface).
- **The path label.** A scaled picture lies in the video processor's
  texture, which carries no `D3D11_BIND_DECODER`, so it tells nothing of
  the DECODER's path. `DecodePath::of_surface(bind_flags, scaled)` labels
  every DXGI surface of a scaled open `hardware` (the bench's
  `decode_path`: on the GPU), and `hw_decode::decoder_path_visible` keeps
  its first picture out of `gpu_decodes` AND `mf_software` (no "hardware
  decode active" INFO, no "MF decodes this file in software" WARN). So
  with the cap in force the 16.10 software-decode guard (`mf_software`)
  is blind for scaled songs: a decoder MFT falling back to software behind
  the processor (a GPU with no decoder for the codec) shows only in the
  bench's timings (a scaled 4K hardware run far slower than ~20 ms a
  picture is not NVDEC).
- **A mid-stream software reopen keeps the cap** (`max_height` on the
  reader) but judges the scale afresh on the new decoder
  (`scaled_open_safe`), so the size can change mid-song EITHER way: a
  D3D11 open that scaled, then a refused reopen, goes on at the stream's
  own size; a D3D11 open that was refused (not whole blocks, no aperture
  shown), then a software reopen that scales, goes on at the scaled
  size. The canvas fits any size. It counts in `scaled_opens` (or
  `scale_refusals`) again, and the reopen's first picture re-reads its
  own decoder. CI pins the software one: `tests/mf_decode_cap.rs`
  `a_reopen_after_a_decode_error_keeps_the_decode_cap`.
- **Which readers take the cap.** The paced producer and the decode bench
  (the cap in force, computed with the SETTING's `video_hw_decode`, not
  the bench request's `hw`: the bench measures what playback runs; set
  the setting back to automatic to bench a 4K sample at 4K; the report
  carries `max_height` and `scaled_from`). The video upgrade's facts and
  the repair's codec open with `None`: they measure the FILE (a 4K file
  read at 1080 would look 1080 to the upgrade worker, which would download
  it again).
- **Telemetry:** `status.video_decode.max_height` (the cap in force),
  `scaled_opens`, `scale_refusals` (`hw_counters()`, process-wide, any
  mode; `gpu_decodes` / `mf_software` count no scaled open).
- **Tests:** `hw_decode_tests_scale.rs` (Linux; every pin from a scratch
  model that also killed each listed mutant), `video_decode_tests.rs` (the
  cap from the setting, the JSON names), `diag/decode_bench_tests.rs` (the
  report's two facts), and on Windows `tests/mf_decode_cap.rs`:
  - the software reader on the 160×120 fixture (coded 160×128) with a cap
    of 60 hands every picture over at 80×60 (stride 80, `scaled_from`
    160×120, one scaled open);
  - the timestamps and the frame rate (as a ratio) equal the unscaled
    reader's;
  - the picture is compared region by region: a 4×4 grid of luma means
    and the four edge bands, tolerance 6. Reviewer-measured maxima over all
    90 pictures with ffmpeg stand-ins:
    - correct scalers: grid ≤ 4.95 (fast_bilinear; the others ≤ 2.9),
      bands ≤ 1.34;
    - defects: ≥ 7.0 (a 1-column pillarbox on the grid; its bands 38), a
      range conversion 10.8.

    One whole-picture mean cannot see a range conversion here, because the
    fixture's mean (111.4) sits near that conversion's fixed point (113.3);
  - a cap of 50 (66×50, pixel aspect 100:99) must fill the frame: edge
    bands, tolerance 10, correct ≤ 7.2, a 1 px letterbox ≥ 25;
  - caps of 120 and 2160 change nothing (no facts either);
  - an injected refusal (`refuse_next_scaled_open_for_test`, doc-hidden)
    plays the file unscaled with `scale_refusals` +1. The hook returns a
    guard (`ScaledOpenRefusal`, `#[must_use]`) whose drop disarms it, so
    a test that panics before its open never refuses a sibling's
    (`a_dropped_refusal_guard_refuses_no_later_scaled_open`);
  - a seek on a scaled reader (60 pictures, then back to the middle)
    lands where the unscaled reader's lands, every picture after it
    80×60, nothing reopened;
  - an injected decode error on a scaled reader: the software reopen is
    scaled too (`scaled_opens` +1), no picture lost or doubled.

  Each scaled open writes to stderr (the CI log) what MF negotiated and
  its whole `ScaledOpenFacts`, and after the first picture the decoder's
  output at the open and at the first picture. Every scaled open asserts
  the chain is the decoder at index 0 and a transform (not an `Error`)
  after it, the decoder's output read (`scaled_decoder_output()` Some),
  and each native colour value stated again on the negotiated type. The
  tests that decode from the first picture (cap 60, cap 50, the seek, the
  reopen's second open) also assert the re-read (`Some(Ok)`) with
  `first_picture_padding_in_reach` `Some(false)` (the fixture fills its
  frame, so this pins that the check reads the stream's own size). Evidence for the chain's shape: Microsoft
  (a decoder at index 0) and Wine's conformance tests, which run against
  Windows (`dlls/mfreadwrite/tests/mfplat.c`: "the video processor can be
  accessed at index 1 with MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING",
  index 2 = `MF_E_INVALIDINDEX`; with no conversion needed the decoder
  alone). Neither proves the processor's CATEGORY on Windows (Wine's own
  reader reports `MFT_CATEGORY_VIDEO_PROCESSOR`), so that is pinned in its
  own test, `a_scaled_open_lists_the_video_processor_in_its_chain`, whose
  first CI run (#282 follow-up) is the proof; if it fails, the stderr
  line before it names the chain.
- **Box checks (CI cannot run the D3D path):**
  - PATCH `max_resolution` to 1080 and wait 5 s. Then run the decode
    bench with `"hw": true` on a 4K sample, for EACH 4K codec (VP9, AV1,
    H.264), over the whole file (`ended: end_of_stream`), and again with
    the setting back to automatic. Expect:
    - `width` / `height` = the scaled size, with `scaled_from` and
      `max_height` set;
    - `frames` and `source_fps` equal to the uncapped run's. Unequal
      means frame-rate conversion: a repeated picture;
    - a mean well under the uncapped run's;
    - the log's `opened scaled to the decode cap` line says `path=D3D11`
      and names the `decoder` output, the `chain` (a `VideoProcessor`
      after the `Decoder`) and the colours (two stated, unequal values of
      `colour_out` and `colour_decoder` suggest a conversion: check the
      hues); the `at the first scaled picture` line says
      `at_first_picture=Ok(…)` and `padding_in_reach=false` (a failed
      re-read also prints `false`).
  - Play a 4K song: `status.video_decode.scaled_opens` +1 and
    `scale_refusals` +0. A refusal's WARN names the negotiated size or the
    decoder's padding. `GET /api/v1/program` `max.upload_mb_s` should drop
    from ~298 to about a quarter.
  - A widescreen 4K song whose rows are not a multiple of 16 (3840×1606:
    NVDEC pads to 1616): it is scaled only if the decoder showed the native
    aperture at open (the INFO's `decoder`), else refused (WARN,
    `scale_refusals`) and played at 4K as before. When it plays scaled,
    always look at MAX's / the wall's bottom rows for a green line (the
    check runs at open, the decoder's type can change at the first
    frame; the `at the first scaled picture` INFO's
    `padding_in_reach=true` is that case, by the model).

    The exact fix if such songs must scale too (review round 1): ONE
    reader with `IMFSourceReaderEx::AddTransformForStream`, inserting a
    `CLSID_VideoProcessorMFT` whose `IMFVideoProcessorControl::
    SetSourceRectangle` is the native picture, so the geometry is ours.
  - A 2560×1350 or 3840×1606 song at 1080 (a fraction of a pixel off the
    aspect): check for a black column or row at MAX's / the wall's edges.
    Check the colours too: no hue shift against the uncapped song.

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
  `Hardware` open, the bench's too): `requested` (counted before the open,
  so a file that then opens nowhere counts only there), `gpu_decodes`,
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
- **Never switch SNV (or PP) to software decode while it plays to a room**
  (#251 comment 6221372473, 16.10.2026, during an event). With the setting
  off, every song from 17:01 to 17:43Z opened in software. The Dante
  Virtual Soundcard's ASIO callbacks then came late or early, 12–40 a
  minute against 4–8 (the out-3 risks of the audio fault ledger). Dante
  Controller showed SNV's packets red, and the audio popped at FOH. It
  stopped with the first GPU-decoded song. The total CPU stayed at 1–2 %:
  MF's software decoder is BURSTY, not heavy on average, so a CPU reading
  never clears it. Software decode did not cure the wall judder either.
  The setting stays `true` on SNV.
- `GET /api/v1/status` → `video_decode {hw_decode, hw_requested,
  gpu_decodes, mf_software, open_fallbacks, mid_stream_fallbacks,
  path_changes, last_fallback, max_height, scaled_opens, scale_refusals}`
  (the setting + the decode cap (#282) + the counters; counts of FILES since the
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

## A box without the VP9 / AV1 Video Extensions opens no YouTube file (#229, 6.10.2026)

Media Foundation decodes VP9 and AV1 only through the Store's VP9 / AV1
Video Extensions. A Windows 11 IoT LTSC box (resolume-pp) has neither. On it,
every cached video fails to open in both modes: the hardware open falls back,
then software fails with "No video: SetCurrentMediaType failed: No suitable
transform". Before blaming the reader, check `Get-AppxPackage -AllUsers` for
`Microsoft.VP9VideoExtensions` / `Microsoft.AV1VideoExtension`. SNV runs VP9
1.2.20.0 and AV1 2.0.35.0. `win-resolume-ops` "PP site" has the winget
install.

