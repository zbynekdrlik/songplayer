---
paths:
  - "crates/sp-core/src/genlock*.rs"
  - "crates/sp-core/src/clock_health*.rs"
  - "crates/sp-server/src/playback/wallclock*.rs"
  - "crates/sp-server/src/playback/clock_health*.rs"
  - "crates/sp-server/src/playback/pacer*.rs"
  - "crates/sp-server/src/playback/audio_grid*.rs"
  - "crates/sp-server/src/playback/submitter*.rs"
  - "crates/sp-server/src/playback/proc_mem*.rs"
  - "crates/sp-server/src/playback/loop_stats.rs"
  - "crates/sp-server/src/playback/frame_buf.rs"
  - "crates/sp-decoder/src/frame_pool.rs"
  - "crates/sp-server/src/process_residency*.rs"
  - "crates/sp-ndi/src/**"
---
# Genlock (NDI outputs locked to the fleet clock) — #146–#151

- Normative contract: zbynekdrlik/camera-box#1294 (§1–§8). Reference math
  + 68 test vectors: camera-box `src/ndi.rs`, `src/genlock_stamp.rs`,
  `src/genlock_pacing.rs` — ported 1:1 into `sp_core::genlock` tests.
- Timecodes are UTC in **100 ns units since the Unix epoch**; video =
  `floor_boundary_100ns` on the fixed grid (`GENLOCK_GRID_FPS`), FLOOR never
  ceil; audio = raw wall clock, never snapped; `SYNTHESIZE` only on the
  standby black frame.
- `clock_ok = is_locked && mode ∈ {LOCK, NANO}` from dantesync
  `127.0.0.1:8898/status`; unreachable → `no dantesync`, never blocks playback.
- Acceptance is on the RECEIVER (`genlock-fifo audit … locked=1`, camera-box
  #1300), never our own counters. Open questions go to camera-box#1294.
- Pacing (#147) runs on the exact-rational 100 ns grid: sleep target from
  `strict_next_boundary_100ns`, stamp = the serviced boundary, never
  `floor(now)` at emission, never a stamp > the wall read before the send.
  The ns gate `genlock_emit_gate` + its 43 vectors are a reference port of
  camera-box's DECIMATOR — never use an epoch-multiple ns grid as a clock
  (it drifts 10 ns/s against the second-anchored stamp grid). Flag
  `genlock_pacing` (DB setting) is read at pipeline spawn → a flip needs a
  SongPlayer restart (= a deploy). Box test 2026-09-13 01:43: idle/paused
  outputs hold 30/s, but a PLAYING output ran ~27/s with every frame late
  (p99 15 s, max 40 s) — read #147 before flipping the flag again.
- Playback ≠ capture (lane 3, 0.47.0-dev.13): catch-up advances the serviced
  boundary ONE slot per `service()` call, but each emitting call also costs one
  decoder `pull`; when a file's per-frame `iter_cost >= interval` the boundary
  can never gain on the wall clock, so lag (and the negative stamp skew) grows
  without bound. camera-box's A5.6 "buffered never resyncs" is a CAPTURE-side
  rule (a live grabber can't outrun the wall clock) — for FILE playback lag is
  bounded by a wall RE-ANCHOR: lag > `GENLOCK_MAX_CATCHUP_INTERVALS` sustained
  > `LAG_REANCHOR_AFTER_100NS` (1 s) with a frame buffered re-anchors
  `wall_start` so the buffered frame is due at the next boundary (content
  continues, no skip; `resyncs += 1`, `ServiceOutcome::Reanchored`). The video
  then plays slightly slow but the stamps stay near `now` (FIFO stays locked).
  Health gauges: `pacing.lag_slots` (whole slots behind at the last emit) and
  `pacing.iter_p99_us` (decode+submit cost; `>= interval` = decoder can't keep
  up) are the honest signals; `jitter_p99_us` only mirrored the lag.
  `late_frames` counts an emit > 2 ms past its boundary.
- Producer/consumer decode split (#147, 0.51.0-dev.2, box test 4→5): a
  ONE-frame synchronous look-ahead inside `Pacer::prepare` on the emit thread
  could NOT hold the grid while the #162 stems child was resident (1440p decode
  p99 93–111 ms > the 41.7 ms slot → 85 % late, re-anchor storm). The paced path
  is now producer/consumer: a dedicated DECODE thread (`pipeline_paced.rs::run_decode_producer`)
  owns the whole MF decoder and fills a BOUNDED look-ahead queue (`playback/pacer_queue.rs`
  — pure `PacedQueue` + a `Mutex`/`Condvar` `SharedQueue`, bound 12 ≈ 500 ms);
  the emit thread only POPS at the boundary (`prepare(target, || queue.consumer_pop())`),
  so `Pacer::prepare`/`service` are unchanged. **COM STA is free**: `MediaFoundationVideoReader::open`
  calls `CoInitializeEx(APARTMENTTHREADED)+MFStartup` on WHATEVER thread calls it,
  so the producer thread self-inits its apartment — just keep open/decode/seek/drop
  ALL on the producer thread (never split the decoder across threads). Seek is
  routed to the producer via `request_seek` (flushes the queue + bumps a seek
  EPOCH so in-flight pre-seek frames are dropped). `pacer_queue.rs` is
  cross-platform + Linux-tested + mutation-scored; `pipeline_paced.rs` is
  Windows-only + mutation-excluded (box-verified).
- Box test 5 (0.51.0-dev.2, stems child RESIDENT 3.96 GB): the split SOLVED the
  decode contention — `resyncs 0`, `lag 0`, `prep_p99 0.4 ms`, seq at the full
  30/s grid, NO re-anchors (box test 4 had `resyncs +8`/30 s, `lag 12…23`). But
  `late_frames` stayed 27.6 % with `iter_p99 81 ms` while `prep_p99` is 0.4 ms —
  the bottleneck MOVED from the decode to the **NDI SUBMIT** (`send_video_async` +
  audio still on the emit thread, stalling under the child's memory-bandwidth /
  page-fault pressure). That led to the dedicated submit thread (#168, next
  bullet). Pacing is ON in production permanently (owner ruling, #147 comment
  5812898277). `iter_p99` ≫ `prep_p99` is the signature of submit-side (not
  decode-side) lateness.
- Submit-thread output split (#168, 0.54.0-dev.1): the symmetric twin of the
  #147 decode split, moving the NDI submit OFF the emit thread. Diagnosis
  (box-test-5 log, `paced: song summary`): `iter_p50_us` 25–31 ms MEDIAN with
  `iter_p99_us` 87–93 ms while `prep_p99` 0.4 ms — a 25 ms MEDIAN cannot be the
  ~sub-ms NV12 `to_vec()` copy, so the stall is `send_video_async` blocking on
  the prior async frame (the SDK send thread starved by the resident child), NOT
  the copy → the submit-thread fix, never a pre-converted pool. The emit thread
  now emits through `HandoffSink` (`pipeline_paced_submit.rs`): it hands the
  stamped frame to a BOUNDED handoff (`submit_handoff.rs::SubmitQueue`, depth
  `SUBMIT_HANDOFF_BOUND=2`) in ~µs and stays on the grid; a dedicated submit
  thread (`run_submit_consumer`) owns the `FrameSubmitter` for the song
  (borrowed via `std::thread::scope` — SDK per-instance affinity + the async
  double-buffer holdover stay single-threaded) and does the blocking
  `send_audio`+`send_video_async`. It works BECAUSE median submit (25 ms) < the
  33.3 ms grid slot: the submit thread's ~40 fps capacity vs 30 fps demand drains
  the p99 spikes out of a shallow queue. `late_frames` is measured HONESTLY at
  the submit thread (`submit_late_100ns` = stamp → submit-start, floored),
  never at the handoff; a full handoff COALESCES to the freshest stamp
  (`handoff_policy` — drop the stalest unsent job, count a submit-side `dropped`).
  The health doc's `PacingStats` is `merge_pacing_stats`: late/max_late/iter_p99/
  dropped from the submit thread, seq/repeats/resyncs/relatches/lag/prep from the
  pacer (`/api/v1/ndi/health` shape unchanged); the paced heartbeat reads a
  submit-side snapshot (`emit_heartbeat_paced`) since the submit thread owns the
  submitter. The pure decisions (`submit_handoff.rs`) are Linux-tested +
  mutation-scored; the `SharedHandoff`/consumer glue (`pipeline_paced_submit.rs`,
  `#[cfg(windows)]`) is `mutants::skip`, box-verified. The NON-paced
  `pipeline::decode_and_send` path is untouched. Acceptance = box test 6
  (`genlock_pacing=true`, stems child resident, 60 s): `late_frames` < 1 % of
  `seq`, `resyncs`/`dropped`/`audio.underruns` 0, `lock_state=LOCKED`; then the
  flag stays ON and camera-box#1302 gets the receiver verdict.
- Submit-call cost gauge on the paced path (#168 round 2, 0.63.0-dev.1): box
  test 6 FAILED (17.9.: 94.5 % late, `iter_p99` 75.8 ms) because the NDI SDK
  `send_video_async` itself costs ~25 ms median / 75 ms p99 per 2560×1440 frame
  with the stems child resident (the 20.9. event showed up to 954 ms) — the
  decode + emit are off the critical path, the remaining wall is the SDK call.
  To decide (SDK cost intrinsic at 1440p → 1080p-genlocked vs 1440p-unlocked, or
  load-induced → policy) this round PLUMBS the per-frame SDK cost onto the paced
  heartbeat, plumbing only, no behaviour change. The submit thread already timed
  every `send_video_async` into `FrameSubmitter.submit_times` (the round-3
  `loop_stats::SubmitHist`) but never surfaced it; now `run_submit_consumer`
  drains that SAME histogram on its ~1 s connection-poll cadence and folds the
  `(max, p99)` worst-of into the handoff snapshot (pure
  `submit_handoff::paced_submit_snapshot` + `PacedSubmitStats`), the heartbeat
  `snapshot()` drains-and-resets it per window, and `emit_heartbeat_paced`
  carries it BOTH into `PacingStats` (`submit_call_us_max`/`submit_call_us_p99`,
  on `/api/v1/ndi/health` `pacing`) AND into the existing `pipeline: loop-stats`
  log line (the SAME field names the SDK-clocked path uses). **Box test 7 reads
  these:** with `genlock_pacing=true` + a stems child resident, the per-minute
  `pipeline: loop-stats … submit_call_us_max=… submit_call_us_p99=…` line (and
  the `/api/v1/ndi/health` `pacing` block) now names the paced per-frame SDK
  submit cost, cross-read against `late %`; then the stems worker OFF for the
  A/B. The `ndi: genlock` line was NOT touched (ndi_health.rs is at the 1000
  cap); the number rides the loop-stats line + the health API instead.
- Burn-id QR overlay (#151, run_id **911014**): the paced emit paints a QR of
  `P{run_id}.{frame_id}.{gen_ts_ns}.{crc32}` bottom-right (side `0.28·h`, margin
  `40/1080·h` — camera-box `payload.rs` + `burn-geom.hpp`, ported into
  `sp_core::genlock::burn`; luma-only 16/235, chroma neutral 128) so the fleet's
  `recording-verdict` proves contiguity for SP-originated frames.
- Dashboard genlock indicator (#150→#164→#176): the header `GlobalLockBadge`
  (`sp-ui/src/components/ndi_health.rs`) is **ALWAYS visible** — grey
  `● GENLOCK OFF` when NO output has pacing enabled (the CODE default
  `genlock_pacing=false`; production runs pacing ON per the #147 ruling), else
  `● LOCKED`/`● DEGRADED`/`● UNLOCKED` (green/amber/red, `n/m` live-locked
  count + worst reason). #176 revised #164's "hide the
  badge entirely while pacing is off" — the owner must always be able to tell at
  a glance whether SongPlayer is genlocked, like the fleet OBS badge. The
  whole-box state is decided by the ONE pure, unit-tested
  `sp_core::genlock::lock_state::global_lock_summary` (`GlobalLock::Off|Locked|
  Degraded|Unlocked`; sp-ui is outside the workspace = no unit-test job, so the
  logic + tests live in sp-core and sp-ui calls it). The PER-CARD `LockBadge`
  keeps #164's "only where actionable" rule (hidden while pacing off). `sp_core::
  genlock::lock_state::summarize` stays a 3-state (no OFF) reference. Testid
  `genlock-global-badge`.
- The burn is **default OFF, NEVER persisted, paced-path ONLY**: toggled per
  output via `POST /api/v1/ndi/burn {output,on}` (204 / 404 / 409 "pacing
  disabled"), read fresh every boundary through a shared `Arc<AtomicBool>`
  (`NdiBurnRegistry`), surfaced as `burn_on` in `/api/v1/ndi/health`. A QR must
  never reach the LED wall in production — a structural guard keeps the legacy
  `decode_and_send` path from ever referencing the overlay.
- SDK-clocked wall-clock AUDIO emitter (#192): on the production SDK-clocked
  path (`genlock_pacing=false`) the NDI audio stream is now clocked by the WALL
  CLOCK, independent of the video submits. `run_loop_windows` spawns ONE
  dedicated OS thread per pipeline (`playback/pipeline_audio.rs::spawn_audio_emitter`,
  `THREAD_PRIORITY_TIME_CRITICAL` via `windows-sys`) that emits ONE 1600-sample
  block (48 kHz stereo = 33.333 ms = one grid slot) every wall boundary
  (`WallClock` QPC + coarse-sleep-to-2ms + spin, the `pipeline_paced.rs` pattern,
  1 ms `timeBeginPeriod`), stamping the NDI audio timecode from the GRID (a clean
  48 kHz clock — this is what kills the receiver servo's ±1500 ppm "rate swings",
  NOT a receiver bug). WHY it exists: `decode_and_send` used to submit audio only
  alongside each video frame, so a song transition (`video ended naturally` →
  next `Play`, 200–400 ms) or a heavy-child model-load stall left the audio
  stream SILENT → the genlock OBS's 3 ms-budget ASRC servo read the ≥1-block hole
  as starvation → buffer collapse + re-lock ate ~1 s of the next song. Now
  `decode_and_send` PUSHES decoded audio into the emitter's bounded ring
  (`AudioRing`, ~250 ms, `push_blocking` — bounded, back-pressures the decoder,
  never drops) BEFORE that frame's `send_video`; the emit thread pops a whole
  block per slot or emits a FULL silence block when the ring is short (never a
  partial block, never a skipped slot), so the stream never starves. The pure
  core (ring + grid + telemetry + the generic `emit_one_block` send seam) is in
  `playback/audio_emitter.rs`, cross-platform + Linux-tested + mutation-scored;
  only the thread lifecycle (spawn/priority/sleep-spin/join-before-sender-drop)
  is `pipeline_audio.rs` (`#[cfg(windows)]`, `mutants::skip`, box-verified).
  A/V: audio leads/lags video by at most one block (33 ms) + ring residency
  (~2 blocks) — box-verify it stays inside the DistroAV sync window. Cross-thread
  NDI: video (decode thread) + audio (emit thread) submit on the SAME
  `NdiSender` via `sp_ndi::AudioSink` (`{Arc<B>, handle}`, cloned from the
  submitter) — NDI permits audio+video from separate threads on one instance;
  the emit thread MUST be joined before the sender is destroyed (the
  `AudioEmitterThread` guard is declared AFTER `submitter` so it drops first).
  Telemetry: `AudioStats.emitter {enabled, mode:"sdk-video/wallclock-audio",
  silence_blocks, ring_depth_ms, emit_jitter_p99_us, late_blocks}` on
  `/api/v1/ndi/health` + the dashboard badge tooltip; the emit thread logs a
  per-minute `audio-emitter: heartbeat` line + one line per silence→audio edge
  (`audio resumed after silence`). `silence_blocks` should grow ONLY at
  transitions/stalls; `late_blocks` ≈ 0 and `emit_jitter_p99_us` < 500 with the
  TIME_CRITICAL thread. The PACED path (`pipeline_paced.rs`) keeps its own audio
  (the Pacer's media-aligned `AudioGridBuffer`, see "Paced audio" below) and is
  untouched.
  **Box finding 19.9.2026 (0.59.0-dev.6):** EVERY pipeline runs an emitter (idle
  ones emit silence, so receivers never starve), and with the FIXED 2 ms spin the
  per-minute p99 was 0.4–5.5 ms, not < 0.5 — on the 24-core, ~3 % busy,
  Balanced-plan Win11 box the coarse `thread::sleep` overshoots by several ms
  (parked cores), TIME_CRITICAL or not; SongPlayer used 0.10 cores total, so it
  was NOT spin contention. Cure: `audio_emitter::SpinMargin` (pure, Linux-tested)
  — margin = worst coarse-sleep overshoot of the last 900 slots + 0.5 ms, clamped
  2–6 ms, and ONLY while the pipeline carried audio in the last 300 slots (a
  song transition stays tight; ten silent idle emitters keep the cheap 2 ms).
  `sleep_until` returns the overshoot; the heartbeat logs `spin_margin_us`.
  Re-read p99 + SongPlayer CPU on the box after any change here.
  **Round 2 — the ring needs a CUSHION (box 19.9.2026):** with the decoder's
  plain 40 ms pairing the ring sat at 24–55 ms (FLAC chunks are ~85 ms, so the
  depth saw-tooths to ~0) and any decode hiccup became a 33 ms SILENCE BLOCK
  mid-song on the live output (`audio resumed after silence … silence_blocks=1`:
  3× in 15 min unloaded, ~1/s while a second pipeline decoded) — and every
  underrun shifted audio later for the rest of the song. Cure: the emitter path
  opens the decoder through `pipeline_audio::open_synced_decoder` →
  `SplitSyncedDecoder::with_tolerance(40 + AUDIO_LOOKAHEAD_MS=100)`, i.e. audio
  is read ~100 ms AHEAD of video. That fills the ring to ≈ 98–225 ms (capacity
  266) WITHOUT an A/V offset (the first block starts as the first frame goes
  out; both then run in real time). Three rules ride with the lookahead: a
  PAUSE must `hold_ring` (silence without popping — else the cushion plays out
  past the pause point and audio leads after resume; released by the next
  `push_blocking`), a SEEK and a new playback must `clear_ring` (else stale audio
  queues ahead and audio lags), and the legacy no-emitter fallback keeps the
  plain 40 ms (its audio rides the video frames). A `silence_blocks=1` resume
  line mid-song is ALWAYS a defect — grep the box log for it after any change.
  The heartbeat also logs `emit_call_max_us` (ring lock + NDI `send_audio`) to
  tell an SDK/lock-delayed slot from a late wake-up.
  **Round 3 — release code review (0.59.0, #192 items 1–6):** `AudioRing::push_some`
  now CLEARS + re-fixes the ring layout when a push's `ch` differs from the fixed
  layout (a mono song after a stereo one must not be read through the old frame
  size), and accepts only whole frames of the pushed `ch`; `push_blocking`
  truncates to whole frames and drops a partial-frame residual with one WARN — it
  can no longer loop forever with `accepted==0` while free space exists (the
  odd-length-input 250 ms `wait_timeout` spin). At a NATURAL song end
  `pipeline_audio::drain_if_present` polls the pure `ring_is_drained` (< one block
  buffered) every 5 ms for ≤ 400 ms so the song's tail is emitted before the next
  song's `clear_ring` wipes it (natural-end path ONLY — Stop/Play/Shutdown still
  clear). `AudioEmitter::tick` RE-ANCHORS the grid when this slot's boundary is
  > 1 s behind `now` (counts `resyncs`, logged in the per-minute heartbeat only)
  so a suspend/debugger stall never fires a TIME_CRITICAL catch-up burst;
  `boundary_for` uses a CHECKED `i64::try_from` via the shared `units_for` helper.
  All Linux-unit-tested with exact boundaries; the drain glue stays `mutants::skip`.
  **Round 3 — 1.5 s cushion + stage telemetry (#192, box 20.9.2026):** the owner
  heard audible holes during the service — mid-song `silence_blocks` 9–14 (≈ 300–
  466 ms) on the on-program output while `emit_call_max_us` was only 33–67 ms, i.e.
  PRODUCER stalls (the ONE decode loop stalling ~1 s under a resident stems child),
  NOT the SDK send. Cure: `AUDIO_LOOKAHEAD_MS` 100 → **1500**, and the two derived
  quantities now track it via pure `const fn`s so they can never drift below the
  cushion: `RING_CAPACITY_BLOCKS = ring_capacity_blocks(AUDIO_LOOKAHEAD_MS)` (= 49
  blocks ≈ 1633 ms — a capacity BELOW the lookahead would cap the realised cushion,
  because `push_blocking` back-pressures the decoder at the cap), and the natural-
  end drain deadline = `drain_budget_ms(AUDIO_LOOKAHEAD_MS)` (= lookahead + one
  slot = 1534 ms; a fixed 400 ms would cut the last ~1.1 s of every song at the
  deeper cushion). So a natural song end now blocks the decode thread up to ~1.5 s
  draining the buffered tail (breaks early on `ring_is_drained`). Attribution
  telemetry: `playback/loop_stats.rs` (pure, Linux-tested, mutation-clean) — a
  `SubmitHist` on `FrameSubmitter` times each `send_video_async` (both `submit_nv12`
  and `submit_frame_at_boundary_owned`), surfaced via `WindowStats`/`drain_window`,
  and a `LoopStageMax` in `decode_and_send` times the decode / submit / audio stage
  of each loop iteration; both ride the `HealthSnapshot` event and log a third
  grep-stable `pipeline: loop-stats` line beside `ndi: heartbeat` (per UTC minute)
  so the A/B box test (same song ± a resident stems child) names the stalling
  stage. The paced (#168; pacing is ON in production since the #147 ruling) submit-thread also surfaces
  this gauge now (#168 round 2, 0.63.0-dev.1): it drains the SAME
  `FrameSubmitter.submit_times` through its handoff snapshot into
  `emit_heartbeat_paced`, carried into `PacingStats` + the paced `pipeline:
  loop-stats` line — see the "Submit-call cost gauge on the paced path" bullet
  above.
- **Round 4 (`av_catchup.rs`): video follows the wall-clock audio, SDK-clocked
  path only.** The round-3 measurement showed the stall's residue is not a
  submit-call block but a lasting A/V offset (the ring stays ~150 ms for the rest
  of the song). So round 4 drops LATE video frames (audio already queued) until
  the video catches up — `pipeline.rs` gains exactly ONE `if` at the submit site
  (`pipeline_audio::is_late_frame` → pure `CatchUp::step`), counted as
  `catchup_dropped` in the `pipeline: loop-stats` line. This runs ONLY on the
  `genlock_pacing == false` (wall-clock-emitter) branch; the paced/genlock path
  keeps its own re-latch logic and is untouched. Full contract: `pipeline-testability.md`.
- Allocation-free steady state (#203, round 2a): the wall's page-fault storm
  under a resident heavy child was the per-frame `Vec<u8>` alloc/free (VirtualAlloc
  demand-zero faults + VirtualFree TLB shootbacks). The submit holdover is now a
  `playback::frame_buf::SharedFrame` (`Arc<Vec<u8>>`, NEVER `Arc<[u8]>` which
  copies) — `FrameSubmitter.prev_frame: Option<SharedFrame>`, sent via the new
  additive `NdiSender::send_video_async_slice(&[u8])`, so the holdover is a
  refcount hold with ZERO pixel copy. Rules for anyone touching `submitter.rs` /
  `pacer.rs`: the field-order SAFETY note still holds (sender drops before
  `prev_frame`); the paced burn overlay paints via `SharedFrame::make_mut`, which
  FORKS into a pooled copy while the pacer still holds its `last_frame` clone
  (the usual case; in place only when the submit side is the sole owner, which
  is safe — #147 round 10); idle Black is submitted by shared reference
  through `PacedSink::submit_shared` (`Standby::Black{dims, &SharedFrame}`,
  `service_standby` clones the Arc = a refcount bump per idle slot, the idle loop
  owns one black `SharedFrame`); `send_black_bgra` reuses a `black_bgra` buffer
  keyed by size (send is synchronous, so it is reclaimed the instant the call
  returns). The NV12 decoder/handoff/pacer-repeat pool (a cross-crate `sp-decoder`
  change) is round 2b, built on this `SharedFrame` seam. The audio emitter
  (`audio_emitter.rs`) is also allocation-free per slot now: `EmittedBlock` is a
  TAG enum, `pop_block` fills a reused `block_buf`, `samples_for` returns a borrow
  (+ a reusable `silence` block), and the jitter-p99 sort moved OFF the
  TIME_CRITICAL thread into `emitter_stats()` (on-demand, heartbeat cadence).
- **Round 5 (#192): a SEEK must not break the cushion + click-free edges.** A seek
  forwarded a keyframe-aligned video seek, so `MediaFoundationVideoReader` landed
  on the PREVIOUS keyframe (`< target`) and `SplitSyncedDecoder::next_synced`
  delivered those pre-target frames — the SDK-clocked sender paced them out (the
  visible jump-back) and the pairing deadline pinned the ring depth ≈ 0 for the
  REST of the song (audio led the picture). Fix (pure, Linux-tested): `seek`
  records `pending_video_target_ms`; `next_synced` decode-and-DISCARDS video
  frames `< target` (bounded by `MAX_SEEK_DISCARD_FRAMES = 600`, never spins)
  before pairing, so the first delivered frame is `>= pos` and the cushion refills
  like a fresh Play. `pipeline.rs` seek arm + the position report are UNTOUCHED
  (same `decoder.seek(pos)` signature; the first reported ts is already `>= pos`).
  Plus a pure `audio_edge_fade::EdgeFade` (sp-server): a fade-out TAIL (last block
  ramped 1→0) at the audio→silence edge and a fade-in (0→1 over `FADE_IN_BLOCKS=3`)
  at the silence→audio edge, removing the clicks a `clear_ring` / stall / song
  transition otherwise produced. TWO design rules that kept every existing test
  green: (1) shape the samples in `samples_for`, NOT `tick` — `tick`/`Emitted.block`
  /`silence_blocks` stay RAW, so the transition log + the tick-asserting tests are
  untouched; only the SENT samples change. (2) the fade-in fires ONLY on a genuine
  silence→audio edge (`fade_in_pos` starts AT `FADE_IN_BLOCKS`, reset to 0 by
  `on_silence`) — a COLD `samples_for(&Audio)` with no preceding silence stays
  full gain, so the many dev tests that push+tick+`samples_for` and assert exact
  samples keep passing; in production the emit thread always emits silence while
  the ring fills, so the first real audio still fades in. Keep the #203
  allocation-free contract: full-gain audio + plain silence are borrowed straight
  from `block_buf` / the reusable `silence`; only faded slots use EdgeFade`s reused
  `out` scratch, and `samples_for` borrows disjoint fields (`&mut edge_fade` +
  `&ring`) so it stays zero-copy. The FIRST post-audio silence slot is now the tail
  (not zeros) — a test asserting that slot is all-zeros must expect the tail + read
  the SECOND slot for the zero block.

## Allocation-free playing steady state — the recycling frame pool (#203 2b)

The per-frame `VirtualAlloc`/`VirtualFree` churn that stalls the wall under a
resident heavy child is removed by recycling NV12 buffers, NOT by a custom
allocator (3–8 MB blocks take the large-object path in every mainstream malloc,
so the OS fault + TLB-shootdown cost is unchanged):

- `sp_decoder::frame_pool` is the SINGLE recycler: a process-global free-list
  keyed by exact `Vec::capacity()` (one frame resolution = one size class),
  `POOL_CAP_PER_CLASS = 6`, `take(len)`/`recycle(buf)`, and `PooledBuf(Vec<u8>)`
  whose `Drop` recycles. It lives in sp-decoder (no sp-core dep) so BOTH the MF
  reader and sp-server use it. The `mf_reader` fills `frame_pool::take(len)` via
  `extend_from_slice` into retained capacity (no fault after the first frame).
- `sp-server`'s `SharedFrame = Arc<PooledBuf>` is the SINGLE sharing handle. One
  allocation flows the whole playing path by Arc bump: `to_paced_frame` wraps
  once → `PacedFrame.video` → the pacer's `last_frame` starvation repeat →
  `SubmitJob::from_paced` (the handoff, `frame.video.clone()`) → the submitter's
  `prev_frame` holdover. No pixel copy anywhere; the burn overlay's
  `SharedFrame::make_mut` (`Arc::make_mut` → `PooledBuf::clone`, a copy into a
  RECYCLED pool buffer since #147 round 10) is the only cloner and burn is
  default OFF.
- **SDK-holdover safety invariant (unchanged from 2a):** a recycled buffer may
  be reused ONLY after every `Arc` is gone. Recycling fires exactly in
  `PooledBuf::Drop`, which the `Arc` runs only on the LAST holder drop — the
  submitter installs the new `prev_frame` (dropping the old Arc) AFTER the async
  call returns, so the buffer the SDK still points at is never recycled early.
  Keep `FrameSubmitter.sender` declared BEFORE `prev_frame` (field drop order).
- The idle black and the cached BGRA black stay OUT of the pool as takers (built
  from their own buffers, never `take`); the idle black's single end-of-loop
  drop recycling one bounded black buffer is harmless.

## Paced measurement session (#168 round-4 recipe)

How to run a paced grid-stall measurement on win-resolume (the check behind the
#168 default `heavy_cpu_affinity_mask` and #147's production flip). A 90-minute
investigation is now a 10-minute read.

- **The default heavy block is now 3 logical cores** (`e00000` on the 24-core
  box; #168 round 8 — round 7 measured it at receiver `dropped_due` 0.27–0.5/min
  vs 0.9–1.35/min for 4 cores). `f00000` (the round-5 4-core block) is now the
  operator experiment/override, no longer the default.

- **Pacing is ON in production permanently** (owner ruling, issue #147 comment
  5812898277, 24.9.2026): the residual stall is solved with guaranteed priority
  and residency, NEVER by switching pacing off — not as a "temporary state", not
  for a measurement. `genlock_pacing` is read ONLY at startup (`lib.rs::start` →
  `engine.set_genlock_pacing`); a restart = `gh run rerun --job <LATEST Deploy
  job id>` — look the id up each time (`gh run view <run> --json jobs`; a rerun
  mints a NEW job id). The dabing 12–16 kHz single-snapshot E2E assertion that
  used to fail on live content was reworked by #206 (post-deploy E2E: content/
  state-dependent audio assertions, closed — commit b41d06e) into a
  content-matched full-band RMS drop.
- **Change containment mid-session.** `heavy_cpu_cap_pct` /
  `heavy_cpu_affinity_mask` apply at the NEXT child spawn (`refresh_containment`
  per tick), NOT to the running child — so after a settings change, kill the venv
  python (`Stop-Process -Id <pid>`) to force a respawn. That video takes one
  `stem_attempts` + backoff; its already-written segments are intact.
- **The no-child control:** `stem_worker_enabled=false` + `lyrics_worker_enabled=false`.
- **Read the grid per minute** from `C:\ProgramData\SongPlayer\songplayer.<date>.log`:
  `pipeline: loop-stats ndi_name="SP-slow" … submit_call_us_max` (the raw
  `send_video_async` call cost) and `ndi: genlock … late=` (lateness/min).
- **Trust the window only if the child is PRODUCTIVE.** `TotalProcessorTime`
  delta over 6 s must be > 0 — a starved child (a 2-logical-core block, W3) gives
  a false-clean grid because it is doing no work, not because placement is safe.

### MCP-shell traps (learned the hard way, round 4)

- Keep each `mcp__win-resolume__Shell` call ≤ ~12 s (e.g. 2 × 4-s `Get-Counter`
  samples). Longer calls time out and lose the output.
- A detached sampler launched via `Start-Process` NEVER wrote its output file —
  run the sampler inline in the (bounded) shell call instead.
- NEVER `Stop-Process` by a `CommandLine -like '<text>'` filter whose text also
  matches YOUR OWN command — it kills your own shell (exit -1, no output). Target
  the child by `-Id <pid>` read from a prior listing.

### The calibrated LOCKED/DEGRADED rule (#168 round 6, #149 classifier)

`sp_core::genlock::lock_state::derive` no longer degrades on ANY late/repeat in
the 60 s window (the old rule-4 `> 0`). It rate-normalises the window counts
against the emitted slots (`seq` differenced by `EventWindow`, fed via
`playback/lock_state.rs::lock_for_heartbeat`), so 24/25-fps content on the 30-fps
grid reads LOCKED, not DEGRADED. Precedence (first match wins): `!clock_ok` →
UNLOCKED "clock not ok"; `!pacing` → UNLOCKED "pacing disabled"; `connections==0`
→ DEGRADED "no receiver"; `resyncs_w>0` → DEGRADED "resync in 60 s"; `slots_w==0`
(nothing emitted, no grid) → LOCKED; then the late rate check and, only while
`decoding` (#150), the repeat rate check; else LOCKED.

- **Late threshold** `LATE_DEGRADED_PERMILLE = 250` (25 % of slots): DEGRADED
  "late > 25 % of slots in 60 s" once `late_w * 1000 > 250 * slots_w`.
- **Repeat threshold** `expected_repeat_permille(source_fps, grid) + REPEAT_MARGIN_PERMILLE(100)`:
  DEGRADED "repeats above the fps conversion in 60 s" once repeats exceed the
  structural conversion + 10 %. `expected_repeat_permille = 1000 − source/grid×1000`
  (24/30 → 200 ‰ = 20 % of slots; 25/30 → 167; 30/30 & 60/30 → 0). Integer
  permille, no float in the rule; `grid` is the pacer's `GENLOCK_GRID_FPS` (30),
  and `source_fps` is the snapshot's **`source_fps`** — the DECODER's rate
  (`decoder.frame_rate()`), path-independent. **#168 r6b: NEVER use `nominal_fps`
  as the source rate.** `nominal_fps` is the OUTPUT nominal — the fixed grid (30)
  on the paced path, the decoder rate only on the SDK-clocked path — so feeding it
  as the source made a 23.976-fps output expect 0 % repeats and falsely DEGRADE on
  the structural 20 % conversion (box read 22.9.2026 17:56 UTC). The event +
  snapshot carry both: `nominal_fps` (output nominal) and `source_fps` (decoder).
  **Sourcing `source_fps`:** SDK-clocked path = `submitter.nominal_fps()` (the
  submitter is `set_frame_rate`'d to the decoder there). PACED path = threaded
  from the decode PRODUCER via `open_tx` (`run_decode_producer` reads
  `decoder.frame_rate()`; the submit thread owns the submitter, and the paced
  submitter is NEVER `set_frame_rate`'d so `submitter.nominal_fps()` there is the
  grid, not the source).
- **Calibration (22.9.2026, SP-slow 24 fps on the 30-fps grid, 1 800 slots/min):**
  clean grid late ≤ 6 % of slots (0–100/min), stalled 42 % (W1 ~750/min,
  30–105 ms); repeats a constant 20 % (= 1 − 24/30) in EVERY window; resyncs 0.
- **Standby repeats are by design; `decoding` gates the repeat rule (#150).**
  With pacing ON, a PAUSED or IDLE output keeps servicing the grid with standby
  frames. `Standby::FrozenLast` bumps `repeats` on EVERY slot, so `repeats_w ≈
  slots_w` (box 24.9.2026: Paused SP-fast / SP-dabing read repeats +1812/min
  against seq +1812/min). `LockInputs.decoding` (the RAW pipeline transport ==
  `TransportState::Playing`, passed by `ndi_health.rs` as
  `transport_from_reported(&reported_state)` into `lock_for_heartbeat`) gates
  ONLY the repeat rule. A non-decoding output never reads "repeats above the fps
  conversion", but clock / pacing / receiver / resync / late still apply to it,
  so a standby grid that genuinely breaks still reads DEGRADED. The rule used to
  fire on standby, which made every paused output flap DEGRADED and failed the
  release E2E `genlock badges agree` (run 36063649894). Never "fix" that by
  suppressing standby frames: receivers need the continuous grid.

**Reading the badge during a soak:** LOCKED with late ≤ 100/min on 24-fps content
= the grid is HOLDING (the round-4/5 clean span). DEGRADED "late > 25 % of slots"
= the sender-side stall (submit-call block, the box-test-6 failure). DEGRADED
"repeats above the fps conversion" = decoder/producer starvation on a 30-fps
source (a 24-fps source's structural 20 % never trips it). The
`/api/v1/ndi/health` `lock_state`/`reason`, the `ndi: genlock` log line and the
dashboard `GlobalLockBadge` all read this one derivation.

## #147 round 9 — memory residency

Pacing is ON in production permanently (owner ruling, issue #147 comment
5812898277). The residual paced-sender stall with a heavy child resident is
treated as a memory-residency problem (design record, issue #147 comment
5812936370). The box runs with ~15 GB of commit over physical RAM. When the
child's working set grows, Windows trims SongPlayer's frame pools and the NDI
SDK's buffers, and the paced submit then takes hard page faults. Priority class,
`timeBeginPeriod(1)` and the TIME_CRITICAL audio thread protect CPU time. None
of them protects residency, so round 9 adds two guarantees and one gauge.

### The two settings

Both settings are read through `GET`/`PATCH /api/v1/settings`. The settings API
has no whitelist and stores any key; the defaults live in the resolvers. Both
values are flat strings in MiB, e.g. `{"sp_min_working_set_mb":"3072"}`.

| key | default | range | takes effect |
|---|---|---|---|
| `sp_min_working_set_mb` | **3072** | `0` = off, else clamped `256..=8192`; absent → default; garbage/negative/out-of-range → default or clamp + WARN | the NEXT SongPlayer start (read once in `lib.rs::start`, right after the DB is ready, before any pipeline spawns) |
| `heavy_max_working_set_mb` | **4096** | `0` = off, else clamped `512..=10240`; absent → default; garbage/negative/out-of-range → default or clamp + WARN | the NEXT heavy-child spawn (`refresh_containment` per worker tick, like the other `heavy_*` knobs) |

**`sp_min_working_set_mb` — SongPlayer's hard minimum working set.**

- The call is `process_start::apply_min_working_set`, which runs
  `SetProcessWorkingSetSizeEx(GetCurrentProcess(), min, 2×min, QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE)`.
  The minimum is HARD: the memory manager never trims SongPlayer below it. The
  maximum stays SOFT.
- At every Windows start it enables `SeIncreaseWorkingSetPrivilege` best-effort,
  whatever `sp_min_working_set_mb` is. Normal users hold that privilege, but it
  is disabled in the token by default. (The heavy child's job working-set cap
  needs a DIFFERENT privilege, `SeIncreaseBasePriorityPrivilege` — round 10.)
- The minimum commits no pages. It guarantees that pages SongPlayer actually has
  resident, up to `min`, are never trimmed.
- It DOES reserve `min` of the box's resident-available memory at call time. That
  is why too large a minimum is refused with `ERROR_NO_SYSTEM_RESOURCES` (1450),
  and it shrinks what OBS, the NDI SDK and the GPU drivers can pin. So size it
  from the measured `working_set_mb` plus headroom, not "as big as possible".
- The pure core is `process_residency.rs`: parse and clamp, the plan, the `0x9`
  flags word, and the outcome line. It is Linux-tested. The `QUOTA_*` mirrors are
  compile-time asserted against windows-sys.
- The windows-sys feature `Win32_System_Memory` was added for
  `SetProcessWorkingSetSizeEx`.
- **Why 3072 MiB.** A playing 1440p paced output holds about 21 NV12 buffers
  (look-ahead 12, pool class 6, handoff 2, repeat, holdover), about 115 MB. On
  top of that come the MF decoder and the SDK's per-sender compression buffers.
  Each idle output holds its NV12 + BGRA black, about 20 MB. That totals about
  1–2 GB, and 3072 MiB leaves headroom. Re-size it from the new `working_set_mb`
  field.

**`heavy_max_working_set_mb` — the heavy child's working-set cap.**

- The cap is set on the child's Job Object: `JOB_OBJECT_LIMIT_WORKINGSET` with
  `MaximumWorkingSetSize` = the cap and `MinimumWorkingSetSize` = 256 MiB
  (`HEAVY_MIN_WS_MB`, never above the cap). It shares the ONE extended-limit
  struct with the #162 10 GiB commit ceiling, kill-on-close and the #203
  affinity. Every existing flag is unchanged.
- When the child needs more resident memory, it pages ITSELF instead of evicting
  SongPlayer.
- The flags word comes from the pure `heavy_containment::job_limit_flags`, which
  adds WORKINGSET only when the cap is non-zero.
- **Fallback.** If the OS rejects the combined limits, or rejects the process
  assignment with them, the seam retries ONCE without the cap and logs a WARN
  (`heavy child working-set cap … rejected` / `… assignment with a … working-set
  cap failed`). A bad working-set value can never cost the child its memory
  ceiling or kill-on-close.
- **Why 4096 MiB.** The measured child working set is 1.1–1.6 GB (round 7), 2.8 GB
  (#168), and 2.79 GB with a 3.35 GB peak (#207). Its commit is 6.7–9 GB. So 4 GiB
  sits above every measured peak working set, while bounding the resident
  footprint well under the 10 GiB commit ceiling.

### Log fields

- **Startup, once, INFO:**
  `sp working set: hard_min_mb=3072 max_mb=6144 flags=0x9 privilege=ok|failed(err=<GetLastError>) result=ok|failed(err=<GetLastError>)`,
  or `sp working set: hard_min disabled (sp_min_working_set_mb=0) privilege=ok|failed(err=…)`.
  - The privilege is enabled whatever the setting.
  - Off Windows the line is `sp working set: not applied off Windows (sp_min_working_set_mb=<n>)`,
    never a fake `ok`.
  - `privilege=failed(err=1300)` is `ERROR_NOT_ALL_ASSIGNED`: the token lacks
    the privilege.
  - `result=failed(err=1450)` is `ERROR_NO_SYSTEM_RESOURCES`: the minimum is too
    large for the box's resident-available memory.
  - Both are logged, never a panic.
- **Per heavy child:** the `heavy child contained (pid …): … reserve_gib=<n> max_ws_mb=<n|off>`
  line gains `max_ws_mb`. It reports the cap the Job Object actually APPLIED;
  `off` means the cap is disabled or was rejected.
- **Per UTC minute, per paced output:** the `pipeline: loop-stats …` line now
  ends with `page_faults_per_min=<n|na> working_set_mb=<n|na>`.
  - The value is SongPlayer's own `PROCESS_MEMORY_COUNTERS.PageFaultCount`
    delta per minute, plus `WorkingSetSize` in MiB, from `GetProcessMemoryInfo`.
  - It comes from `playback/proc_mem.rs`. ONE process-global `FaultWindow` is
    sampled at most once per 60 s by whichever paced heartbeat comes first, so
    every paced output's line shows the same last-full-minute value.
  - `na` means one of:
    - the first minute after start;
    - the first minute after a gap of more than 5 min with no paced heartbeat
      (`MAX_SAMPLE_GAP_MS`), which re-baselines;
    - the SDK-clocked path;
    - non-Windows.
  - `PageFaultCount` is a u32 that wraps about every 2.4 h at 500k/s. The delta
    is the shared wrap-safe `process_start::residency::fault_delta` (`wrapping_sub`).
    `lyrics/heavy_faults.rs` now uses the same helper: its old `saturating_sub`
    zeroed every wrap.
  - The paced heartbeat runs on the EMIT thread, so `proc_mem::gauge` never
    blocks. It only `try_lock`s the window; a busy lock means another output is
    sampling. Every caller reads the published gauge from two lock-free atomics
    (`to_slots` / `from_slots`, sentinel `u64::MAX`).
  - The arithmetic and the slot encoding are pure, Linux-tested and
    mutation-scored. The OS read is `mutants::skip`.

### Box A/B window recipe (pacing ON in every window)

This is the round-7 method: one variable per 15-minute paced window, the live
wall on program (SP-slow), and the receiver read as cg OBS
`genlock-fifo audit 'sp-slow_video'`. A stems child must be resident AND
productive: `TotalProcessorTime` delta over 6 s > 0.

| window | `sp_min_working_set_mb` | `heavy_max_working_set_mb` | how to switch |
|---|---|---|---|
| **W-a** both off | `0` | `0` | PATCH both, restart (Deploy-job rerun; the restart also respawns the child, uncapped) |
| **W-b** SongPlayer hard min only | `3072` | `0` | PATCH, restart (the minimum is read at start) |
| **W-c** both | `3072` | `4096` | PATCH, kill the child by `-Id` (the cap applies at the next spawn, no restart); a `max_ws_mb=off` contained line here means the OS rejected the cap and the retry-without fallback fired (read the WARN) |

**Confirm each window before trusting it:**

- the startup `sp working set:` line shows the expected `hard_min_mb`/`disabled` and `result=ok`;
- the child's `heavy child contained … max_ws_mb=` line shows the expected cap;
- in W-c, `Get-Process python | Select WorkingSet64` stays ≤ the cap.

**Report per window:**

- sender minutes with `submit_call_us_max` ≤ 20 ms;
- SongPlayer `page_faults_per_min` and `working_set_mb`;
- receiver `dropped_due`, underruns, relocks and late_holds;
- child throughput (segments or CPU-s per window).

**Targets and what the gauge tells you:**

- **Target:** W-c reaches the no-child control (0 drops/min).
- **If W-c does not reach it** and `page_faults_per_min` is already flat, residency
  is not the channel. The next lever is memory bandwidth, e.g. a separator fork
  with a bounded batch size (design Approach 3).
- **If `page_faults_per_min` still spikes in W-b/W-c** while `working_set_mb` sits
  at the minimum, raise `sp_min_working_set_mb`.

## #147 round 10 — no per-frame allocation churn

**The rule: no allocation of ≥ 64 KB per frame (or per audio block) on the
playback path.** A per-frame large buffer is REUSED:

- On the owning thread, an owned scratch `Vec` that is `clear()`ed and
  `resize()`d, so its capacity is kept.
- Across threads, a bounded recycle: `sp_decoder::frame_pool` (NV12 frames,
  `take`/`recycle`/`PooledBuf`), a tap's own bounded pool, or a one-slot `spare`
  handed back by the consumer.

SongPlayer runs on the Windows system heap. Every block above ~512 KB is a fresh
`VirtualAlloc`/`VirtualFree`, and its first touch demand-zero faults each 4 KB
page. A 1440p NV12 frame is 5.5 MB, about 1350 faults per fresh copy.

**The check is `page_faults_per_min` on the paced `pipeline: loop-stats` line**
(round 9, `proc_mem.rs`). Read it before and after any change on this path.

**The audit (issue #147 comment 5814563750).** The playing wall path — the MF
reader copy, `to_paced_frame` → pacer → handoff → submitter holdover, and
`submit_nv12` — was already pool/Arc-reused by #203 2b. Round 10 converted:

- `FrameSubmitter`'s `PacedSink::emit` is now an Arc bump into
  `submit_frame_at_boundary_owned`. It used to be a `to_vec` copy.
- `submit_frame_at_boundary(&[u8])` and `PooledBuf::clone` (the burn overlay's
  per-frame `make_mut` fork) copy into a pooled buffer:
  `PooledBuf::copy_from_slice` / `SharedFrame::copy_from_slice` →
  `frame_pool::take`.
- The #178 stream tap's vfeed hands each written canvas back to the tap's pool
  (`StreamShared::write_frame`). Before, every watched frame was a fresh
  337.5 KB alloc.
- The #15 JPEG tap downscales into the RGB buffer the encoder worker hands back
  (`Inbox.spare`, `downscale_nv12_to_rgb_into`).

**Left on purpose:**

- Audio blocks and chunks ≤ 32 KB (below the threshold).
- The per-idle-entry black frame.
- The fMP4 fragments (per 500 ms, not per frame).
- The JPEG encoder's output `Vec` (≤ 5/s while a JPEG viewer polls, a 320×180
  JPEG is far below 64 KB).
- Allocations inside Media Foundation (`ConvertToContiguousBuffer` / `Lock` on a
  row-padded 2D surface) and inside the NDI runtime. These are not in our code.
  If `page_faults_per_min` stays in the millions after round 10, that is where
  the churn is. The next lever there is an `IMF2DBuffer::Lock2D` read path,
  which needs box verification of the padded-plane offsets.

**Job privilege for the heavy child's working-set cap.** The cap is
`JOB_OBJECT_LIMIT_WORKINGSET` with `MinimumWorkingSetSize` = 256 MiB.

- It needs `SeIncreaseBasePriorityPrivilege` (`SE_INC_BASE_PRIORITY_NAME`) in
  SongPlayer's token.
- Without it `SetInformationJobObject` fails with 1314 (`ERROR_PRIVILEGE_NOT_HELD`)
  — the round-9 box read.
- Source: Windows Research Kernel `base/ntos/ps/psjob.c`, `NtSetInformationJobObject`,
  the WORKING SET LIMIT branch — `MinimumWorkingSetSize <= PsMinimumWorkingSet ||
  SeSinglePrivilegeCheck(SeIncreaseBasePriorityPrivilege)`, else
  `STATUS_PRIVILEGE_NOT_HELD`. Microsoft's `JOBOBJECT_BASIC_LIMIT_INFORMATION`
  page names this privilege only for the priority and scheduling class.
- It is NOT the `SeIncreaseWorkingSetPrivilege` that round 9 enables for
  SongPlayer's own hard minimum.

`heavy_slot::job_working_set_privilege` enables it ONCE per process, before the
first capped job, and logs
`heavy child job privilege: SeIncreaseBasePriorityPrivilege=ok|failed(err=N)`.

- `failed(err=1300)` means the account's token does not hold it. The
  "Increase scheduling priority" user right is granted to Administrators by
  default.
- The retry-without-cap fallback stays. Its WARN now carries
  `base_priority_privilege=…`.
- The privilege is deliberately left enabled for the process lifetime (a token
  privilege is process-wide; enabling it only permits what the account already
  holds).
- Box acceptance: the contained line reads `max_ws_mb=4096` and no `rejected`
  WARN appears.

## #147 round 11 — one lock per NDI sender, and the full task token

**Per-sender NDI locking (`sp-ndi/src/handle_table.rs`).**

- **The old lock.** `RealNdiBackend` kept every sender in ONE
  `Mutex<HashMap>`, held across each SDK call: video, async video, flush,
  audio, tally, connections and source URL.
  `NDIlib_send_send_video_async_v2` blocks until the SDK has finished with that
  sender's previous frame. With pacing, all ~10 outputs emit on the same
  33.3 ms boundary, so one slow sender delayed every other output's video and
  audio send. The per-output `submit_call_us_*` gauge included that wait.
- **Map lock.** The map is now `HandleTable<RealHandleState>`: an `RwLock`
  over `usize → Arc<Mutex<Option<T>>>`. The map lock is held ONLY to insert,
  remove, or clone one handle's `Arc`, never across an SDK call.
- **Handle lock.** Each handle's own `Mutex` is held across its SDK call.
  Calls on one sender stay ordered, so audio and video on the same output
  still serialise. Different senders run in parallel.
- **Destroy.** `remove_with` removes the `Arc` under the write lock, then
  waits on the handle's lock for the in-flight send. It takes the state out
  (`Option::take`) and calls `NDIlib_send_destroy` while still holding that
  lock. A send that cloned the `Arc` before the remove finds `None` and does
  nothing, the same as a missing handle.
- **Tests.** The table is generic, so the locking is Linux-tested and
  mutation-scored with plain values and threads (channels plus bounded
  timeouts). The `RealNdiBackend` methods stay `mutants::skip` (SDK pointer
  derefs). Never put the map lock back around an SDK call:
  `an_op_on_another_handle_completes_while_one_handle_is_blocked` and
  `create_and_destroy_of_other_handles_proceed_while_one_handle_is_blocked`
  fail on that shape.

**The SongPlayer task runs with `-RunLevel Highest`** (the `ci.yml` Deploy step
"Configure auto-start and launch").

- **Why.** `Resolume` is an Administrator. A `Limited` task gets the filtered
  UAC token, which does not hold `SeIncreaseBasePriorityPrivilege`. The box
  logged `heavy child job privilege: SeIncreaseBasePriorityPrivilege=failed(err=1300)`,
  and the child's working-set cap was rejected with 1314 (issue #147 comment
  5815246953). The round-9 `SeIncreaseWorkingSetPrivilege` is in both
  tokens; only `SeIncreaseBasePriorityPrivilege` needs Highest.
- **Side effects, accepted.** SongPlayer and its children (CLIProxyAPI,
  yt-dlp, ffmpeg, the heavy python workers) now run at high integrity, and so
  does the port-8920 server. Windows (UIPI) blocks input from medium-integrity
  processes into the Tauri window, e.g. drag-and-drop from Explorer.
  WebView2 runs elevated. If it failed to start, the deploy's health check
  would catch it.
- **Where.** The task is re-registered on EVERY deploy, so the RunLevel lives
  only in `ci.yml`. `scripts/setup-runner.ps1` registers only the runner's own
  task (already Highest).
- **Box acceptance after the deploy:**
  - the `heavy child job privilege:` line reads
    `SeIncreaseBasePriorityPrivilege=ok`. It is logged once, at the first capped
    heavy-child job, not at startup (`heavy_slot::job_working_set_privilege`);
  - the `heavy child contained` line reads `max_ws_mb=4096`;
  - no `working-set cap … rejected` WARN appears.

## Paced audio is pinned to the picture by MEDIA TIME (#148 design v2)

The measured defect (A/V gate, 25.9.2026): with pacing ON the offset was fixed
per song but random across songs (−38 … +60 ms). The chunk media time was
dropped in `to_paced_frame`, `AudioGridBuffer` was a plain FIFO, an early
underrun kept the queue (the audio stayed late for the rest of the song), and
the #148 PLL steered the buffer LEVEL toward 3200 samples, which has no
relation to the picture. The PLL (`AudioPll`, `LevelAverager`, `residual_ppm`,
`audio.residual_ppm`/`applied_ppm`, the `audio_ppm=` log token) is DELETED.
Now:

- `to_paced_frame` puts each chunk's 0-based media time
  (`(ts_ms − pts_offset_ms)·10⁴`) in `AudioFrame.timecode_100ns`. Only the
  pacer reads it; the boundary chunk it submits still carries `None`, so the
  submitter stamps the raw wall clock (§6).
- `AudioGridBuffer` has a media HEAD in samples, taken from the first TIMED
  push after `clear()`. After that it is counted, never re-read from the later
  chunk stamps (Symphonia stamps are integer ms).
- **Audio is pushed when a frame is PULLED** (`Pacer::pull_frame`, in both
  `prepare` and `service`), not when it is consumed. The aligned take at
  boundary B needs media up to B + 33 ms. Only the NEXT (parked) frame's paired
  audio covers that, so pushing on consume underruns on every 24/25-fps song.
- **The paced decoder reads a 250 ms audio cushion (#148 v4).**
  `pipeline_paced.rs` opens the decoder through `pacer::open_paced_decoder`,
  which calls `SplitSyncedDecoder::with_audio_lead(.., PACED_AUDIO_LEAD_MS = 250)`.
  Each frame therefore carries audio up to pts + 250 ms, not pts + 40 ms.
  - **Why.** With the 40 ms pairing the grid held only ~40 ms past the parked
    frame. A video decode stall of 2+ boundaries (the MF stalls under a
    resident heavy child) emptied it, and `take_block` zero-filled an audible
    ~100 ms gap. Box, dev `a19beda`: song 334, `audio_underruns` 2→3, A/V gate
    `dropouts=1 dropout_ms=100`; SP-slow underruns climbed 5→13 over one E2E.
  - **Depth does not move A/V.** The take is aligned by the media HEAD
    (`err = head − expected`), never by `level_samples`, so a deeper buffer
    plays the same sample at each boundary. Tests:
    `pacer_tests_av_lead.rs` (a 5-boundary stall: 0 underruns, bit-exact, 0
    corrections; a 40 ms control underruns) and
    `a_deep_buffer_is_never_servoed_toward_a_level_target`.
  - **Bounded.** The G5 read gate keeps the read-ahead ≤ lead + one chunk,
    far under the grid's 2 s cap, so it never grows.
  - **Cost.** Fader latency rises by up to the lead (`karaoke-stems.md` G5).
  - **Dashboard preview follows the lead.** The preview (#178) taps audio at
    the SAME decode seam and holds it for `preview_stream::lead_ms_for(true)`
    = `PACED_AUDIO_LEAD_MS − 40` = 210 ms. If the lead changes, this changes
    with it (the test pins both), or the preview plays its audio early.
  - **Scope.** The pacing-OFF path is untouched: `open_synced_decoder` →
    `decoder_tolerance_ms` (40, or 1540 with the wall-clock emitter).
  - **A stall longer than ~250 ms still underruns.** Raise the lead only with
    a box measurement of the stall length, never as a blind bump.
- **The anchor is local, on the DUE boundary** (`pacer_av_align.rs`). The first
  FRESH frame emitted on a new map fixes `(pts in samples, the boundary it is
  DUE at)`. The due boundary is the first grid boundary at or after
  `wall_start + pts`, computed as `strict_next_boundary_100ns(ws + pts − 1)`.
  It is NEVER the boundary the frame happened to be emitted at. The block at
  boundary B must then start at `anchor_media + (B − anchor_wall)`, which is
  the wall line the picture follows, including the frame's sub-slot phase.
  - It keeps going across repeat boundaries, so a 24→30 pattern needs no
    correction.
  - Do NOT compare against each emitted frame's own pts: that is a 0…41 ms
    sawtooth on 24-fps content, and the corrector would thrash.
  - Do NOT anchor at the emit stamp (review of `0c75806`, 1 red). A STALE first
    frame would pin the audio behind the picture for the whole song:
    - a slow decoder at song start;
    - the first frame after a stall.

    The picture catches up to the wall line by dropping frames, and the audio
    must do the same.
- **Two re-align kinds.**
  - A NEW map forgets the anchor (`AvAlign::realign`): `anchor()`
    (play/seek/new song) and a `Reanchored` lag re-anchor, which moves
    `wall_start`.
  - The SAME map keeps the anchor and only re-snaps the buffer onto its line
    (`AvAlign::resnap`): a grid resync in `resolve_emit_boundary` (only the
    stamp jumps) and `audio_resume_reset()`.
  - Until the expected media is buffered the output is silence. Then
    `align_to` DROPS early audio or PADS late audio with leading silence, so
    the first real sample plays at its media time (±1 sample).
  - With too little buffered to drop, the block stays silent and the drop is
    retried on the next boundary.
  - A pad that would take pad + buffered audio past the 2 s cap is refused
    (the cap trim would eat the fresh padding): silence until it fits, never an unbounded allocation.
- **Continuous correction** (`correction_for`) is the ONLY controller. It
  engages when |err| > 240 samples (5 ms), moves ≤ 48 samples per block, and
  stops at |err| ≤ 48 (1 ms). A drop or insert of d samples reads n ± d inputs
  onto n outputs by linear interpolation (`take_block`), so there is no click.
  Output 0 is always an exact input sample. Underrun samples are zero-filled
  and do NOT advance the head, so a decoder stall shows up as a negative error
  that the correction then drops away. A corrected block is a 3 % time-stretch
  (48 of 1600 samples, about 51 cents) for about 0.6 s per 20 ms. That is
  audible on a sustained note, and it is what the ≤ 48-samples-per-block
  design accepts.
- Untimed audio (tests / frames with `timecode_100ns: None`) plays as a plain
  FIFO. It is never aligned.
- Telemetry is on `PacingStats` (`/api/v1/ndi/health` `pacing`), not `audio`:
  - `av_align_err_ms` — head − expected at the last productive boundary, before
    its correction; + = audio AHEAD of the picture, the gate's sign;
  - `av_corrections` / `av_corrected_samples` — cumulative; they include a
    non-zero start drop or pad.

  The same three keys replace `audio_ppm=` on the `ndi: genlock` line; the
  song summary logs `av_align_err_ms` + per-song `av_corrections`.
  `PacingStats` is no longer `Eq` (f64).
- Tests encode each sample's own media index in its VALUE
  (`pacer_tests_av_align.rs`: `enc(s) = s + 1e6`, silence = 0.0). Assertions
  then read the media time actually put on the wire, without float tolerance.
  Keep that pattern for any new alignment case.

## Merge gate for pacing/decode/NDI/audio changes: the post-deploy A/V gate (#147)
A change to pacing, the submitter, decode, the mixer, NDI or the audio path
merges only with `e2e/post-deploy-av-sync.spec.ts` green. That spec records the
OBS program and requires |A/V| ≤ 40 ms and zero 50 ms dropout blocks against
the original sidecars. Method, thresholds and how to read the `AV-SYNC …`
output: `.claude/rules/obs-ndi-health.md` "Post-deploy A/V gate (#147)".
