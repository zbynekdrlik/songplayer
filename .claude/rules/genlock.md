---
paths:
  - "crates/sp-core/src/genlock*.rs"
  - "crates/sp-core/src/clock_health*.rs"
  - "crates/sp-server/src/playback/wallclock*.rs"
  - "crates/sp-server/src/playback/clock_health*.rs"
  - "crates/sp-server/src/playback/pacer*.rs"
  - "crates/sp-server/src/playback/submitter*.rs"
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
  page-fault pressure). So `genlock_pacing` STAYS OFF in production until the
  submit is also moved off the boundary-critical path (a dedicated NDI-submit
  thread) or the stems child's D3D/NDI-path impact is bounded. `iter_p99` ≫
  `prep_p99` is the signature of submit-side (not decode-side) lateness.
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
  `● GENLOCK OFF` when NO output has pacing enabled (the production default
  `genlock_pacing=false`), else `● LOCKED`/`● DEGRADED`/`● UNLOCKED` (green/amber/
  red, `n/m` live-locked count + worst reason). #176 revised #164's "hide the
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
  clock (the Pacer's `AudioGridBuffer` + PLL) and is untouched.
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
  stage. The paced (#168, genlock_pacing OFF in prod) submit-thread also surfaces
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
  `prev_frame`); the paced burn overlay paints via `SharedFrame::make_mut` (in
  place while sole owner — it IS the sole owner on the submit path, since the pacer
  keeps its own `last_frame` clone); idle Black is submitted by shared reference
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
  `SharedFrame::make_mut` (`Arc::make_mut` → a fresh `PooledBuf` copy) is the
  only cloner and burn is default OFF.
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
#168 round-5 default `heavy_cpu_affinity_mask` and #147's production flip). A
90-minute investigation is now a 10-minute read.

- **Toggle pacing.** `genlock_pacing` is read ONLY at startup (`lib.rs::start` →
  `engine.set_genlock_pacing`), so a paced test = `PATCH /api/v1/settings` with
  the FLAT body `{"genlock_pacing":"true"}`, THEN restart the app. Restart via
  `gh run rerun --job <LATEST Deploy job id>` — look the id up each time
  (`gh run view <run> --json jobs`; a rerun mints a NEW job id). The dependent
  E2E re-runs and FAILS under pacing ON on the dabing 12–16 kHz spectral test —
  EXPECTED — so END every session with the flag back to `false` + another
  Deploy-job rerun to a green run.
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
(paused/idle, no grid) → LOCKED; then the two rate checks; else LOCKED.

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
- **Calibration (22.9.2026, SP-slow 24 fps on the 30-fps grid, 1 800 slots/min):**
  clean grid late ≤ 6 % of slots (0–100/min), stalled 42 % (W1 ~750/min,
  30–105 ms); repeats a constant 20 % (= 1 − 24/30) in EVERY window; resyncs 0.

**Reading the badge during a soak:** LOCKED with late ≤ 100/min on 24-fps content
= the grid is HOLDING (the round-4/5 clean span). DEGRADED "late > 25 % of slots"
= the sender-side stall (submit-call block, the box-test-6 failure). DEGRADED
"repeats above the fps conversion" = decoder/producer starvation on a 30-fps
source (a 24-fps source's structural 20 % never trips it). The
`/api/v1/ndi/health` `lock_state`/`reason`, the `ndi: genlock` log line and the
dashboard `GlobalLockBadge` all read this one derivation.
