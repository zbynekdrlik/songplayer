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
- **`RealNdiReceiveBackend` locks PER receiver and PER FrameSync.** This uses
  `handle_table::HandleTable`, the same pattern #147 round 11 introduced for
  the senders. It is NEVER one lock across every SDK call.
  - Each call holds only its own instance's slot lock. So a destroy still
    waits for a capture in flight on the SAME instance.
  - The input's close helper destroys the old pair (~0.5 s on the box) while
    the grid thread captures from the new one. With one global lock, the
    grid's first `recv_connections` on the new pair waited for that destroy.
    That was the very stall the lifecycle rule below removes (#212 follow-up,
    review round 2).
  - `NdiReceiveBackend`'s doc states the contract: calls on DIFFERENT handles
    never wait for each other.
  - The SDK calls live in `RecvHandles`. `RealNdiReceiveBackend` only adds the
    `NdiLib` (which keeps the library loaded and serves `find`) and delegates.
  - The lock scope is pinned over FAKE SDK functions
    (`receive.rs` `calls_on_one_instance_never_wait_for_a_slow_call_on_another`),
    with a gate that holds one instance's destroy or capture. The other pair's
    calls must return, and a FrameSync destroy must wait for a capture in
    flight on the same FrameSync.
  - The per-call pass-throughs stay `mutants::skip`: the real functions need
    the runtime.
- The color format is `NDIlib_recv_color_format_fastest` with
  `allow_video_fields = false` and highest bandwidth. A source without alpha
  comes as UYVY. One with alpha comes as UYVA, whose FIRST plane is UYVY, so a
  single converter covers both. A progressive or an interleaved WHOLE frame
  (`frame_format_type` 1 / 0, both full height) is converted. The standby
  pair goes out instead, logged as
  `ndi input: video format … full_frame=… supported=false`, for:
  - any other FourCC;
  - an odd size;
  - a stride under 2 × width;
  - a single field (type 2 / 3). The SDK should de-interlace with
    `allow_video_fields = false`, but `fastest` may still deliver fields.

  Such a boundary is counted in `unsupported_boundaries`, never in
  `frames_received`.
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
  via `mmcss::raise_thread_priority` + the 1 ms timer: while cut it
  owns every program boundary) ticks on the program wall domain: a
  `WallVbanClock`, ticked once per boundary. `grid_step` (→ `InputGridStep`)
  applies the pacer's rule, with the resync decided by
  `sp_core::genlock::lag_over_catchup_bound_100ns` (it STEPS the grid; never
  divide by the nominal interval):
  - wait for the next boundary;
  - catch up one by one while ≤ 8 behind;
  - above 8 behind, resync on the floor;
  - a boundary more than 2 slots ahead is a backward clock step, so re-latch.
  - At a fleet date step its wall relabels (#224 part 2, `genlock.md`), so
    its timeline moves by the remainder r only: the next boundary comes at
    most ONE early, never a catch-up of the step's slots
    (`ndi_input_tests_regrid.rs`). Its `WallVbanClock::new` follows the
    wall; only VBAN's clock slews r. That one early boundary asks FrameSync
    for its block up to r (≤ 33 ms) sooner: check its audio queue at box
    acceptance (the input's `input` telemetry across a controlled step: no
    short or silent block counted).
- `NdiInput::service(B, bus)` (the pair's audio AND video stamped on `B`,
  #224 — never the emit instant, also in a catch-up):
  1. apply a settings change, take a finished connect, or request a
     (re)connect / the retry of a failed one exactly 5 s after the boundary
     that requested it. NO SDK call happens here (see "Receiver lifecycle off
     the grid thread" below);
  2. `touch(-1, B)`;
  3. capture the video plus `capture_audio(48000, 2, 1600)` on every
     CONNECTED boundary, so the FrameSync keeps tracking our cadence. Nothing
     is captured without a receiver or while the source is disconnected;
  4. only while a candidate, convert UYVY→NV12 into a `frame_pool` buffer
     (recycled on the last `SharedFrame` drop) and `offer` a `SubmitJob`
     stamped at B, with the audio stamped at the emit (§6).
- An interleaved whole frame is converted like a progressive one. Its row-pair
  chroma mean mixes the two fields' rows, which gives slight chroma combing on
  moving interleaved content. That is acceptable: the receiver asks for
  progressive frames, so it should be rare.
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
- An INACTIVE input (disabled, or enabled with an empty source) receives
  nothing: no receiver, no capture, no counters. It still `touch`es. If it is
  still selected on program (cut while active, then switched off), it keeps
  offering its standby pair, so the program never fills for it. Otherwise it
  offers nothing.
- The thread is started from `start_program` via `ndi_input::start_ndi_input`.
  `lib.rs` and `pipeline.rs` are untouched (cap). On shutdown,
  `bus.input().stop()` ends the loop, which closes the receiver.

## Receiver lifecycle off the grid thread (#212 follow-up)

The rule: **the grid thread never calls an SDK create or destroy.** On the box
(#212 comment 5849047061) a source change closed the old receiver and created
the new one ON the grid thread. The destroy alone blocked it for ~0.5 s, which
gave `> 8 boundaries missed — resync`. While the input is cut to `SP-program`,
that is ~14 black-filled program slots. Design record: #212 comment 5849076208
(Approach 1). The code is `playback/ndi_input_connect.rs`, a child module of
`ndi_input.rs`.

- **Connect.** A (re)connect runs on a short-lived `ndi-input-connect` thread.
  It calls `NdiFrameSync::connect` and hands the result back through
  `mpsc::sync_channel(1)`. `apply_settings` does, per boundary:
  1. apply a settings change: the old pair goes to the close helper;
  2. `try_recv` a finished connect and swap it in;
  3. request the next connect if one is needed.
- **Close.** A replaced or disabled pair is dropped on an `ndi-input-close`
  thread. A pair whose connect was superseded is dropped the same way.
- **Meanwhile.** Every boundary goes through the disconnected path: `capture`
  finds no receiver, so the input offers the standby pair (black + silence)
  and counts `no_source_boundaries`. The candidate and standby logic is
  unchanged.
- **One connect in flight.** A settings change during a pending connect
  supersedes it. When the stale pair lands it is closed off the grid thread,
  and the connect for the newest settings starts on that same boundary.
  Changing back to the pending connect's own settings makes it current again.
- **Retry.** A failed connect retries exactly `INPUT_RECONNECT_100NS` (5 s)
  after the boundary that REQUESTED it, not the one where the failure landed.
  So the retry timing never depends on how long the helper took.
- **Helper edge cases.**
  - A connect helper that could not be spawned leaves a closed channel
    (`Disconnected`), which lands as a failed connect.
  - A panicking helper does the same. The shipped exe unwinds: `src-tauri` is
    outside the workspace, so its release profile keeps cargo's default
    `panic = "unwind"` (see `crash-diagnostics.md`). Only that helper thread
    dies, the panic hook records it, and the closed channel lands as a failed
    connect, retried 5 s after the attempt (tested with `set_panic_create`).
  - A close helper that could not be spawned closes inline, with a WARN.
- **Stop path.** It runs after the loop ended.
  - The current pair, and a connect result that was already handed over but
    not taken yet, are closed on the close helper and JOINED.
  - A connect still running is abandoned: its helper's `send` finds no
    receiver, and the helper drops (closes) the pair itself.
  - Closes started by earlier settings changes may still be running, detached.
    That is harmless, since the loop is over.
  - A hand-over that lands in the instant between `try_recv` and the drop of
    the channel is dropped with the channel on the ending input thread. That
    is inline and untimed, but harmless: the loop is over.
  - The helper publishes `last_connect_ms` only AFTER its hand-over. So a
    test that waits for a value set by THIS connect (e.g. ≥ 400 ms for the
    mock's 500 ms) knows the result is in the channel. The rig's own first
    connect already left a value there, so waiting for any value is not
    enough.
- **Telemetry** in `input`:
  - `connects_pending` (0 / 1), published by the grid;
  - `last_connect_ms` / `last_close_ms`, which the helpers time themselves.

  The log lines `receiver + FrameSync created` / `creating the receiver failed`
  carry `connect_ms`, and `receiver closed` carries `close_ms`. A superseded
  connect logs `a superseded connect finished — closing it off the grid thread`.
- **Trade-off (accepted).** A source change takes effect once the helper's
  create is done. The old receiver lives ~0.5 s longer on its close thread,
  concurrently with the new one's create (the per-handle backend locks make
  that concurrency real).
- **Box check.** Put the input on program and change the source in Nastavenia.
  - The program `filled` and `resyncs` counters must stay +0, and the input
    must log no `> 8 boundaries missed` WARN.
  - The change must log `receiver + FrameSync created`, and never
    `creating the receiver failed`. The new create now overlaps the old
    receiver's destroy, so two receivers named `SongPlayer program input`
    exist for ~0.5 s. If the SDK rejected that, the change would cost 5 s of
    standby while counting no fills or resyncs. The fix would then be a
    per-connect suffix on the receiver name.
  - `last_close_ms` / `last_connect_ms` show the SDK's real cost.

## Settings, API, UI

- Keys:
  - `ndi_input_enabled`: only `"true"` enables.
  - `ndi_input_source`: the full `"MACHINE (stream)"` name, trimmed.
- `run_input_config_task` re-reads the keys every 5 s.
  - While the input is ENABLED and not connected, it lists the visible NDI
    sources every 30 s on the blocking pool (a 1 s `find_wait`). This also runs
    with no source name yet, which is exactly when the operator needs the
    names.
  - The list is served as `input.visible_sources`, at most 16 names.
  - "The configured source is not visible" is logged only for a non-empty
    source.
- API:
  - `GET /api/v1/program` and the cut answer carry `input`:
    `{id, label, enabled, running, connected, source, stream, frames_received,
    video_repeats, video_drops, no_source_boundaries, unsupported_boundaries,
    boundaries, resyncs, relatches, connects_pending, last_connect_ms,
    last_close_ms, audio_queue_depth, last_frame_size, format, frame_rate,
    visible_sources}`.
  - `enabled` and `source` come from the STORED settings, so a save shows at
    once.
  - `stream` = `extract_ndi_stream_name(source)` (`playback/ndi_input_name.rs`,
    a child module of `ndi_input.rs`; #221 lane 3 moved it out of the deleted
    `obs/ndi_discovery.rs`).
  - The input is a program source only while it is ACTIVE:
    `ndi_input_enabled` plus a non-empty `ndi_input_source`
    (`InputSettings::active()`, the same rule as `NdiInput::service`). An
    enabled input with no source never receives anything, so cutting to it
    would only ever show black.
    - `POST /api/v1/program/cut {"source": -1}` → 404 unless active.
    - A persisted `program_source = -1` is restored only while active.
  - Disabling the input WHILE it is on program leaves the program selected
    on `-1`. The input thread then offers its standby pair (black + silence),
    so there are 0 fills. The dashboard shows "Na programe: OBS manuál"
    without the button; cut to a playlist to leave it.
- sp-ui:
  - `ProgramControl` lists an extra `program-cut` button with
    `data-playlist-id="-1"` and the text "OBS manuál" AFTER the playlists, only
    while `input.enabled` with a non-empty `input.source`
    (`ProgramInput::is_source`, the server's rule).
  - Nastavenia has the fieldset `settings-ndi-input`, with
    `settings-ndi-input-enabled` and `settings-ndi-input-source` (placeholder
    `CG-OBS (manual)`).
- The mock's `GET /api/v1/program` derives `input` from the stored settings,
  and it refuses a cut to `-1` with 404 unless the input is active. The specs
  reset the settings (`/__mock/settings-reset`) and the program
  (`/__mock/program-reset`).

## Tests

- `ndi_input_tests.rs` runs the input over the mock into a REAL `ProgramBus`
  with `-1` on program:
  - frames are 4×2 UYVY and the standby black is 2×4, so a job's width names
    its owner;
  - `drain()` panics on a program fill, because the input must own every
    boundary.
- #223: the input offers its capture at the SOURCE's own size; the
  `SP-program` sender fits it into the 1920×1080 canvas like any source pair
  (`program-bus.md` "SP-program is ALWAYS 1920×1080").
  `ndi_input_tests_fhd.rs` (a child module) takes one captured 4×2 job
  through `ProgramOutput::fhd` and asserts the 1920×1080 stride-1920 send.
- The loop tests run `run_input_loop` on its own thread with a virtual
  `FakeClock`: a sleep advances it, it never goes past `limit`, and it can do
  one jump. They wait with bounded polls and `recv_timeout`, so a no-op stop
  fails in ≤ 10 s instead of hanging.
- The converted buffer's size class is pinned through `frame_pool::pool_len`
  on a unique 50×34 class (2550 bytes). A wrongly sized `take` would be an
  equivalent mutant otherwise.
- The connect is asynchronous, so `rig()` awaits it before `b(1)` (`settle`
  applies the settings until the pair or a scheduled retry lands, servicing
  no boundary). `raw_rig()` is the not-yet-connected input.
- `ndi_input_tests_lifecycle.rs` (a child module) holds the lifecycle tests.
  The mock's `set_blocking(500 ms, 500 ms)` makes `recv_create` /
  `recv_destroy` block like the box's SDK. The tests assert:
  - one pair per boundary, stamped on it;
  - the exact standby pair in the connect window;
  - the new pair's FrameSync handle capturing only after it landed;
  - `connects_pending` 1 during the connect and 0 after;
  - exactly three `recv_create`s for a superseded connect;
  - the stop path's handed-over and abandoned pairs;
  - a helper that dies (`set_panic_create`) counting as a failed connect.
- The rule "never on the grid thread" is asserted through the mock's
  `calls_by_thread()`. Every create runs on `ndi-input-connect`. Every destroy
  runs on `ndi-input-close`, except an abandoned connect's pair, which its
  connect helper drops. The loop test runs `run_input_loop` on a thread named
  `ndi-input` and finds no create or destroy there.
- "The grid never WAITS on them" is asserted with the mock's `set_held(true)`.
  The old pair's destroy and the new pair's create both stay inside the SDK
  until released, and the loop must still service 20 more boundaries,
  bounded by `wait_for`. The stop-path test holds the create the same way.
  - This catches any wait on the INPUT's side, with no timing threshold: a
    `recv` instead of `try_recv`, a `join` in `release`, or one of its own
    locks held across the call.
  - It cannot see the BACKEND's lock scope, because the mock has no lock. That
    is pinned by `receive.rs`'s fake-SDK test (see the SDK section).
- NEVER assert the rule with a wall-time threshold. The coverage job runs the
  tests under tarpaulin's ptrace, where a thread can stall for a long time on
  a breakpoint.
  - The loop test's clock is paced virtual time (`PacedClock`: every wait
    really sleeps, but only the waits advance it). So it can never resync on
    a stall. That makes its own "no resync / contiguous" checks structural:
    the `set_held` phase is what proves the grid does not wait.
  - Timed durations are only ever lower bounds (≥ 400 ms for the mock's
    500 ms).
  - Every wait is bounded (`wait_for`, 10 s), so a hang fails instead of
    stalling the mutation gate.

## Box acceptance (the supervisor's job)

Set `ndi_input_source` to a live NDI source on the LAN (the cg OBS program NDI
output) and cut to "OBS manuál" and back in a real browser. Check that:

- the program stamps are contiguous;
- there are 0 fills;
- `input.frames_received` advances;
- `input.connected` is true.

On a 25 fps source, `input.video_repeats > 0` confirms that the real FrameSync
returns the SAME buffer for a repeat, which is what the repeat detection keys
on (timecode + data pointer). If it stays 0 while `frames_received` climbs at
30/s, every repeat is re-converted: the picture is still right, but it costs
CPU. Then key a repeat on the timecode alone.

Never switch the OBS program scene.
