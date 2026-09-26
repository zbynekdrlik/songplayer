---
paths:
  - "crates/sp-ndi/src/receive*.rs"
  - "crates/sp-ndi/src/receiver*.rs"
  - "crates/sp-server/src/playback/ndi_input*.rs"
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/program_control.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "e2e/settings-ndi-input.spec.ts"
  - "e2e/program-control.spec.ts"
---

# NDI input "OBS manuál" (#212, B3 of EPIC #174)

SongPlayer is the master switcher. cg OBS stays only for the manual scenes
(media, browser, Slido, photos, ALEX NB). Its manual mix comes in over NDI as
ONE program source, id `PROGRAM_INPUT_ID = -1` (`sp_core::config`), with the
label "OBS manuál". Design record: #212 comment 5847592877 (Approach 1).

## SDK receive half (`sp-ndi/src/receive.rs`, `receiver.rs`)

- The layouts and signatures were copied from the official headers
  (`Processing.NDI.{Recv,FrameSync,Find,structs}.h`). The DistroAV repo vendors
  them under `lib/ndi/`. Things that are easy to get wrong:
  - `NDIlib_framesync_capture_audio` / `free_audio` take
    **`NDIlib_audio_frame_v2_t`**: planar float with `channel_stride_in_bytes`
    and NO FourCC field. It is not v3; the `_v2` suffixed functions are the v3
    ones.
  - A received video frame's FourCC is read as a plain `u32`
    (`NDIlib_video_frame_v2_recv_t`). The send-side Rust enum would be UB on an
    unknown value.
  - `NDIlib_recv_create_v3_t` field order: `source_to_connect_to`,
    `color_format`, `bandwidth`, `allow_video_fields`, `p_ndi_recv_name`.
  - The `offset_of` tests pin every 64-bit offset. Never edit a struct without
    them.
- The receive symbols are resolved OPTIONALLY (`NdiLib::recv: Option<RecvFns>`).
  A runtime without them keeps every sender working; there is just no input.
  `RealNdiReceiveBackend::new(lib)` shares the ONE `NdiLib`
  (`RealNdiBackend::lib()`), because `NDIlib_initialize` must run once per
  process.
- The color format is `NDIlib_recv_color_format_fastest` with
  `allow_video_fields = false` and highest bandwidth. A source without alpha
  comes as UYVY. One with alpha comes as UYVA, whose FIRST plane is UYVY, so a
  single converter covers both. Any other FourCC, an odd size, a stride under
  2 × width, or a non-progressive frame (a field — the SDK should de-interlace
  with `allow_video_fields = false`, but `fastest` may still deliver fields)
  gets the standby pair plus a logged
  `ndi input: video format … progressive=… supported=false`. It is counted in
  `unsupported_boundaries`, never in `frames_received`.
- `NdiFrameSync` (RAII):
  - It destroys the FrameSync BEFORE its receiver, the SDK's order.
  - `capture_video` / `capture_audio` return guards that free the frame on
    drop and borrow the pair, so a frame can never outlive it.
  - The all-zero frame (no video yet) is freed too.
- `MockNdiReceiveBackend` (`sp_ndi::test_util`) is scripted with frames plus a
  per-capture SCHEDULE:
  - the same index twice is a FrameSync repeat, with the same pointer and
    timecode;
  - a skipped index is a drop;
  - `None` is the all-zero frame.

  It records create, destroy, capture and free, and exposes the outstanding
  captures. Every recorder it exposes is asserted in `receiver_tests.rs`
  (per-package mutation gate).

## The grid thread (`playback/ndi_input.rs`)

- `run_input_loop` (Windows thread `ndi-input`, `THREAD_PRIORITY_TIME_CRITICAL`
  via `pipeline_audio::raise_thread_priority` + the 1 ms timer: while cut it
  owns every program boundary) ticks on the program wall domain: a
  `WallVbanClock`, ticked once per boundary. `grid_step` (→ `InputGridStep`)
  applies the pacer's rule, with the resync decided by
  `sp_core::genlock::lag_over_catchup_bound_100ns` (it STEPS the grid; never
  divide by the nominal interval):
  - wait for the next boundary;
  - catch up one by one while ≤ 8 behind;
  - above 8 behind, resync on the floor;
  - a boundary more than 2 slots ahead is a backward clock step, so re-latch.
- `NdiInput::service(B, audio_now, bus)`:
  1. apply a settings change (reconnect), or retry a failed receiver after
     exactly 5 s;
  2. `touch(-1, B)`;
  3. capture the video plus `capture_audio(48000, 2, 1600)`, EVERY boundary,
     so the FrameSync keeps tracking our cadence;
  4. only while a candidate, convert UYVY→NV12 into a `frame_pool` buffer
     (recycled on the last `SharedFrame` drop) and `offer` a `SubmitJob`
     stamped at B, with the audio stamped at the emit (§6).
- A FrameSync repeat has the same timecode AND the same data pointer. It
  re-offers the SAME converted `SharedFrame` (an `Arc` bump, no second
  conversion). A distinct buffer with the same timecode is a new frame.
- Drops are the rounded source-frame distance minus one (`skipped_frames`,
  from the source rate). A disconnect forgets the last frame, so an outage is
  never counted as drops after the reconnect.
- The standby pair (the input's own NV12 black + one silent block) goes out
  when there is no SDK, no receiver, the source is disconnected
  (`recv_get_no_connections == 0`), there is no video yet, or the format is
  unsupported. So the input ALWAYS owns its boundary on time, and the bus
  never fills for it.
- A disabled input, or an enabled one with an empty source, does nothing: no
  touch, no receiver. It is not a program source then.
- The thread is started from `start_program` via `ndi_input::start_ndi_input`.
  `lib.rs` and `pipeline.rs` are untouched (cap). On shutdown,
  `bus.input().stop()` ends the loop, which closes the receiver.

## Settings, API, UI

- Keys:
  - `ndi_input_enabled`: only `"true"` enables.
  - `ndi_input_source`: the full `"MACHINE (stream)"` name, trimmed.
- `run_input_config_task` re-reads the keys every 5 s. While the input is
  enabled and not connected, it lists the visible NDI sources every 30 s on
  the blocking pool (a 1 s `find_wait`). The list is logged and served as
  `input.visible_sources`, at most 16 names.
- API:
  - `GET /api/v1/program` and the cut answer carry `input`:
    `{id, label, enabled, running, connected, source, stream, frames_received,
    video_repeats, video_drops, no_source_boundaries, unsupported_boundaries,
    boundaries, resyncs, relatches, audio_queue_depth, last_frame_size, format,
    frame_rate, visible_sources}`.
  - `enabled` and `source` come from the STORED settings, so a save shows at
    once.
  - `stream` = `obs::ndi_discovery::extract_ndi_stream_name(source)`.
  - `POST /api/v1/program/cut {"source": -1}` → 404 while disabled.
  - A persisted `program_source = -1` is restored only while the input is
    enabled.
- sp-ui:
  - `ProgramControl` lists an extra `program-cut` button with
    `data-playlist-id="-1"` and the text "OBS manuál" AFTER the playlists, only
    while `input.enabled`.
  - Nastavenia has the fieldset `settings-ndi-input`, with
    `settings-ndi-input-enabled` and `settings-ndi-input-source` (placeholder
    `CG-OBS (manual)`).
- The mock's `GET /api/v1/program` derives `input` from the stored settings,
  and it refuses a cut to `-1` with 404 while the input is disabled. The specs
  reset the settings (`/__mock/settings-reset`) and the program
  (`/__mock/program-reset`).

## Tests

- `ndi_input_tests.rs` runs the input over the mock into a REAL `ProgramBus`
  with `-1` on program:
  - frames are 4×2 UYVY and the standby black is 2×4, so a job's width names
    its owner;
  - `drain()` panics on a program fill, because the input must own every
    boundary.
- The loop tests run `run_input_loop` on its own thread with a virtual
  `FakeClock`: a sleep advances it, it never goes past `limit`, and it can do
  one jump. They wait with bounded polls and `recv_timeout`, so a no-op stop
  fails in ≤ 10 s instead of hanging.
- The converted buffer's size class is pinned through `frame_pool::pool_len`
  on a unique 50×34 class (2550 bytes). A wrongly sized `take` would be an
  equivalent mutant otherwise.

## Box acceptance (the supervisor's job)

Set `ndi_input_source` to a live NDI source on the LAN (the cg OBS program NDI
output) and cut to "OBS manuál" and back in a real browser. Check that:

- the program stamps are contiguous;
- there are 0 fills;
- `input.frames_received` advances;
- `input.connected` is true.

Never switch the OBS program scene.
