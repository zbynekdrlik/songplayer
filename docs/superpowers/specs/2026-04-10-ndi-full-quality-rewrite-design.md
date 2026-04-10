# NDI Output Full-Quality Rewrite — Design

**Status:** proposed
**Date:** 2026-04-10
**Author:** zbynek (with Claude)
**Branch:** `dev`

## Problem

The NDI stream published by SongPlayer currently has **no audio at all** in OBS, and the video pacing relies on manual `std::thread::sleep` which drifts under load. Root-cause investigation against the canonical NDI SDK headers (`Processing.NDI.structs.h`, `Processing.NDI.Send.h`) and the DistroAV reference implementation identified five distinct defects, all in `sp-ndi`, `sp-decoder`, and `sp-server/playback/pipeline`:

1. **Wrong audio FourCC.** `FourCCAudioType::FltInterleaved = 0x0000_0001` is not a value the NDI SDK defines. The only documented audio FourCC is `NDIlib_FourCC_audio_type_FLTP = NDI_LIB_FOURCC('F','L','T','p')` (`0x70544C46`). NDI silently drops frames whose FourCC it doesn't recognise.
2. **Wrong audio memory layout.** FLTP requires **planar** float buffers (`[L0..L_{n-1}][R0..R_{n-1}]…`). Windows Media Foundation delivers **interleaved** (`[L0,R0,L1,R1…]`). The current code hands MF's interleaved bytes straight to NDI.
3. **Wrong `channel_stride_in_bytes`.** For FLTP this field must be `no_samples * sizeof(float)` (the byte distance between the start of each channel's buffer). The code sets it to `0`.
4. **No NDI clocking + wrong declared frame rate.** Senders are created with `clock_video: false, clock_audio: false` and the pipeline thread paces with `std::thread::sleep`. Windows sleep granularity is ~15 ms; the thread also does per-pixel NV12→BGRA conversion between frames; drift is inevitable. The declared frame rate is hardcoded to 30000/1001 regardless of the actual source.
5. **Unnecessary NV12→BGRA CPU conversion.** `sp-decoder` decodes into NV12 (MF's native output on hardware decoders) and then runs a scalar per-pixel converter to BGRA before handing frames to NDI. NDI accepts NV12 directly via `NDIlib_FourCC_video_type_NV12`. The conversion is pure waste — at 1080p60 it costs ~124 Mpixel/s of a single core — and is a major drift source.

All five are related: fixing audio exposes sync drift, fixing clocking requires the real frame rate, and removing the NV12 conversion is what actually makes the clocking loop fast enough not to drift. Any partial fix would leave a visibly or audibly broken output. This spec addresses all five in one coherent change.

## Goals

- Audio is audible in OBS via the NDI source, at the correct sample rate and channel count, with no resampling or format conversion in SongPlayer.
- Video frames are paced by NDI's internal clock (high-resolution, wall-clock accurate) rather than by `std::thread::sleep`.
- Audio and video are kept in sync across long playback (full 3-5 minute songs) with no perceptible drift.
- Decode and network send overlap: while NDI is sending frame N, the decoder is already working on frame N+1.
- The sender exposes NV12 natively end-to-end, so no CPU is spent on format conversion.
- Every new code path has unit tests; mutation testing on all touched files shows zero surviving mutants before merge.
- Post-deploy verification on `win-resolume` confirms audio is actually audible and A/V are in sync.

## Non-goals

- **Not** changing the dashboard, REST API, or WebSocket protocol. This is purely a media-pipeline fix.
- **Not** adding hardware-accelerated color conversion. We remove the conversion entirely instead.
- **Not** adding audio resampling. NDI accepts whatever sample rate MF reports; we pass it through.
- **Not** supporting other NDI sender flags (metadata, failover, groups, etc.). Only `clock_video`/`clock_audio` are wired.

## Reference sources

- NDI SDK v6 headers shipped with DistroAV:
  - `Processing.NDI.structs.h` (types, FourCC macros, audio/video frame structs)
  - `Processing.NDI.Send.h` (sender API, `clock_video`/`clock_audio` semantics, `send_send_video_async_v2`)
- DistroAV reference implementation:
  - `src/ndi-output.cpp:346-417` — the exact pattern for delivering raw video + FLTP audio from a source that already has channel-planar data
  - `src/ndi-output.cpp:378-417` — audio frame construction, `channel_stride_in_bytes = frame->frames * 4`, `FourCC = NDIlib_FourCC_audio_type_FLTP`
  - `src/ndi-output.cpp:207-209` — NV12 passthrough case

These are the authoritative references the design tracks against.

## Architecture

The change is layered:

```
sp-decoder (MediaReader)
    ├─ reads NV12 raw bytes + interleaved f32 PCM from MF
    ├─ exposes video_info() with real width/height/frame_rate/pixel_format
    ├─ no color conversion
    ▼
sp-decoder (SyncedDecoder)
    ├─ same pairing of video frame ↔ audio chunks
    ├─ forwards video_info() from reader
    ▼
sp-server::playback::pipeline
    ├─ creates NdiSender with clock_video=true, clock_audio=false
    ├─ reads video_info from decoder once per file
    ├─ for each synced tuple (video_frame, audio_chunks):
    │     - for each audio chunk: sender.send_audio(chunk)   (fast, non-blocking)
    │     - sender.send_video_async(video_frame)             (blocks on NDI clock)
    │     - hold onto video_frame until next send_video_async
    ├─ on any transition (Pause/Stop/Ended/NewPlay/Shutdown/Error):
    │     sender.send_video_flush()                           (releases last buffer)
    │     drop prev_frame
    ▼
sp-ndi::NdiSender
    ├─ send_video_async → NDIlib_send_send_video_async_v2
    ├─ send_video       → NDIlib_send_send_video_v2          (used only for idle black frames)
    ├─ send_audio       → NDIlib_send_send_audio_v3 with FLTP FourCC, planar buffer
    ├─ send_video_flush → NDIlib_send_send_video_async_v2(NULL)
    ▼
sp-ndi::RealNdiBackend
    ├─ resolves NDIlib_send_send_video_async_v2
    ├─ owns a per-sender interleave→planar scratch buffer for audio
    └─ owns no video scratch (we pass NV12 bytes straight through)
```

### FourCC values

All FourCCs on little-endian x86-64 have bytes laid out in memory as `[ch0, ch1, ch2, ch3]`:

| Name | Chars | `u32` value |
|------|-------|-------------|
| `FourCCAudioType::FLTP` | `F L T p` | `0x70544C46` |
| `FourCCVideoType::NV12` | `N V 1 2` | `0x3231564E` |
| `FourCCVideoType::BGRA` (unchanged) | `B G R A` | `0x41524742` |

Tests in `sp-ndi/src/lib.rs` will read each enum value back into a byte array and assert the ASCII bytes match the name exactly. This prevents endian/byte-order regressions and guarantees we match what the C macro would have produced.

### Interleaved → planar conversion (audio)

MF delivers stereo as `[L0, R0, L1, R1, …, L_{n-1}, R_{n-1}]`. NDI expects `[L0, L1, …, L_{n-1}, R0, R1, …, R_{n-1}]`. The conversion is a pure function:

```rust
fn deinterleave(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    let samples_per_channel = interleaved.len() / channels;
    out.clear();
    out.resize(interleaved.len(), 0.0);
    for ch in 0..channels {
        for s in 0..samples_per_channel {
            out[ch * samples_per_channel + s] = interleaved[s * channels + ch];
        }
    }
}
```

`RealNdiBackend` owns a per-sender `Mutex<Vec<f32>>` scratch buffer that `send_audio` reuses, so there is no per-frame allocation in steady state. The scratch buffer lifetime is tied to the sender (drop of sender → drop of scratch).

The NDI frame is then:

```rust
NDIlib_audio_frame_v3_t {
    sample_rate: 48000,           // whatever MF reported
    no_channels: 2,
    no_samples: samples_per_channel,
    four_cc: FourCCAudioType::FLTP,
    p_data: planar.as_ptr() as *const u8,
    channel_stride_in_bytes: (samples_per_channel * 4) as i32,
    timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
    ...
}
```

### NV12 passthrough (video)

`MediaReader` already negotiates `MFVideoFormat_NV12` with the hardware decoder. We stop calling `nv12_to_bgra` and return the raw NV12 bytes. The buffer size is `width * height * 3 / 2` (Y plane + half-height interleaved UV plane). The Y-plane stride is `width` when MF doesn't add padding (which is the typical case for software H.264/AV1/VP9 decoders; hardware paths can add 16/32/64-byte alignment). We read the actual stride from the MF sample's `lStride` attribute if available, otherwise assume `width`.

For the NDI video frame:

```rust
NDIlib_video_frame_v2_t {
    xres: width,
    yres: height,
    four_cc: FourCCVideoType::NV12,
    frame_rate_n,                 // real, from MF
    frame_rate_d,
    picture_aspect_ratio: 0.0,    // square pixels
    frame_format_type: FRAME_FORMAT_PROGRESSIVE,
    timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
    p_data: nv12.as_ptr(),
    line_stride_in_bytes: y_stride,
    ...
}
```

### Clocking strategy

Create the sender with:

```rust
NDIlib_send_create_t {
    p_ndi_name: c_name.as_ptr(),
    p_groups: ptr::null(),
    clock_video: true,    // NDI paces send_video_async on the wall clock
    clock_audio: false,   // single-thread submission; pacing both would deadlock
}
```

This matches the SDK guidance in `Processing.NDI.Send.h:44-49`:

> In general if you are submitting video and audio off a single thread then you should only clock one of them (video is probably the better of the two to clock off).

The pipeline then submits per synced tuple in this strict order:

1. For each audio chunk: `sender.send_audio(chunk)` — fast, never blocks, goes straight into NDI's internal audio queue.
2. `sender.send_video_async(&video_frame)` — blocks until the wall clock reaches this frame's natural presentation time, then returns as soon as NDI has taken ownership of the buffer.

Because audio is queued before video blocks, there is no chance audio starves waiting for clocking, and NDI has enough buffered audio to keep the receiver's output smooth across clocking waits.

The existing `playback_start`/`pause_offset` tracking and manual `thread::sleep` are deleted. The only remaining pausing logic is: when `PipelineCommand::Pause` arrives mid-decode, stop submitting frames and instead send black frames at ~10fps via the sync `send_video` path (which does not honour clocking, so pause does not deadlock waiting on a clock that already passed).

### Double-buffer lifetime for async send

`NDIlib_send_send_video_async_v2` returns immediately after scheduling. Per the SDK header:

> The memory accessed by NDIlib_video_frame_t cannot be freed or re-used by the caller until a synchronizing event has occurred. Synchronizing events are: a call to NDIlib_send_send_video, a call to NDIlib_send_send_video_async with another frame to be sent, a call to NDIlib_send_send_video with p_video_data=NULL, a call to NDIlib_send_destroy.

Pattern:

```rust
let mut prev_frame: Option<DecodedVideoFrame> = None;
loop {
    let current = decoder.next_synced()?;
    for af in &current.audio {
        sender.send_audio(af);
    }
    sender.send_video_async(&current.video);
    // The async call is itself the sync point that releases prev_frame;
    // we can now safely drop it on the next iteration.
    prev_frame = Some(current.video);
}
// On any exit path:
sender.send_video_flush();    // NDIlib_send_send_video_async_v2(NULL)
drop(prev_frame);              // now safe — NDI has released our pointer
```

`send_video_flush` is called from every exit branch: `Ended`, `Stopped`, `Shutdown`, `NewPlay`, `Error`, and from `NdiSender::drop` as a belt-and-braces guarantee before `send_destroy`.

### Frame rate discovery

`MediaReader::open` reads the negotiated video media type and extracts `MF_MT_FRAME_RATE`, a `UINT64` attribute packed as `(numerator << 32) | denominator`. Exposed as:

```rust
pub struct VideoStreamInfo {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,    // Nv12
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
}

impl MediaReader {
    pub fn video_info(&self) -> VideoStreamInfo { ... }
}
```

Fallback: if `MF_MT_FRAME_RATE` is not present (rare; only on malformed containers), fall back to `30000/1001` and log a warning. In practice every test file we care about declares a frame rate.

### Public API impact (within the workspace)

- `sp-ndi::VideoFrame` gains a `pixel_format: PixelFormat` field (`Bgra` or `Nv12`).
- `sp-ndi::NdiSender` gains `send_video_async`, `send_video_flush`, and `new_with_clocking(backend, name, clock_video, clock_audio)`. The existing `new` keeps today's semantics (no clocking, no async) so existing sp-ndi tests continue to exercise the basic path without the async complications.
- `sp-ndi::NdiBackend` trait gains `send_create_with_clocking`, `send_video_async`, and `send_video_flush`. The existing mock backend is extended to record these new calls; the existing `send_create`/`send_video` are kept for backwards-compatible paths.
- `sp-decoder::DecodedVideoFrame` gains `pixel_format: PixelFormat`.
- `sp-decoder::SyncedDecoder` gains `video_info(&self) -> VideoStreamInfo`.
- `sp-server::playback::pipeline` is the only consumer of the new clocking API; all other callers (tests, stubs) stay on the non-clocking path.

No external API (HTTP, WebSocket, dashboard) changes.

## Data flow (steady state, one file)

1. `pipeline::run_loop_windows` receives `PipelineCommand::Play(path)`.
2. It creates a fresh `SyncedDecoder` for the file.
3. It calls `decoder.video_info()` to learn `(width, height, frame_rate_n, frame_rate_d, pixel_format)`.
4. It loops calling `decoder.next_synced()` which returns `(video: DecodedVideoFrame, audio: Vec<DecodedAudioFrame>)`.
5. For each audio chunk, `sender.send_audio(&sp_ndi::AudioFrame { data: chunk.data, channels, sample_rate })`. RealNdiBackend deinterleaves into its scratch buffer and fires `NDIlib_send_send_audio_v3`.
6. `sender.send_video_async(&sp_ndi::VideoFrame { data: video.data, width, height, stride, frame_rate_n, frame_rate_d, pixel_format: Nv12 })`. RealNdiBackend fires `NDIlib_send_send_video_async_v2`. With `clock_video=true`, this blocks internally until the wall clock reaches the frame's natural time (based on NDI's synthesized timecode counter).
7. The previous iteration's video buffer is dropped only after step 6 returns (the async call is itself the synchronizing event).
8. Decoder decodes the next frame in parallel with NDI's internal send thread.
9. On any loop exit (`Ended`, `Stopped`, `Shutdown`, `NewPlay`, `Error`): `sender.send_video_flush()` releases the last buffer, the pipeline sends a black frame to keep the source visible, and the outer state machine handles the transition.

## Tests

### Unit (sp-ndi)

- `fltp_fourcc_bytes_spell_FLTp` — `(FourCCAudioType::FLTP as u32).to_le_bytes() == [b'F', b'L', b'T', b'p']`
- `nv12_fourcc_bytes_spell_NV12` — same pattern
- `bgra_fourcc_bytes_spell_BGRA` — guards the existing value against regression
- `deinterleave_mono_is_passthrough` — 1 channel input unchanged
- `deinterleave_stereo_four_samples` — `[1,2,3,4,5,6,7,8]` (4 samples × 2ch) → `[1,3,5,7,2,4,6,8]`
- `deinterleave_six_channel_preserves_sample_count` — 5.1 layout, asserts both output length and sample order
- `deinterleave_preserves_exact_float_bits` — use `f32::from_bits` values so any accidental math is caught
- `deinterleave_reuses_scratch_buffer` — call twice with different sample counts, assert no realloc on the second call if capacity is sufficient
- `send_audio_passes_fltp_fourcc_to_backend` — mock backend records the FourCC
- `send_audio_passes_correct_channel_stride` — mock backend records `channel_stride_in_bytes == samples_per_channel * 4`
- `send_audio_passes_planar_layout` — mock backend records the raw bytes, assert they match the deinterleaved expectation
- `send_create_with_clocking_records_flags` — mock records the `clock_video`/`clock_audio` passed
- `send_video_async_calls_correct_symbol` — mock distinguishes sync vs async paths
- `send_video_async_with_nv12_passes_nv12_fourcc` — mock records the FourCC
- `send_video_async_with_bgra_passes_bgra_fourcc` — mock records the FourCC
- `send_video_flush_calls_async_with_null` — mock records the null flush call
- `sender_drop_calls_flush_before_destroy` — mock records the order of calls at drop time
- `interleave_to_planar_scratch_buffer_is_per_sender` — two senders with different channel counts don't clobber each other's scratch

### Unit (sp-decoder, non-Windows)

- `video_stream_info_serde_and_defaults` — pure struct test for `VideoStreamInfo`
- `pixel_format_enum_values` — exhaustive match coverage

### Integration (sp-decoder, Windows CI)

- `open_reads_frame_rate_from_test_mp4` — a checked-in ~100KB 30fps NV12 H.264 test clip; assert `video_info().frame_rate_num == 30` and `frame_rate_den == 1`
- `next_video_frame_returns_nv12_data_of_expected_size` — `data.len() == width * height * 3 / 2`
- `audio_channels_and_sample_rate_match_source` — 48kHz stereo test clip, assert 2/48000

### Unit (sp-server/playback/pipeline, cross-platform)

- Add an `NdiBackend` mock-based test entry point that drives the pipeline's submission logic without requiring Windows / a real decoder. Verifies:
  - `send_create_with_clocking(_, true, false)` is the only create call
  - For each synced tuple in a fixture, audio calls precede the video call (ordering preserved)
  - `send_video_flush` is called exactly once on each of `Stopped`, `Ended`, `NewPlay`, `Shutdown`, `Error` before any next `send_video_async`
  - The declared `frame_rate_n`/`frame_rate_d` in the video frame matches `video_info()` from the decoder mock
  - On `Pause`, no `send_video_async` calls occur; only the sync `send_video` black-frame path is exercised
- Regression test for the existing `pipeline_processes_multiple_sequential_plays` lives on unchanged.

### Mutation testing

Re-enable `cargo-mutants` coverage on all files touched:

- `crates/sp-ndi/src/types.rs`
- `crates/sp-ndi/src/sender.rs`
- `crates/sp-ndi/src/ndi_sdk.rs`
- `crates/sp-decoder/src/reader.rs` (Windows-only, still gated in CI)
- `crates/sp-decoder/src/types.rs`
- `crates/sp-decoder/src/sync.rs`
- `crates/sp-server/src/playback/pipeline.rs`

Allow exclusions with `#[cfg_attr(test, mutants::skip)]` only where the mutant would require a 5-minute real-time wait to observe (same pattern as PR #6). Target: **zero surviving mutants** before merge.

## Verification plan

### CI (automated)

1. `cargo fmt --all --check`
2. All sp-* unit and integration tests pass on Linux and Windows runners
3. Coverage jobs within existing thresholds
4. Mutation testing shows 0 surviving mutants on the touched files
5. Frontend E2E (Playwright) passes against the mock API — this proves the dashboard still builds and talks to the server correctly
6. Build-Tauri-Windows produces an installer
7. Deploy-to-win-resolume succeeds
8. E2E (win-resolume) tests pass against the real deployed binary

### Post-deploy on `win-resolume` (manual + MCP-driven)

1. Via `mcp__win-resolume__Shell`: confirm the service is running and the new binary version is `0.8.0-dev.2` (or whatever the dev version is at merge time)
2. Via the dashboard (Playwright), trigger playback on `ytfast`
3. Dashboard shows now-playing song + elapsed time advancing — proves the decode loop runs
4. Via `mcp__obs-resolume__obs-get-scene-item-list` on the `sp-fast` scene: NDI source item is present and enabled
5. Via `mcp__obs-resolume__obs-get-source-active`: the NDI source reports as active on program
6. Via `mcp__obs-resolume__obs-get-source-screenshot` of the NDI source: assert the image is not all-black and not all-one-color (i.e. actual decoded video pixels)
7. Via `mcp__win-resolume__Snapshot` capturing the OBS main window: inspect the audio mixer for the NDI source — dB meter must be moving (proves audio is reaching OBS, not just a stale channel)
8. **Manual audio listen check** — final subjective A/V sync confirmation on the remote machine. Document the exact clip used and the observed behaviour in the completion report.

A/V sync is a subjective check because OBS WebSocket does not expose decoded frame/sample timestamps. The screenshot + mixer-meter check is the best we can do to make it automated; the final ear/eye test is explicit and documented.

## Risks and mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Async send lifetime bug → use-after-free in NDI SDK | Crash | Strict `prev_frame` holdover + explicit `send_video_flush` on every exit branch + a unit test that a mock records flush-before-destroy |
| NV12 stride mismatch on hardware decoders with padding | Green-stripe garbage in video | Read `lStride` from the MF sample; fall back to `width` only when it's unavailable; integration test against a real decoded frame on Windows CI |
| Frame rate missing from malformed container | Wrong pacing, drift | Fallback to `30000/1001` with a WARN log; manual verification step watches for this in the deploy log |
| `clock_video=true` deadlock on paused state | Pipeline hangs | Pause path uses the sync `send_video` black-frame call, which does not honour clocking |
| Scratch buffer growth on channel-count change | Unexpected allocation | Per-sender scratch, documented invariant "sample_rate and channel count are fixed per sender instance"; if MF ever changed mid-file we'd create a new sender |
| Mutation testing finding mutants we can't cheaply test | Red CI | Follow the same `#[cfg_attr(test, mutants::skip)]` pattern as PR #6, with a comment explaining why each exclusion is justified |

## Version and branching

- Base: current `dev` at `0.8.0-dev.1`
- Bump to `0.8.0-dev.2` as the first commit of the implementation plan
- One PR from `dev` to `main` when complete and CI is green
- No separate release — the merge to `main` auto-deploys to `win-resolume` via the existing pipeline

## Open questions

None. The canonical NDI headers + the DistroAV reference implementation give us exact answers for every design decision in this document.
