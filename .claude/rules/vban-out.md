---
paths:
  - "crates/sp-server/src/playback/vban_*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "e2e/settings-vban.spec.ts"
---

# VBAN audio output of the program (#210, B2 of EPIC #174)

SongPlayer sends the program audio (the samples `SP-program` carries) as VBAN
to FOH (VB-Matrix on fohabl) and lv1. This replaces cg OBS's bursty obs-vban
(#148). Design record: #210 comment 5846308506 (Approach 1).

## Data path

- `ProgramOutput::submit` (`program_output.rs`) hands each submitted pair's
  audio to `VbanOut::push` right AFTER the NDI submit:
  - a forwarded block is MOVED in (`VbanBlock::from_frames`, no copy);
  - a standby pair becomes `VbanBlock::silence`.
  A pair whose audio is not exactly one 48 kHz stereo 1600-frame frame is sent
  as silence and counted in `blocks_substituted`.
- The queue never blocks. Over `VBAN_QUEUE_BOUND` (10 = the program queue's
  bound) it drops the OLDEST block and counts it in `blocks_dropped`.
- The `vban-output` thread (`run_vban_loop`, Windows) encodes one block into 8
  packets of 200 frames, as INT24 PCM with full scale ±8388607 and clamping.
  It sends packet k of boundary B at `due(B) + L + k·1e7/240` (100 ns, floored:
  0, 41 666, 83 333, …, 291 666). L is one slot (`VBAN_SEND_LATENCY_100NS` =
  333 333). An on-time packet gets exactly one wait, so they go out evenly.
  A block that arrives AFTER its first packet is due sends its past-due packets
  back-to-back, each counted in `late_sends`. That happens after a program fill
  past the 3-slot grace, or for ≤ ~3.3 s after a confirmed fleet date step
  (walls up to ~1.5 slots apart, see program-bus.md). If the box capture shows
  `late_sends` climbing there, L = 2 slots is the knob.
- Residual: a program RESYNC (> 8 missed slots) skips stamps. VBAN then has a
  time gap while its counter stays contiguous, and the receiver sees an
  underrun, not a counter loss.
- The thread runs at `THREAD_PRIORITY_TIME_CRITICAL`
  (`pipeline_audio::raise_thread_priority`, shared with the NDI audio emitter)
  with the 1 ms multimedia timer.
- Clock: its own `WallClock`, ticked through `program_output::BoundaryTicker`
  once per boundary passed (`WallVbanClock`). `run_vban_loop` reads the clock on
  EVERY pass, also for a block it does not send (disabled, no target) and on an
  idle wake every 100 ms (`VBAN_IDLE_WAIT`, under the 8-tick cap). If it did
  not, the wall would go stale while the output is off, and the first packets
  after enabling would burst or stall. That is the same cadence as the
  program and the pacer walls, so a UTC step slews in at the same rate — one
  clock domain. A single wait is capped at 4 slots (`VBAN_MAX_WAIT_100NS`), so
  a clock mismatch never parks the thread.
- The frame counter (`nuFrame`) grows by exactly 1 per SENT packet, across
  cuts and standby. While the output is disabled or has no resolved target,
  nothing is encoded or sent and the counter does not move.

## Wire format (verified against VB-Audio "VBAN Protocol Specifications" rev. 13)

The 28-byte header is little-endian:

- `VBAN`
- `format_SR` = 0x03 (SR index 3 = 48 kHz, sub protocol AUDIO 0x00 in bits 5–7)
- `nbs` = 199
- `nbc` = 1
- `format_bit` = 0x02 (INT24; bit 3 = 0; codec PCM 0x00 in bits 4–7)
- `streamname[16]`: ASCII, zero padded; a non-ASCII or control character
  becomes `_`
- `nuFrame`: u32

The payload is 1200 B of interleaved 3-byte LE samples, within the spec's
1436 B data maximum. The default VBAN port is 6980.

## Settings + telemetry

- The keys live in `sp_core::config`:
  - `vban_enabled`: only `"true"` enables;
  - `vban_stream_name`: default `sp-program`. Policy, not enforced by code:
    never `cg` before B4;
  - `vban_targets`: comma-separated `host:port`, at most `VBAN_MAX_TARGETS`
    (8); extra entries are ignored with a warning.
- `run_vban_config_task` re-reads them every 5 s, so a dashboard save applies
  without a restart. It resolves DNS on a change and every 60 s, with std
  `ToSocketAddrs` on the blocking pool and the first IPv4 address, because the
  socket is IPv4. The 60 s re-resolve runs only while enabled. A failed
  re-resolve KEEPS the target's last good address and shows the error.
- `GET /api/v1/program` and the cut answer carry `vban`: `{enabled, running
  (the thread is alive), stream_name, packets_sent, send_errors, blocks_dropped, blocks_substituted,
  late_sends (> 2 ms after due), send_interval_p99_us (last 1200 intervals),
  frame_counter, targets[{target, addr, error}]}`. The API reaches it through
  `ProgramBus::vban()`, so there is no new `AppState` field and `lib.rs`
  (1000/1000) is untouched.
- UI: Nastavenia fieldset `settings-vban` with the testids
  `settings-vban-enabled` (checkbox), `settings-vban-stream-name`
  (maxlength 16) and `settings-vban-targets` (placeholder `dev1.lan:6980`,
  never FOH). The mock keeps the vban_* keys
  absent (defaults) and `/__mock/settings-reset` restores the fixture. Its
  `GET /api/v1/program` derives `vban` from the stored settings. The spec
  waits for `settings-gemini-model` to read the fixture's `gemini-2.5-flash`
  before it clicks: the vban defaults equal the fixture, so only a field whose
  fixture value differs from the form default proves the load landed.

## Tests

The helpers in `vban_packet_tests.rs` (`parse_packet`, `ramp_block`) and
`vban_out_tests.rs` (`FakeClock` with a `reads` counter, `RecordingSink`,
`active_config`) are `pub(crate)`. `vban_out_tests.rs` reuses the packet
helpers, and `api/program_tests.rs` reuses `active_config`. The schedule is tested on `FakeClock`,
with exact send instants and the recorded sleeps. The counter test drives a
real `ProgramOutput` over `MockNdiBackend` and keeps the output alive past the
assertions. The loopback test sends through a real `UdpSocket` to
`127.0.0.1:<ephemeral>`.

## Box acceptance

Box acceptance is the supervisor's job: point `vban_targets` at a dev1 LAN
receiver and never at FOH, capture 60 s with tcpdump and check 0 counter gaps,
an interval p99 < 7 ms, and PCM that cross-correlates with `SP-program`.
Routing fohabl/lv1 in VB-Matrix is B4, with the owner's go.
