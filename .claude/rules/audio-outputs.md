---
paths:
  - "crates/sp-server/src/playback/asrc*.rs"
  - "src-tauri/resources/THIRD-PARTY-NOTICES.txt"
---

# Program audio outputs (#233)

Spec `docs/superpowers/specs/2026-10-07-audio-outputs-asio-design.md`, plan
`docs/superpowers/plans/2026-10-07-audio-outputs-asio.md` (three lanes; lane
1: the output list + VBAN per destination; lane 3: the ASIO output over
`azo`). This file starts with lane 2; lanes 1 and 3 add their sections.

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
share counts); clamp ±300, slew ≤ 5 ppm per second of wall (dt = the 100 ns
between applied windows).

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
otherwise stay open the step + 1 s). A rate point more than 10 ms off the fit (once the
fit has 30 points) RE-BASES the regression (the offset absorbs the step, the
slope stays) and the NEXT point REALIGNS onto the moved line whatever its
residual (`Offered::Realigned`): the same straddle splits a step across two
window means, and a remainder under 10 ms would otherwise sit in the
regression as a level shift (~17 ppm of rate bias for a span). Before the
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
