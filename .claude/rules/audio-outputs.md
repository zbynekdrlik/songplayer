---
paths:
  - "crates/sp-core/src/audio_outputs*.rs"
  - "crates/sp-server/src/playback/audio_out*.rs"
  - "crates/sp-server/src/playback/vban_rate*.rs"
  - "crates/sp-server/src/playback/vban_out_tests_dest.rs"
  - "crates/sp-server/src/playback/vban_packet_tests_format.rs"
  - "crates/sp-server/src/playback/vban_packet_tests_legacy.rs"
  - "crates/sp-server/src/playback/asrc*.rs"
  - "crates/sp-server/src/playback/asio_*.rs"
  - "crates/sp-server/src/api/audio*.rs"
  - "crates/sp-server/src/api/program_tests_outputs.rs"
  - "crates/sp-server/src/api/settings_tests_audio.rs"
  - "sp-ui/src/components/audio_outputs.rs"
  - "e2e/settings-audio-outputs.spec.ts"
  - "e2e/audio-outputs-gate*.ts"
  - "e2e/post-deploy-audio-*.spec.ts"
  - "src-tauri/resources/THIRD-PARTY-NOTICES.txt"
---

# Program audio outputs (#233)

Spec `docs/superpowers/specs/2026-10-07-audio-outputs-asio-design.md`, plan
`docs/superpowers/plans/2026-10-07-audio-outputs-asio.md` (three lanes; lane
1: the output list + VBAN per destination; lane 3: the ASIO output over
`azo`). Lane 2's section (the drift servo, the ASRC) comes first, then lane
1's, then lane 3's (the ASIO output).

## The drift servo and the ASRC (`asrc_servo.rs`, `asrc.rs`) — lane 2

Pure modules, no thread, no I/O, no runtime wiring yet: lane 3's ASIO worker
calls `Servo::observe` once per program block, gives the answer to
`Asrc::set_correction_ppm` and (through `frames_from_100ns`: positive =
insert, negative = skip) `Splice::insert` / `skip`, and pushes
`Splice::process(Asrc::process(block))` into the card's ring.

**The observation** (per block, on the program wall, 100 ns): when the block
was handled, its boundary (stamp), the frames buffered for the card (the
ring + the splice's 5 ms hold), the frames the splice has still to skip
(`Splice::pending_skip_frames`) and the frames the card consumed since the
output opened. The pending skip is counted OUT of the buffered frames,
SIGNED: the splice skips at most one block per call, so a 100 ms skip runs
over three blocks, and without it the servo would ask for the rest again
(review round 1: 150 ms of excess skipped 397 ms in 10 re-centres); after a
worker stall the frames still to skip can outnumber the buffered ones, and
a `saturating_sub` read that as more to skip (review round 3: a 300 ms stall
re-centred 15 times, 13 while a skip ran; signed: twice, none).

**The latency target is two grid slots (66.7 ms, VBAN's send budget) + the
entry's `delay_ms`, NOT the spec's "2–3 driver buffers".** The program hands
one 33 ms block per boundary, 10–33 ms late in normal operation, so a ring
held at a few 1.3 ms driver buffers underruns on every block (plan decision
1, main-session ruling 1). latency = buffered/rate + (handled − stamp); it
is jitter-invariant (a late block finds the ring that much emptier), so the
servo controls it, not the raw fill (`fill_ms` in the status = this latency,
ruling 7).

**camera-box's constants** (`camera-box/src/asrc_bench.rs`,
`vendor/obs-studio/libobs/media-io/asrc-compensator.h`), pinned by
`the_constants_are_camera_boxs`:

| Constant | Value | asrc_bench.rs | .h |
|---|---|---|---|
| `MAX_PPM` | 300 | 204 | 42 |
| `MAX_SLEW_PPM_PER_S` | 5 | 209 | 48 |
| `REGRESSION_SPAN_S` | 600 | 223 | 62 |
| `REGRESSION_MIN_POINTS` | 30 | 228 | 67 |
| `REGRESSION_LOCK_SPAN_S` | 60 | 235 | 76 |
| `REGRESSION_CAP` | 640 | 243 | 82 |
| `WINDOW_100NS` (WINDOW_S) | 1 s | 470 | 120 |
| `STEP_RESIDUAL_S` | 10 ms | 284 | 149 |
| `LEVEL_KP_PPM_PER_MS` | 2 | 373 | 232 |
| `LEVEL_KP_MAX_PPM` | 50 | 380 | 239 |
| `LEVEL_EMA_TAU_S` | 10 s | 392 | 251 |
| `LEVEL_KI_PPM_PER_MS_S` | 0.0002 | 264 | 130 |
| `LEVEL_INTEGRAL_MAX_PPM` | 3 | 271 | 137 |
| `MAX_SANE_WINDOW_PPM` (MAX_SANE_INSTANTANEOUS_PPM) | 100 000 | 447 | 109 |

SongPlayer's own: `GROSS_STEP_100NS` = one grid slot (333 333),
`RECENTRE_100NS` = 10 ms, `BASE_LATENCY_100NS` = `VBAN_SEND_LATENCY_100NS`
(666 666), all literals pinned against their sources by the same test.

**Sign:** a POSITIVE correction makes MORE output per input (the card runs
fast, or too little is buffered); rubato's relative ratio is `1 + ppm·1e-6`.

**The loop:** rate point per closed 1 s window = (mean handled − origin,
mean consumed/rate − (handled − origin)); the OLS slope over ≤ 600 s is the
card's ppm, used once 30 points span 60 s; P on a 10 s EMA of the
window-mean error, ±50; I ±3, frozen while |rate + P + I| ≥ 300 (I's own
share counts; the plan's form — camera-box freezes on its estimate + bias +
I, without P); clamp ±300, slew ≤ 5 ppm per second of wall (dt = the 100 ns
between applied windows, floored at 0, not capped: after a forward wall
step the slew may move 5·step ppm in one window).

**Steps:** a block more than one slot off target RE-CENTRES at once by its
own error. A window mean more than 10 ms off RE-CENTRES by the mean error of
the last 8 blocks (`RECENT_BLOCKS`, a ring emptied by a re-centre) when that
is also more than 10 ms off; otherwise the level loop takes the window as it
is (a transient already passed splices nothing). The action's
`recentre_100ns`: insert > 0, skip < 0; the window, the ring and the EMA
restart; the regression is untouched (the card's consumed count did not
move). Not by the window's mean: a step inside a window shows in its mean
only in part (review round 1: 20–30 ms steps failed at 4–9 of 17 window
phases). Not by one block either: a block's latency saws by one driver
callback period (review round 2: 2048 frames at 48 kHz re-centred 118 times
in 900 s). With the level at target the last 8 blocks lie after the step (a
step under one slot moves a 32-block mean past 10 ms only with ≥ 30 % of the
window behind it) and span ≥ 266 ms of the sawtooth. The trigger is the
window's whole error, step plus any standing offset: a −10.5 ms step on a
−45 ppm card whose level still stands 1.7 ms high from the start-up reads
−8.8 ms and is left to the level loop (pinned among the honest bounds). A
window whose wall goes back before its first point starts over (it would
otherwise stay open the step + 1 s). Known limit (review round 4, not fixed):
a pure wall step of ≳ 110 ms landing mid-window makes that window read over
100 000 ppm and FLUSHES the regression — the lock returns 60 s later, the
correction slewing to P + I meanwhile; the same step at a window start only
re-bases. A restart on a forward gap would need dense observations in every
unit test (they feed 1 s-sparse ones). A rate point more than 10 ms off the fit (once the
fit has 30 points) RE-BASES the regression: that point stays out, and the
NEXT point REALIGNS — the offset moves by its whole residual, the slope stays
(`Offered::Realigned`, checked before the step test, so one step re-bases
once and `rebases` counts steps; with the offset moved only there, no
equivalent mutant hides in a second offset update): the same straddle splits a step
across two window means, and a remainder under 10 ms would otherwise sit in
the regression as a level shift (~17 ppm of rate bias for a span). Before the
fit has its points a step RESTARTS it. A window measuring > 100 000 ppm (a
stalled card) FLUSHES it and holds the correction; `status()` reads the rate
and the lock from the regression, so the flush shows at once. A whole step
< 10 ms (a dropped callback, a 9 ms jump) enters the regression as a point —
camera-box's design, and its blind spot: the rate bias is ≈ 1.5·step/span
with the span growing from 60 s, so an early step weighs most (model, 20 ppm
card: −9.9 ms at 75 s peaks the correction at 133 ppm, 3 re-centres, still
9.5 ppm off at 900 s while P pays the level back; ±9 ms at 400 s ends 7–8 ppm
off). A lower re-base residual for SongPlayer's absolute window-mean
readings (much less noisy than camera-box's cumulative increments) is a
main-session decision, not taken here. A wall stepped back past the last window gives
no time: `dt` is floored at 0, so the EMA and the integral do not move (an
EMA over −10 s divides by zero, and the NaN would stay).

**The splice** (`Splice`): 5 ms fade out of the held tail, the silence or the
skipped frames (a skip may span blocks; nothing is emitted while it eats a
whole block), 5 ms fade in; one silent frame where the fade out ends. It
holds its last 5 ms back always (counted as buffered). A re-centre that
comes while still muted fades nothing twice.

**The ASRC** (`Asrc`): ONE rubato 5.0.1 `Async` sinc (256 taps,
BlackmanHarris², `FixedAsync::Input` of 1600 frames, Cubic), ratio room
±1000 ppm (`ASRC_MAX_RELATIVE` 1.001: rubato accepts `1/1.001 ..= 1.001`, so
−1000 ppm itself is refused), ramped across the next block. Delay
`sinc_len·ratio/2` (256 frames at 96 kHz; it follows the ratio: 255 at
−250 ppm). Its output count is NOT `1600·ratio` per block: the first block
gives 3 196 at 96 kHz (start index −255), so totals run 2·ratio frames short
plus half a block of the first ramp — derive pins from a model of rubato's
`calculate_output_size` / `step_index` (plain f64, exact), never a ±N guess
(the plan's 300-block "±4" failed at +100 ppm by 1).

**The closed-loop simulation** (`asrc_servo_sim_tests.rs`): card −50 / 0 /
+50 ppm, ±1 ms steps, ±20 / ±30 ms steps at three window phases, ±44 ms, a
100 ms forward step, a dropped callback, a 200 ms worker stall (underruns by
physics, the skip asked once), a 48 kHz card with 256-frame buffers, a
1024-frame driver at 48 kHz with a 20 ms step; 900 s each;
asserts 0 underruns, |ppm| ≤ 300, slew, latency error ≤ 10 ms after 70 s
(model: ≤ 4.1; + half a period for the 1024-frame driver), |final − card|
≤ 5 ppm, re-centres 1 (2 for a step over 10 ms from a level at target), no
re-centre asked while a skip runs. The sub-10 ms steps and the standing-offset
case have their own honest bounds
(`a_step_under_10_ms_enters_the_rate_and_is_paid_back`,
`a_10_5_ms_step_on_a_standing_offset_is_left_to_the_level_loop`). **Envelope for
lane 3:** a driver callback period well under one grid slot (≤ 512 frames
at 48 kHz, ≤ 1024 at 96 kHz; 2048 at 48 kHz saws past the per-block
threshold) — lane 3 reads the driver's preferred buffer and cannot change
it, so it should surface a larger one in the status. Harness rules learned
here:

- draw the hand-off jitter ONCE per block and run the card's callbacks due
  by then first; the plan's loop redrew it on every callback, which ran
  callbacks due after the hand-off before it (±20 ms phantom latency);
- check the slew on the 100 ns instants the servo saw, not the float wall:
  rounding gives a 5e-7 ppm "over-move";
- model the splice as the worker will run it: an insert at once, a skip
  from each later block's output, the pending skip in the observation;
  an instant re-centre hides the double-skip;
- sweep a step over ≥ 3 window phases: the straddle bug passed at the one
  phase the plan pinned.

The scratch model's fuzz (240 runs: cards ±120 ppm, jitter 0–30 ms, steps
−35…+150 ms at random window phases, drops, 44.1–96 kHz, buffers 64–1024 up
to 10.7 ms) held: no underrun within the budget, |ppm| ≤ 300, the slew, no
skip asked twice, |final − card| ≤ 5 ppm when undisturbed. Re-centres reach
4 only for a card beyond ±50 ppm (before the 60 s lock P saturates at 50 and
the level drifts once) plus a dropped 512-frame buffer at 44.1 kHz (11.6 ms,
a re-centre of its own).
Outside the envelope, by physics: a backward step larger than the target
minus the hand-off lateness (48 ms on 30 ms of jitter) can underrun (one
event, phase-dependent) — the audio does not exist yet; `delay_ms` widens
the margin.

**Mutation shape** (a model with one switch per listed mutant kills every
viable one): the fit centres x once and sums `dx·y` (a second centring has
equivalent mutants); eviction is `drain` past the cap, then a `while let`
that pops by span (every pass pops: no mutant can spin); the origin
subtraction `handled − origin` is only shift-visible, so
`a_wall_far_from_zero_is_measured_from_the_first_block` (wall 4.7e18) makes
its `+` mutant overflow; the splice always resizes by `insert` (an
`if insert > 0` guard has an equivalent `>=`); DC test signals hide a missing
fade, so the splice tests pin the exact silent-frame count and the final
level; the 8-block ring is pinned through alternating block errors (its
index arithmetic only shows when the blocks differ). The scratch model must
fold sums left to right: Python 3.12's `sum()` of floats is compensated and
disagrees with Rust in the last digits.

**A possible redesign (not taken; main-session call):** camera-box tests
each window's INCREMENT against the fitted slope (`asrc_bench.rs` ~1150), so
a step inside a window shows whole in one point; contiguous windows (each
starting at the previous one's closing observation) with that residual would
remove the realign.

**rubato** `=5.0.1` (newest; MIT OR Apache-2.0; rust-version 1.87, CI builds
on stable): its LICENSE-MIT, verbatim, is in
`src-tauri/resources/THIRD-PARTY-NOTICES.txt` (its own MIT/Apache
dependencies get none, like every other Rust crate the project links),
pinned against the `Cargo.toml` version by
`the_installer_notice_carries_the_pinned_rubatos_license` — re-copy the
notice when the pin moves. Added to the lock with `cargo update --workspace`
(Tier-0-allowed, compiles nothing): it adds exactly the new tree.

## The list (`audio_outputs`) and the network rate (`audio_network_rate`) — lane 1

- ONE setting, a JSON list of `sp_core::audio_outputs::OutputEntry`:
  `{id, name, type, enabled, rate ("network" | Hz), delay_ms (0..=2000),
  vban: {host, port, stream_name, format: int16|int24|float32}}`. Limits: 16
  outputs, 8 VBAN (the per-type caps sum under 16: a const assert in
  `audio_out_config.rs`); ids `[a-z0-9-]{1,32}`, unique; `out-N` from the
  dashboard (`next_id`, saturating). An entry is built by `OutputEntry::vban`
  or the server parser only (a new transport's field touches only those).
- `audio_network_rate`: 44100 / 48000 / 88200 / 96000 / 192000, default 48000;
  SNV = 96000 (MAIN SESSION OPS after the deploy). An entry at `"network"`
  runs at it.
- A PATCH is parsed strictly (`audio_out_config::parse_list`, through
  `Box<RawValue>` maps, never `serde_json::Value`): every error names the
  entry, the sanitized id (`shown_id`: a-z 0-9 - else `?`) and the field,
  never the value (serde's text can quote input); a value that is no list
  gives line and column only (serde_json 1.0.149 counts a top-level object
  as column 0). 400 refuses the whole PATCH, nothing written
  (`api/settings.rs::prepare` runs `checked` before the exchange check).
  Stored normalized (every default written out).
- The same rules in Slovak for the dashboard: `ListError::sk()`
  ("Výstup 1 (out-1): pole „cieľ“ je prázdne": the field as `pole „…“`, so
  every problem text agrees with the neuter "pole"; #233 review round 2).
- The outputs task (`audio_out_task.rs`) makes one pass (`tick`) every 5 s:
  the migration until it has run (below), then the list, read leniently
  (`parse_stored`): an entry this version cannot read is skipped and named
  in `outputs_problems` (a WARN once per new problem); the rest run. A
  stored value that is no list at all (`not_a_list`) changes NOTHING but the
  problem: what runs keeps running (Review Focus 3, never "all outputs
  off"; `a_stored_value_that_is_no_list_keeps_what_runs`).
- An entry identical to a running one UP TO ITS NAME (`same_but_name`, and
  built for the same rate) is KEPT: thread, queue, frame counter (`plan` →
  `Step::Keep`); the kept output takes the new entry, so a rename only
  relabels it. The exception: an output whose thread could not start
  (`RunningOutput::start_failed`, a failed UDP bind or spawn) is rebuilt on
  every pass until it starts. Only a new or changed entry is rebuilt; a network-rate
  change rebuilds only the `"network"` entries. So a dashboard save, a
  rename, or the post-deploy probe entry never disturbs FOH (Review Focus 2,
  `apply_keeps_an_unchanged_output_when_another_is_added`,
  `renaming_an_output_keeps_it_running_under_its_new_name`). A rebuilt
  entry's new output starts its frame counter at 0 (a receiver sees one
  jump). A replaced or removed output is stopped with `discard`: its queue
  is dropped and a stopped output takes no later push (`apply` replaces the
  list before it discards, so a boundary can still push into the old
  snapshot), so a delay lowered from 2 s to 0 sends at most the ONE block
  its thread already holds next to its successor (same host and stream
  name), never the old schedule; only the shutdown's `stop_all` drains
  (#233 review rounds 4–5). The slew's ±100 ppm per packet interval holds
  for FOH's 48 kHz INT24; its 100 ns steps are a larger share of a shorter
  interval (up to ~±250 ppm at 192 kHz).
- A built output's thread is started through a PARAMETER (`StartThread`):
  `start_outputs` passes `start_vban_thread` (the MMCSS thread on Windows),
  every unit test a no-op or a recorder. Before, `build` spawned the real
  thread itself, so on the Windows test job (`cargo test --workspace` on
  `windows-latest`) the apply tests got a live `vban-output` thread taking
  their queued blocks. Any new `cfg(windows)` OS-thread spawn reached from a
  unit-tested fn needs the same seam (`rust-workspace.md`).

## Migration (first start, `audio_out_migrate.rs`)

`vban_enabled` / `vban_stream_name` / `vban_targets` → one entry per target
(the first 8 non-empty ones #210 used), `rate: 48000` (fixed, NEVER
"network"), `int24`, the stream name as #210 put it on the wire
(`wire_stream_name`), `enabled` as #210 was (only `"true"`), named after its
`host:port`. A target #210 could never have resolved is skipped and named.
It runs on the outputs task's first pass, ONLY while `audio_outputs` is
ABSENT and an old key exists, and writes the list with `INSERT OR IGNORE` (a
list a PATCH stored meanwhile wins). A pass whose migration FAILED (a busy
or unreadable database) tries it again on the next pass (5 s), never only
at the next restart (`a_failed_migration_is_tried_again_on_the_next_pass`).
A target is read as #210's `ToSocketAddrs` read it (`split_target`: the
whole target trimmed, nothing next to the colon), so "h : 1" is skipped,
never migrated as h:1. **The `vban_*` keys are KEPT** (main-session
ruling 4, 7.10.2026): the new code ignores them once the list exists, and a
rollback to ≤ 0.72 still finds them, so FOH keeps its sound. A later lane
deletes them (and `sp_core::config::SETTING_VBAN_*`) once the list has run a
main release (lane 3 kept them: lane 1 had not been in a main release yet).
A `vban_*` change made on a rolled-back ≤ 0.72 is NOT carried forward when
the list version comes back (the list exists then).

## The fan-out (`audio_out.rs`)

`ProgramOutput::serve` → `split` → `limit` → `feed_outputs` (ONE `Arc<[f32]>`
copy of the limited block, `ProgramBlock`, `audio_out_block.rs`) → MAX →
video side + NDI submit. `AudioOutputs` (owned by `ProgramBus`,
`outputs()`) holds the running list as one `Arc` snapshot swapped whole by
the task; `offer` pushes into every running output's own drop-oldest queue
and never waits. Every output's queue is ONE type,
`audio_out_queue::BlockQueue` (a `VbanOut`'s and an `AsioOut`'s; #210's
VBAN queue moved there in lane 3's review round 1, `VbanTake` re-exports its
`Take`), and `audio_out_queue::lock` is the outputs' one poison-tolerant
lock helper. Pinned:
`program_output_tests_order.rs::every_output_has_the_block_before_the_ndi_submit`
and `audio_out_tests.rs` (no cross-output drops). `ProgramOutput::with_vban`
is a `#[cfg(test)]` shim over `AudioOutputs::single_vban` (#210's tests).

## VBAN per destination (`vban_out.rs`, `vban_packet.rs`, `vban_rate.rs`)

- `VbanFormat`: SR index (48k=3, 96k=4, 192k=5, 44.1k=16, 88.2k=17), sample
  type (INT16 0x01, INT24 0x02, FLOAT32 0x04), packets = the largest divisor
  of `rate/30` within 256 frames and 1436 payload bytes (96k INT24: 16 × 200,
  one every 1/480 s; 192k INT16: 25 × 256; 48k FLOAT32: 10 × 160). Packet k
  at `due + L + delay + k·slot/packets`, floored.
- 48 kHz INT24 = `VbanFormat::PROGRAM`: no converter, #210's bytes — pinned
  against a verbatim copy of the 0.72.0 encoder (`vban_packet_tests_legacy.rs`,
  `vban_out_tests_dest.rs::a_migrated_foh_entry_sends_the_0_72_datagrams`).
- Other rates: rubato `Fft` with `FixedSync::Both` (1600 in, `rate/30` out;
  5.0.1 takes the whole block as ONE FFT, `fft_chunks = chunk / min_in`),
  delay `rate/60` frames = 16.7 ms at every rate (`fft_delay_frames`). A
  silent or malformed block goes through the filter as zeros.
- The delay: the sender's send latency is `L + delay`, and its wait cap is
  `VBAN_MAX_WAIT_100NS` (4 × L = 8 slots) PLUS the delay (`plan_wait_up_to`;
  the 8-slot cap alone sent a delayed output's first packet early). That
  wait is slept in steps of at most 7 slots (`VBAN_SLEEP_STEP_100NS`,
  `sleep_until`, at most `VBAN_WAIT_STEPS` = 10), the clock read between two
  of them and each next step planned from that read: a step plus an
  oversleep of under a slot passes at most 8 boundaries, the wall's tick cap
  per read (`BoundaryTicker`; an 8-slot step could pass a 9th, review round
  2), and an oversleep never adds up. A FOH wait (≤ L) is one sleep, as in
  #210
  (`the_longest_delay_is_waited_for_whole_in_sleeps_the_wall_can_tick`,
  `a_wait_is_slept_in_steps_the_wall_can_tick`). The queue bound grows with
  the delay (`queue_bound`: the program queue's 10 + `ceil(delay / slot)`).
- A converter rubato refuses (none of the supported rates) sends silence
  and logs one WARN (`VbanSender::new` reads `failed()`).
- One `VbanOut` per entry, ONE target (`VbanConfig::for_dest`,
  `resolve_dest`); `VbanStallLog`'s buckets count packets, so at 96 kHz
  `late_max_us` covers 30–60 s (ruling 11).

## Telemetry

`GET /api/v1/program` → `outputs[]` `{id, type, name, enabled, state
(running|opening|waiting|disabled, `vban_state`; a thread that could not
start — Windows: the UDP bind or the spawn failed — is "waiting" with
`VbanOut::start_error` as its reason, never "opening" for good, and is
rebuilt — retried — on the outputs task's next pass, `RunningOutput::start_failed`), reason, rate, format,
channels, delay_ms, latency_ms (L + delay + the converter,
`vban_latency_ms`), blocks_sent, blocks_dropped, vban: {#210's VbanStatus +
blocks_sent}}`, `audio_network_rate`, `outputs_problems`; the cut answer
carries them too. The top-level `vban` is gone (ruling 5). Its readers were
the mock, `settings-vban.spec.ts` (deleted), `program_tests_max.rs` (now
reads `outputs`) and the MAX box gate in `gpu-max.md` (now FOH's
`outputs[i].vban`).

## Dashboard (`sp-ui` `audio_outputs.rs`)

Nastavenia "Zvukové výstupy", after the main form: its own PATCH of only the
two keys (`patch_json_empty`), validated with
`sp_core::audio_outputs::validate_list` and shown as `ListError::sk()`
before anything is sent; rows keyed by id, every cell reading the list by
id; each row's live state from `outputs[]` (polled every 2 s). The main form
MERGES its save into `store.settings` (replacing it blanked the list:
Review Focus 1, `settings-audio-outputs.spec.ts`). Each section re-reads
only ITS keys through a `Memo` (the section the two output keys, the form
every other key), so neither section's save resets the other's unsaved
edits. Every `<option>` of a select carries a reactive `selected`: tachys
sets `prop:value` BEFORE the options mount, so a row read back from the
store showed its select's FIRST option ("podľa siete", "16 bitov") until
#233 review round 1 (the playlist picker's pattern). Nothing is saved
before the Settings page LOADED the settings (`loaded`: `None` while
`GET /api/v1/settings` runs, `Some(false)` when it failed, passed to both
sections): an empty list shown before the load would replace the stored one
(FOH's entry with it), and the form's fields would hold defaults; a failed
load shows "Nastavenia sa nenačítali — …" on both. The same holds over a
stored list this dashboard cannot read (`load_error`, the rows reset to
none). Each save is stopped twice: by its disabled button AND by its
handler's own guard (`loaded` / `load_error`), the specs reaching the
guards past the buttons (`sp-ui-frontend.md`). The message span is
`audio-outputs-status` (`.save-status` is the form's alone: five Nastavenia
specs read it unscoped, Playwright strict mode). A stored entry missing from
`outputs[]` reads "uložený, nespustený" with its `outputs_problems` line as
the tooltip, marked "Hlásenie servera: …" (an unsaved one "neuložený"); a new row's id is above every
row AND every stored entry (a removed, unsaved row still runs under its id).
`style.css` gives the section the form's fieldset look, one framed grid
block per output. The mock refuses the cases the dashboard can send in the
server's order and words (every entry read first, then the counts, the host,
the port and duplicates; the id through `shown_id`'s rule) — a SUBSET of
the server's checks, never a stand-in for its tests, serves `outputs[]`
from the stored list with a `vban` object per enabled entry, and has two
knobs: `/__mock/fail-mode {kind: "settings"}` and `/__mock/outputs-skip
{ids}` (both reset by `/__mock/settings-reset`).

## Live gates (`e2e/post-deploy-audio-outputs.spec.ts`)

FOH (`fohabl.lan:6980`) still 48 kHz INT24 `sp-program`, no delay, ≥ 25
blocks between two reads, no send errors (`audio-outputs-gate.ts`, unit-
tested in the mock suite), polled for first (after a restart FOH is listed
only once the outputs task's first pass ran); a temporary `e2e-96k` entry to a UDP receiver on
127.0.0.1 reads index 4, 200-frame INT24 packets, a contiguous counter
(480/s); `finally` restores the list stored NOW (an operator may have
saved one meanwhile) without the probe. The probe needs a STORED list first:
with none its PATCH would store one and the migration (only while no list
is stored) would never run, FOH off for good. Its loopback receiver takes a
4 MiB buffer. The file records no trace (it reads the settings). PP's
subset does not run it (PP has no FOH entry).

## ASIO outputs (`asio_out.rs`, `asio_win.rs`, `asio_format.rs`, `asio_state.rs`) — lane 3

**The entry.** `type: "asio"`, `asio: {driver, channels: [left, right]}`,
0-based (the dashboard shows them 1-based through
`sp_core::audio_outputs::{asio_channel_index, asio_channel_shown}`,
wrapping: a typed 0 is stored as u32::MAX, refused "musí byť 1 až 512" and
shown as 0 again, never read as channel 1). The driver is 1..=128
characters with no control character; the channels are ≤ 511 and different.
At most 4 ASIO entries (`MAX_ASIO_OUTPUTS`; the per-type caps sum under 16,
the const assert). ONE entry per driver, a switched-off one included (DVS
takes one ASIO client): `validate_list` / the stored read refuse a second
(`driver_taken`, "pole „ovládač“ je už použité iným výstupom ASIO"). The
entry's `rate` is kept as stored and unused: `build_rate` is 0, so a
network-rate change never rebuilds an ASIO output.

**azo 0.4.0** (main-session ruling 10, `default-features = false`: no
`oneshot` host): `driver::Metadata::enumerate` reads HKLM\SOFTWARE\ASIO
(no COM, `list_drivers`); `driver::SafeHandle::new(&clsid)` initialises COM
as an STA on the calling thread and uninitialises it in its own `Drop`,
which runs BEFORE its interface field is released (`!Send`: it lives on the
output's worker). So the device holds the thread's STA itself
(`asio_win::ComApartment`, its last field, from `new` until it is dropped):
every driver release happens inside the device's apartment (review round
1; azo's own host does the same, `host.rs:105-111`; azo 0.2.1's InitGuard,
which iemmixer ran, dropped the driver first). A failed `SafeHandle::new`
leaks azo's own init count; inside the device's STA that init cannot have
failed, so `open` balances it. `use azo::driver::Driver` for the methods;
`dispose_buffers`. azo-sys 0.3.2: the sample-type codes (Int16/24/32LSB
16-18, Float32LSB 19, Int32LSB16/18/20/24 24-27), `Callbacks` = four
context-less `unsafe extern "system"` fn pointers. azo-sys needs bitflags
≥ 2.13.2, so the lock moved bitflags 2.11.1 → 2.13.2.

**The worker** (`asio_out.rs`, `AsioWorker::step`, one thread per entry,
`run_asio_worker`, NOT an MMCSS thread: it must never pre-empt the driver's
own callback thread and has two slots of cushion):

- closed: open when the backoff allows (2 / 10 / 30 / 60 / 60 … s, a run of
  60 s resets it, the count saturates); the driver's rate (admitted 8–384 kHz),
  preferred buffer, output channels and one sample type are READ (the
  `AsioDevice` trait has no setter); a new servo, `Asrc` and `Splice` for
  that rate; a ring of target + 4 slots + one block; start. A failed open
  releases the driver and shows its reason.
- running: each block → `Servo::observe` (the ring + the splice's 5 ms hold,
  the splice's pending skip, the hand-off lateness, the card's consumed
  frames) → `Asrc::set_correction_ppm` + the re-centre through
  `frames_from_100ns` into `Splice::insert` / `skip` (an `Ordering` match:
  `if > 0` / `else if < 0` would give a `>=` mutant that inserts 0, an
  equivalent survivor) → the ring. A closed output drops the blocks it is
  handed.
- open drops the blocks queued while the driver opened (stale by the open's
  duration; an INFO, never `blocks_dropped`), so the servo's first block is
  fresh.
- close: a reset request or a buffer-size change (answered 0: never
  resized live), a rate change of 1 Hz or more (`sampleRateDidChange(0)` =
  a lost clock, code `clock_lost`, "ovládač stratil hodinový signál": a
  slot marks a report with its own flag, since 0.0's bits are 0), or 2 s
  with no callback (`StallWatch`). "waiting", the reason and the retry are
  published BEFORE the device is closed (a vanished driver can block its
  stop or release; review round 2), and the closed run's ppm, rate_ppm,
  lock and latency are cleared (the counters and the last open's driver
  rate / sample type stay). EVERY counter (underruns, overloads,
  overflows, re-centres, resets) counts since the output was built: a
  run's device, servo and ring count from 0 again, so the worker adds each
  closed run's into one `Closed` (review round 3). `kAsioLatenciesChanged`
  re-reads the driver's output latency
  (`AsioDevice::output_latency_frames`), so `latency_ms` follows it.
- a lost clock stays a lost clock: ONE predicate, `asio_state::lost_clock`
  (under 1 Hz), names a rate report (`close_reason`, through `rate_change`)
  and a read (`admit_rate`), and `getSampleRate`'s ASE_NoClock is one too
  (`sample_rate_error`, pure; the glue only calls it): a reopen during the
  outage reads `clock_lost` again, never a refused rate or "the rate
  changed to 1 Hz" (review rounds 3–4). Its text: "the driver lost its
  clock (no rate)".
- a close publishes the run's counters with it (`Closed::write`): a driver
  that never comes back still shows what the output did.
- the backoff runs from the step that decided the close, not from the end
  of the release: a release that blocks for seconds reopens right after
  it; a driver still busy then is refused `busy` and the next wait is
  10 s. Kept (the worker step takes one instant; a busy driver costs one
  retry).
- one holder per driver in the process (`asio_hold`, `asio_win::HELD`): a
  device holds the driver's name before it reads the registry or loads
  anything, and gives it back after the driver is released. A rebuilt
  entry's successor starts while its predecessor still releases the same
  driver (`apply` starts the new output before it discards the old), so it
  is refused `Reason::Held` (code `held`, "predchádzajúci výstup ešte
  neuvoľnil ovládač") and opens after the 2 s backoff: never two
  instances of one driver in one process (review round 1; before, it
  waited only if the driver happened to refuse the second init).
- the process exit does not wait for the release: the box stops SongPlayer
  with `taskkill /F` (every deploy, the scheduled task), which no
  in-process join could cover, so the driver is torn down with the process
  and the next start opens it like any other open (busy → the backoff). Box
  evidence for DVS: the MAIN SESSION OPS read the entry after the deploy
  that follows its addition.
- every open, failed open and close is logged with the reason and the retry.

**The glue** (`asio_win.rs`, `#[cfg(windows)]`, excluded in
`.cargo/mutants.toml`): 8 static callback slots, two per entry (a replaced
output's old worker may still hold its slot while its successor starts).
The buffer switch pops its frames from the `rtrb` ring into a scratch
allocated at start, writes L/R into the two configured channels in the
driver's type and zeroes every other channel and the frames the ring did not
deliver (`asio_format::fill_channel`), counts an underrun only once primed,
and only counts in atomics — no allocation, lock, log or syscall. Messages
are counted and answered by `asio_state::reply` (iemmixer's table). `close`
stops a STARTED driver (a driver whose start never ran is not stopped),
unhooks the stream, frees it only once no callback is inside it, disposes
the buffers, drops the driver (inside the device's COM apartment), gives
the driver's hold back, THEN releases the slot. Its wait for the in-flight
callback PUMPS the thread's messages (a driver may need them to finish a
callback; iemmixer `asio.rs:484-504`, review round 5; the bound is 1 s,
iemmixer's 2 s). A callback still inside after that second PARKS the
device (iemmixer `asio.rs:487-500`, review round 2):
the stream is leaked, the buffers are not disposed, the driver is
forgotten (never released), the slot stays claimed (its in-flight count
is the callback's) and the hold is kept for the process's life, marked
parked (`DriverHold::park`, `DriverHolds::is_parked`); every later open of
that driver — this device's, or the output that replaces it — is
`Reason::Parked` (code `parked`, "ovládač zamrzol — pomôže len reštart
SongPlayera"), never retried: the worker keeps the output closed for good
(`NEVER`), `retry_in_s` is None and the WARN is logged once (review round
5). It parks only when no read of the in-flight count saw 0 within the
bound (once 0 was read, the unhooked slot gives a later callback no
stream). Known limit (iemmixer's too): a driver whose `stop()` never
returns (its callback stuck) hangs the worker before the park check; the
row stays at its last waiting reason with a frozen "ďalší pokus o N s", an
edited entry's successor reads `held`, and only a restart of SongPlayer
helps. Two more edges, kept (review round 6): a device that parks INSIDE a
close shows the close's normal countdown until its next open answers
`Parked` (at most the backoff, ≤ 60 s); and a parked device's thread pumps
no messages any more (the worker stays closed), so a callback blocked on
this thread for good stays blocked — both inside a state only a restart
ends.
`outputReady` is not called: the plan allows it from the callback, but
one driver buffer (1.3 ms at DVS's 128 frames / 96 kHz) is noise against
the 66.7 ms target, and the call would go through a raw pointer to a
`!Send` STA handle on the driver's thread, a path no CI runner can
exercise (no driver). `poll` pumps the thread's window messages. The rate,
the clock source and the panel are never touched: the Test Integrity job
fails on the names `set_sample_rate` / `set_clock_source` /
`open_control_panel` / azo-sys's raw `control_panel` anywhere under
`crates/` (word boundaries: a method call, a UFCS call or a spaced one).

**Status** (`outputs[i]`): `rate` = the driver's, `format` = its sample type,
`latency_ms` = the servo's + the resampler's + the driver's output latency
(66.625 + 4 = 70.625 ms on the scripted 96 kHz / 128-frame driver; 0 until
the servo measured its first 1 s window), `asio`
= `{driver, channels, driver_rate, buffer_frames, out_channels,
sample_type, ppm, rate_ppm, locked, latency_ms, underruns, resets,
recentres, overflows, overloads, retry_in_s, reason_code}`, and while it runs a `note`
when the driver's rate is not `audio_network_rate` or its buffer is over a
third of a grid slot (1/90 s, lane 2's envelope); the task WARNs a note once
per change. Off Windows an ASIO output never opens: "ASIO runs on Windows
only" (`start_output_thread` → `start_asio_thread`). The task's
`StartThread` takes the built `OutputSink`, so a unit test on the Windows job
starts no real worker. `GET /api/v1/audio/asio-drivers` = `{drivers: […]}`
(the registry, on the blocking pool; empty off Windows).

**Dashboard.** "Pridať výstup ASIO" (waits for the driver list) adds an ASIO
row on the first listed driver, channels 1 / 2. Each row shows its type and
only its own fields (`<Show>` on a `Memo` of the type); a stored driver the
box does not list stays, marked "(nenájdený)" only once the list was READ
(`sp_core::audio_outputs::asio_driver_options`: an unknown list — still
loading, or its GET failed — marks nothing, #225's "claim only what you
were told"); "Pridať výstup ASIO" is off while the list loads, after a
failed read ("Zoznam ovládačov ASIO sa nenačítal", mock: `/__mock/fail-mode
{kind: "asio-drivers"}`) and on a box that lists no driver ("V systéme nie
je žiadny ovládač ASIO"), its title from
`sp_core::audio_outputs::asio_add_refusal`; the rate reads "podľa
ovládača". A
running ASIO row reads "beží · 71 ms · +0.4 ppm · výpadky 0", "meria sa" in
place of the latency while the server reads it 0
(`sp_core::audio_outputs::asio_running_text`); a waiting one its reason in
Slovak (`asio_reason_sk`, by the server's `reason_code`; every
`Reason::code` has its text, pinned on the server) and its next try, none
for `parked` (`asio_waiting_text`). Mock: two drivers (or a 500, the
fail-mode above), the server's ASIO refusals, an enabled ASIO entry running
at the network rate, `/__mock/asio-state {id, reason_code, reason,
retry_in_s}` and `/__mock/asio-measuring {ids}` (latency 0), both reset by
`/__mock/settings-reset`.

**Tests.** The worker over a scripted `FakeDevice` (`asio_out_fake.rs`);
every exact pin (the 5 436-frame ring after the card's callbacks, 8 636 after
the first block, the 82 223-frame overflow of a block stamped a second ahead,
133 underruns, the busy retries at 0 / 2 / 12 / 42 / 102 / 162 s, two
overflowing runs reading 82 223 / 1 then 164 446 / 2, the 61-slot stall)
comes from a scratch model of the worker over lane 2's servo and rubato
models. The
servo's own latency figure is 66.625 ms whether the held frames are added or
subtracted: only the RING pin sees that mutant. The glue runs on the Windows
job (`asio_win_tests.rs`: the copy-only buffer switch over heap buffers,
every slot's four callbacks, messages, a 0 Hz report, a closed device
releasing its slot, a held driver refused before the registry is read, a
device holding its thread's STA until it is dropped, a callback stuck
past 1 s parking the device, its successor told the driver is parked,
`close` pumping the thread's messages while a callback finishes — a
"callback" a 10 ms thread timer's proc lets leave).
`asio_hold_tests.rs`
pins the holds on Linux.

**Live gate** (`e2e/post-deploy-audio-asio.spec.ts`, SNV's suite and PP's
subset): DVS is registered; exactly `SP_ASIO_OUTPUTS_EXPECTED` enabled ASIO
outputs (ci.yml / deploy-pp.yml, "0" until the main session adds each box's
DVS entry, then "1"), each running, then a minute (1 800 blocks) at its
driver's rate with no underrun, no reopen, |ppm| ≤ 300 and a latency
over 0 and under the entry's delay + 1 s (`asioGateFailures`, unit-tested
in the mock suite; every output over the SAME minute, so the gate's time
does not grow with the outputs; the driver's `overloads` are logged with
the two reads, not gated: a driver's CPU-overload report is the box's
load, not the output's fault, and the gate fails only on what the output
does — underruns, reopens, the correction, the latency; `rate` vs `driver_rate` is a consistency check of two
fields the server fills from the same driver read). A failed read of
`GET /api/v1/program` never counts as zero outputs.

**Notices.** rtrb 0.4.0 (MIT OR Apache-2.0; its LICENSE-MIT has no copyright
line, its Cargo.toml names the authors), azo 0.4.0 and azo-sys 0.3.2 (MIT,
identical LICENSE files) are in `THIRD-PARTY-NOTICES.txt`, each pinned
against `Cargo.toml` by a test in `asio_out_tests.rs`.

**Still open for a closing lane:** the `vban_*` keys and
`sp_core::config::SETTING_VBAN_*` stay until the list has run one main
release (ruling 4), then go.
