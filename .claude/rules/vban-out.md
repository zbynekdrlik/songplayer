---
paths:
  - "crates/sp-server/src/playback/vban_*.rs"
  - "crates/sp-server/src/playback/mmcss*.rs"
  - "crates/sp-server/src/playback/stat_window*.rs"
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

- `ProgramOutput::serve` (`program_output.rs`) hands each pair's audio to
  `VbanOut::push` BEFORE the pair's `SP-program` NDI submit (#210 stall fix,
  design record 5911744233). Same stamp, same samples:
  - a forwarded or mixed block is COPIED (`VbanBlock::copied`, 12.8 KB): the
    NDI submit still borrows the pair after the push;
  - a standby pair becomes `VbanBlock::silence`;
  - a mixed boundary computes its crossfaded block and pushes it FIRST, then
    paints the picture (`paint_mix`) and submits.

  The order is structural: `serve` = `split` (the audio side, no video work)
  → `feed_vban` → `submit_video` (a mix's picture, the standby black, the NDI
  submit), and the run of mixed boundaries ends only after the unmixed
  boundary went out. So no video-side cost of a boundary — a slow NDI send,
  a mixed picture — delays that boundary's FOH block. The sender is one
  thread: a video side longer than a slot still delays the NEXT boundary's
  take, which shows as `ready_late_us` (below). Before the fix the push came
  after the NDI submit, and a late submit showed on dev1 as a 20–35 ms gap
  followed by a 6–8 packet burst (#210 findings 5907620763 / 5907883948).
  Pinned by
  `program_output_tests_order.rs`: the NDI backend holds its first send
  behind a gate, and VBAN must already have the block (all three kinds).
  Keep that order in any new submit path.
  A pair whose audio is not exactly one 48 kHz stereo 1600-frame frame is sent
  as silence and counted in `blocks_substituted`.
- The queue never blocks. Over `VBAN_QUEUE_BOUND` (10 = the program queue's
  bound) it drops the OLDEST block and counts it in `blocks_dropped`.
- The `vban-output` thread (`run_vban_loop`, Windows) encodes one block into 8
  packets of 200 frames, as INT24 PCM with full scale ±8388607 and clamping.
  It sends packet k of boundary B at `due(B) + L + k·1e7/240` (100 ns, floored:
  0, 41 666, 83 333, …, 291 666). L is TWO slots (`VBAN_SEND_LATENCY_100NS` =
  666 666). It was one slot at first; the 26.9.2026 FOH capture (VB-Matrix stream 6,
  5 min) showed 0.5 % late sends and Overload +11 / Underrun +14 against cg's +3 / +7,
  because a program block arrived only after the source submit AND the `SP-program`
  NDI submit (each up to ~20 ms p99; since the #210 fix the program's NDI submit
  no longer precedes the push). An on-time packet gets exactly one wait, so they go out evenly.
  A block that arrives AFTER its first packet is due sends its past-due packets
  back-to-back, each counted in `late_sends`. That happens after a program fill
  past the 3-slot grace or a real stall — never at a fleet date step since
  #224 part 2 (below). If the box capture shows
  `late_sends` climbing there, re-measure the block arrival lead before raising L again.
- Residual: a program RESYNC (> 8 missed slots) skips stamps. VBAN then has a
  time gap while its counter stays contiguous, and the receiver sees an
  underrun, not a counter loss.
- The thread is an MMCSS "Pro Audio" thread at `AVRT_PRIORITY_HIGH` for its
  whole life (#210 part 2, `mmcss::join_pro_audio`; below), with the 1 ms
  multimedia timer. When MMCSS refuses it, it falls back to
  `THREAD_PRIORITY_TIME_CRITICAL` (`pipeline_audio::raise_thread_priority`,
  shared with the NDI audio emitter and the NDI input).
- Clock: its own `WallClock`, ticked through `program_output::BoundaryTicker`
  once per boundary passed (`WallVbanClock::slewing`; the NDI input uses
  `WallVbanClock::new`, which follows its wall). `run_vban_loop` reads the clock on
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
  late_sends (> 2 ms after due), late_max_us, late_events[{utc_ms, late_us}]
  (#210 part 2, below), send_interval_p99_us (last 1200 intervals),
  frame_counter, slew_owed_us (#224 part 2), targets[{target, addr, error}]}`. The API reaches it through
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

## The program boundary's timing (`health.timing`, #210)

What makes a FOH block late is named on the box, per boundary, by the
`SP-program` sender (`playback/program_output_timing.rs`, pure):

- `ProgramOutput::serve(job, now)` returns `BoundaryMarks`, four instants
  off the sender's own wall (the stamps' timeline): the job taken, the VBAN
  hand-off, the NDI submit started and returned. `BoundarySample::of` →
  `ready_late_us` (taken − boundary), `vban_feed_late_us` (hand-off −
  boundary) and `submit_us` (the video side after the hand-off: since #223
  the picture made a 1920×1080 canvas picture — a fit, or a fade's picture —
  then the NDI call; before, the NDI call alone); an instant before its
  reference is 0 late.
- `BoundaryTiming` (in `ProgramCore`, fed through
  `ProgramBus::record_timing` by `run_program_loop`) keeps each figure's
  worst over the last 60–120 s (two 1800-boundary buckets,
  `stat_window::TwoBucketWorst`; its WARN rate limit is
  `stat_window::WarnLimiter`, both shared with the VBAN thread's
  `VbanStallLog`), counts the
  boundaries over 5 ms per figure since start, and counts
  `vban_feed_late_over_10ms` (`VBAN_FEED_SLOW_US`, a trend figure) and
  `vban_feed_late_over_budget` (below).
- VBAN's budget for a hand-off is its send latency L:
  `VBAN_FEED_BUDGET_US` = 66 666 µs, whole µs of `VBAN_SEND_LATENCY_100NS`
  (pinned). A block handed over later than that reaches the VBAN thread
  after its first packet was due, so its packets go out late, back to
  back. That is `vban_feed_late_over_budget`, and it is the ONLY thing
  WARNed (#210 part 2, design record 5916097259). Part 1 WARNed over 10 ms
  and fired every 5 s: hand-offs 10–33 ms late are the program's normal
  state on the box (41 % of the boundaries over 10 ms in the 15 min capture
  of finding 5915907311; 67 834 of 105 105 since start in the lane's read,
  comment 5916271682), and each one is still inside L.
- ONE WARN `program output: a boundary's VBAN audio was handed over after
  its first packet was due (over VBAN's send latency) — its packets go out
  late, back to back` per such boundary, at most one per 5 s of timeline;
  the next WARN carries `suppressed` (how many it skipped). It names
  `boundary_100ns` (the wire stamp), `boundary_utc` (to line up with a dev1
  capture) and the three figures. The decision is `observe`'s return value
  (tested, `warned` counts it); the log call is `mutants::skip`.
- `GET /api/v1/program` → `health.timing`: `{boundaries,
  ready_late_us_max, vban_feed_late_us_max, submit_us_max,
  ready_late_over_5ms, vban_feed_late_over_5ms, submit_over_5ms,
  vban_feed_late_over_10ms, vban_feed_late_over_budget, warned}`. The
  counts only grow: read them twice and diff over the capture window.
- Reading it: a late `vban_feed` with a late `ready_late` is upstream of the
  sender (a late source or release, or the sender still busy with the
  boundary before); `submit_us` alone high is the video side — the canvas
  fit of a source that is not 1920×1080 (#223), a fade's picture, the NDI
  SDK — and, since the fix, no longer delays its own boundary's FOH block. The sender is one thread,
  so the next boundary's take (its `ready_late_us`) still slips whenever the
  boundary before ends past that job's arrival: a late take plus its video
  side, which `submit_us` times whole since #223: the canvas fit or a
  fade's mixed picture (the fade's INFO line also has it as
  `max_picture_us`), then the NDI submit.
- A fleet date step is not a stall. The figures compare the sender's wall
  with the sources' stamps, and at a step the two walls can sit up to r
  (< one slot) apart for about one boundary (`program-bus.md`, "One clock
  domain"). So a step that the program wall follows first can add ~one
  counted boundary with no real stall, over 10 ms when r > 10 ms — never
  over the budget, and so never a WARN (r < one slot < L). Match it to the
  step (the relabel log line, `vban.slew_owed_us` ≠ 0) before calling it
  one.
- Tests: `program_output_timing_tests.rs` (exact pins, the two buckets, the
  5 ms / 10 ms / L / 5 s edges; a hand-off inside L is counted and never
  WARNed), `program_output_tests_order.rs` (an NDI send that advances a
  settable wall shows in `submit_us` only; the real loop puts one
  boundary's figures on the bus, 15 ms late: counted, not WARNed),
  `api/program_tests.rs` (the JSON names).

## The VBAN thread's scheduling and its own late packets (#210 part 2)

Design record 5916097259. After part 1 the bursts were gone, but single
packets still went out 11–20 ms late on a fixed 10 s grid, the next one on
time (finding 5915907311, the stem worker ruled out). That is the
`vban-output` thread not running for that long.

- **MMCSS** (`playback/mmcss.rs`). `MmcssTask::join` is an RAII guard:
  `AvSetMmThreadCharacteristicsW("Pro Audio")`, then
  `AvSetMmThreadPriority(AVRT_PRIORITY_HIGH)`; its drop calls
  `AvRevertMmThreadCharacteristics`. The calls come to a pure
  `MmcssOutcome`:
  - `High`: in the task at HIGH;
  - `TaskOnly`: in the task, but the priority call failed. It still runs
    in MMCSS's real-time band; a WARN names the error;
  - `Refused`: `needs_fallback()` → `THREAD_PRIORITY_TIME_CRITICAL`, with a
    WARN naming the error (`avrt_error_name`: ERROR_INVALID_TASK_NAME 1550,
    ERROR_INVALID_TASK_INDEX 1551, ERROR_PRIVILEGE_NOT_HELD 1314).

  `join_pro_audio(thread)` is the one call a thread makes (Windows; the
  guard lives for the thread's life, `let _mmcss = …`). Off Windows `join`
  is a stub that is always refused. The avrt calls are in windows-sys 0.59
  `Win32::System::Threading` (NOT `Win32::Media`), feature already on.
  avrt.dll is a load-time import: it ships with every Windows since Vista
  (desktop, and Server with the Desktop Experience), so the process never
  starts without it; a missing DLL would stop the start, not take the
  fallback. Any real-time sender thread can take the same call (the NDI
  input, the program output); only `vban-output` does so far.
- **Why it helps** (anchors comment 5916282660). TIME_CRITICAL is 15 in a
  NORMAL_PRIORITY_CLASS process: the top of the normal band, the same
  level the NDI runtime's own threads can set. NDI 6.3.2 imports
  `SetThreadPriority`, with no avrt string in the DLL, and its docs say
  nothing on MMCSS. "Pro Audio" runs the thread at 26 on the box and 27
  at HIGH (a PowerShell P/Invoke probe on Windows 11 IoT 22631), above
  every normal-band thread. OBS registers its audio threads the same way,
  with task "Audio" at default priority (`libobs/media-io/audio-io.c:205`).
  VB-Audio's own VBAN senders (Voicemeeter, VB-Matrix) are closed source.
  The VBAN Protocol Specifications rev. 13 (Sep 2025) says nothing on
  sender scheduling, priority or MMCSS. It asks only that the RECEIVE
  thread never wait (p. 11). It also notes (p. 12) that Voicemeeter
  receivers keep a stack of at least 6 × 256 samples and "might consider
  receiving burst of 2, 3 or 4 VBAN packets", because its own senders
  follow the audio buffer size.
  The box's MMCSS profile:
  - `NetworkThrottlingIndex` 0xFFFFFFFF: MMCSS's network throttling is off,
    so an MMCSS thread cannot slow the NDI traffic;
  - `SystemResponsiveness` 0;
  - the Pro Audio task: Priority 6, Scheduling Category High.
- **Log lines** (one per thread start):
  - INFO `mmcss: the thread runs as an MMCSS "Pro Audio" thread at
    AVRT_PRIORITY_HIGH` (`thread`, `task_index`);
  - WARN `mmcss: the thread joined MMCSS "Pro Audio", but
    AvSetMmThreadPriority(HIGH) failed — it runs at the task's own
    priority`;
  - WARN `mmcss: MMCSS "Pro Audio" refused the thread — it falls back to
    THREAD_PRIORITY_TIME_CRITICAL` (`error`, `error_name`), then
    `paced thread: priority = TIME_CRITICAL`.
- **Its own late packets** (`playback/vban_stall.rs`, pure `VbanStallLog`,
  inside `VbanOut`'s counters: ONE lock per packet on the real-time thread,
  review round 1; `status()` only COPIES the counters under that lock and
  sorts / builds after it, so an API poll holds the thread's lock only for
  that copy: two small allocations (the 1200 intervals, 9.6 KB, and the
  ring of at most 32 events) plus their memcpy, review rounds 2–3).
  `send_block` times every packet it sends against its planned instant,
  `due + L + k/240 s`:
  - over 5 ms late = an event `{utc_ms, late_us}` in a ring of the last
    32, served oldest first as `vban.late_events`. `utc_ms` is the fleet
    label of the send reading (`VbanClock::label_100ns`, defaulted to the
    reading itself; `WallVbanClock` answers `t + D(K_F)`): UTC, to line up
    with a capture. In the ~14 min after a date step VBAN's clock still
    owes the step's movement (`vban.slew_owed_us`), and `utc_ms` is off UTC
    by that much: before it after a forward follow (≤ one slot), after it
    after a residue hold (≤ ~4 ms);
  - `vban.late_max_us`: the worst packet over the last 60–120 s of sending
    (two buckets of 14 400 packets; it does not age while nothing is sent);
  - over 10 ms = ONE WARN `vban output: a packet went out more than 10 ms
    after its planned instant` (`utc`, `late_us`, `packet` = k,
    `waited_us`, `suppressed`), at most one per 5 s of VBAN's timeline.
    `waited_us` > 0 means the thread overslept that wait (a scheduling
    stall inside the sleep). 0 means it came to the packet already late:
    a stall before it (taking the block, or right after the packet
    before), a block handed over late, or the packet before sent late.
    The decision is `observe`'s return value; the log call is
    `mutants::skip`.

  Every packet counts, so a block that reached the thread more than 5 ms
  after its first packet was due shows as a run of events: its packets
  still over 5 ms late, packet k about X − 4.167·k ms for a block X ms past
  due. A block 0–5 ms past due counts in
  `health.timing.vban_feed_late_over_budget` and `late_sends` but adds no
  event. The packet WARN of such a block, if one fires, carries
  `waited_us` 0.
- Tests: `mmcss_tests.rs` (the outcome table, the fallback, the names, the
  UTF-16 task name, the Linux stub; on the Windows job the real call for an
  unknown task), `vban_stall_tests.rs` (exact pins from a scratch Python
  model: the 5 ms / 10 ms / 5 s edges, the ring, the two buckets, a late
  packet through `send_block` into `status()`, a clock whose label is not
  its reading, the wall clock's label), `stat_window_tests.rs` (the shared
  window and rate limit), `api/program_tests.rs` (the JSON names).

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
Routing fohabl/lv1 in VB-Matrix is B4, with the owner's go. The #210 stall
fix adds: a 15 min dev1 capture with 0 inter-arrival gaps over 15 ms and 0
bursts, and `health.timing` read right before and right after the capture.
Part 1's design record (5911744233) also asked that
`vban_feed_late_over_5ms` / `_over_10ms` do not grow and
`vban_feed_late_us_max` < 5 ms. Part 2 SUPERSEDES those three: the box
showed hand-offs 10–33 ms late to be the program's normal state, all
inside L. They are trend figures only now; the hand-off criterion is
`vban_feed_late_over_budget` +0 (below). The `*_max` figures cover only
the last 60–120 s, so a stall early in a 15 min capture shows only in the
counter diff. A fleet date step inside the capture may add about one
counted boundary with no real stall (never a WARN, see "Reading it"):
match it to the step's relabel log line before failing the run. If a late
`ready_late` remains, the next step targets that source by the measured
cause.

Part 2 (design record 5916097259) adds, over the same kind of 15 min dev1
capture:

- 0 gaps over 15 ms and 0 bursts;
- `vban.late_events` read right after the capture: empty, or every
  `late_us` < 10 000;
- `health.timing.vban_feed_late_over_budget` +0;
- the log's `mmcss: the thread runs as an MMCSS "Pro Audio" thread` INFO
  at the thread start.

If stalls remain under MMCSS, the next step is a WPR trace of one
predicted grid instant (DPC / ISR attribution; `late_events` gives the
instants), with a FINDING on the record before any further change.

## FOH routing on fohabl (VB-Matrix over VBAN-TEXT)

On 28.9.2026 FOH "CG L/R" (`VASIO32.OUT[27..28]`) was switched back to the cg OBS VBAN (`VBAN2`). SongPlayer's `sp-program` (`VBAN6`) points stay present but muted (#210 comment 5864647147).

- **Switch with `Mute` only.** VB-Matrix silently ignores `dBGain=-inf`: it sends no reply and applies no change. A revert that relied on it summed cg and SongPlayer on FOH for ~16 s.
  - SongPlayer → FOH: `Point(VBAN2.IN[1],VASIO32.OUT[27]).Mute = 1;Point(VBAN2.IN[2],VASIO32.OUT[28]).Mute = 1;Point(VBAN6.IN[1],VASIO32.OUT[27]).Mute = 0;Point(VBAN6.IN[2],VASIO32.OUT[28]).Mute = 0;`
  - cg → FOH: the same four points with the Mute values swapped.
- **Read back every point after each write.** Record the prior values first (fohabl is critical production).
- **FOH follows cg now.** Before a SongPlayer deploy or restart, or an Arena kill/relaunch, on win-resolume, message the camera-box session: it watches the cg audio path.

## A fleet date step: SlewRemainder (#224 part 2)

The walls relabel a date step (`genlock.md` "A date step relabels"): the whole
slots N never reach any timeline, the timeline moves only by the remainder r
(< one slot). VBAN has no timecode and VB-Matrix paces by arrival, so even a
jump of r would send r of audio at once (the 20:58Z +260 ms step sent ~80
packets back to back before part 2).

- `WallVbanClock::slewing` (the VBAN thread's clock, `vban_clock.rs` since
  review round 1; `vban_out` re-exports it) reads `line − owed`
  (`RemainderSlew`, pure): when its wall's summed line movement
  (`WallClock::shift().moved_100ns`: the LINE's net movement, measured at
  the tick's instant, in every tick that followed a step or rejoined after
  an idle gap — a resample's armed 1 ms and a rejoin hold a follow
  re-anchors through included, review round 3) changed, it OWES the change
  (SIGNED: r ahead, or a residue hold of at most ~4 ms), so its own reading
  neither jumps nor stops; the owed amount then shrinks toward 0 at
  `VBAN_SLEW_PPM` = 40 ppm of the elapsed line (100 ns per 2.5 ms: a whole
  slot is paid in ~14 min). It reads the timeline's LINE
  (`WallClock::line_100ns`), which runs on through a hold. The queue holds
  up to r more meanwhile (about one block at most, far under the bound); a
  hold's packets go out up to its size early, inside the 2-slot send
  latency. A movement over `VBAN_SLEW_MAX_100NS` (one slot + the 3 ms
  residue: only a rejoin after a > 10 s VBAN stall, or two epochs at once)
  is taken at once with one WARN (`vban clock: the timeline moved more than
  a slot at once`) and counted (`WallVbanClock::taken_at_once`): slewed at
  40 ppm it would keep VBAN off the program for hours. The cap applies to a
  tick's NET movement: a > 10 s VBAN stall whose rejoin drifted over 3 ms
  AND a follow in the same tick is taken at once whole (rare; known).
- 40, not 100: the packet spacing is already 41 666 / 41 667 / 41 668 × 100 ns
  and each wait rounds to 100 ns, so one interval can pay ⌈41 668 × ppm /
  10⁶⌉ × 100 ns; up to 47 ppm that is at most 2, keeping EVERY packet
  interval within 4.1667 ms ± 100 ppm. At 50 ppm a 41 668 interval paid 3
  and reached +104 ppm at one step phase in three (review round 3). Pinned
  for +260.3 ms, −19.8 ms and a residue hold at three consecutive step
  phases, `late_sends` 0, none under 1 ms: `vban_out_tests_regrid.rs`.
- A residue hold IS owed (review round 1 🟡 G): read off the frozen wall,
  one packet interval stretched by the hold (4.67 ms for 500 µs;
  `a_residue_hold_at_a_follow_is_slewed_too_never_a_gap`). A bounded
  resample's ≤ 1 ms correction in a tick that followed nothing is NOT owed:
  VBAN follows it at once, as before #224 (#210). That is the drift since
  the last resample (~100 µs at ±30 ppm, up to ~313 µs at 94 ppm: one
  interval of ~4.07 / 4.27 ms), or up to 1 ms when a resample arms a step
  whose follow comes in a LATER tick: a 1–2 ms step only the next resample
  confirms, or a step over 2 ms landing on the resample tick whose probe is
  rejected (a wide or unconfirmed read; 1 tick in 100 plus a preempted
  read). The ±100 ppm guarantee covers date steps only.
- Telemetry: `vban.slew_owed_us` on `GET /api/v1/program`, signed — r
  right after a follow (negative after a residue hold), then toward 0; 0 in
  steady state. `run_vban_loop` publishes it every pass.
- Box acceptance at a controlled step: a dev1 capture with 0 bursts and 0
  gaps over ~5.2 ms across the step, `late_sends` +0, `slew_owed_us` ≈ r
  after it.
