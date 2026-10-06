---
paths:
  - "crates/sp-core/src/genlock*.rs"
  - "crates/sp-core/src/clock_health*.rs"
  - "crates/sp-server/src/playback/wallclock*.rs"
  - "crates/sp-server/src/playback/fleet_shift*.rs"
  - "crates/sp-server/src/playback/clock_health*.rs"
  - "crates/sp-server/src/playback/pacer*.rs"
  - "crates/sp-server/src/playback/audio_grid*.rs"
  - "crates/sp-server/src/playback/submitter*.rs"
  - "crates/sp-server/src/playback/paced_*.rs"
  - "crates/sp-server/src/playback/pipeline_paced*.rs"
  - "crates/sp-server/src/playback/proc_mem*.rs"
  - "crates/sp-server/src/playback/loop_stats.rs"
  - "crates/sp-server/src/playback/frame_buf.rs"
  - "crates/sp-decoder/src/frame_pool.rs"
  - "crates/sp-server/src/process_residency*.rs"
  - "crates/sp-server/src/playback/lock_state*.rs"
  - "crates/sp-server/src/playback/pacing_stats.rs"
  - "crates/sp-ndi/src/**"
---
# Genlock (NDI outputs locked to the fleet clock) — #146–#151

## #221 lane 3 — read this first: one NDI sender, pacing the only path

SongPlayer's only NDI sender is `SP-program` (`program_output.rs`; the LED
wall's `SP-program-MAX` goes over Spout). A playlist pipeline has NO NDI
output of its own: its paced consumer delivers every boundary to the program
bus (`paced_output.rs`: `BoundaryOut` → `InstalledBus` → `offer_to_bus`), and
`SP-program`'s `FrameSubmitter` is the one wire edge. Deleted with the
per-playlist senders (0.71.0-dev.16) — the history below still names them:

- the SDK-clocked path and its switch: `pipeline::decode_and_send`, the
  `genlock_pacing` setting (`sp_core::config`), `pacer_sink::idle_poll`, the
  SYNTHESIZE BGRA standby, `submit_nv12`, `send_standby_black`;
- the #192 wall-clock audio emitter and its round-4 video catch-up
  (`audio_emitter.rs`, `audio_edge_fade.rs`, `pipeline_audio.rs`,
  `av_catchup.rs`, `sp_ndi::AudioSink`);
- the #151 burn-id overlay (`burn_overlay.rs`, `ndi_burn.rs`,
  `sp_core::genlock::burn`, `POST /api/v1/ndi/burn`, `burn_on`);
- the per-playlist submit side: `submitter_paced.rs` (`paced_handoff`),
  `NdiSender::twin`, the `send_video_async` call gauge
  (`submit_call_us_max` / `_p99`, `SubmitHist`, `drain_window`,
  `WindowStats`, `paced_submit_snapshot`), the receiver count on a
  pipeline's health row and the lock's "no receiver" rule.

Pacing is the only path (the owner's rule, #147 comment 5812898277: pacing
stays ON). A "submit" on the pipeline side now means the consumer's
delivery to the program bus; `SP-program`'s own NDI submit is timed on
`GET /api/v1/program` `health.timing` (`vban-out.md`).

- Normative contract: zbynekdrlik/camera-box#1294 (§1–§8). Reference math
  + 68 test vectors: camera-box `src/ndi.rs`, `src/genlock_stamp.rs`,
  `src/genlock_pacing.rs` — ported 1:1 into `sp_core::genlock` tests.
- Timecodes are UTC in **100 ns units since the Unix epoch**; video =
  `floor_boundary_100ns` on the fixed grid (`GENLOCK_GRID_FPS`), FLOOR never
  ceil; audio = the timeline instant of the block, i.e. the boundary it belongs
  to (#224: an on-time emit's "raw wall clock at submission", never the emit
  instant of a late or catch-up emit); never `SYNTHESIZE` (it was only on
  the deleted SDK-clocked standby BGRA black).
- `clock_ok = is_locked && mode ∈ {LOCK, NANO}` from dantesync
  `127.0.0.1:8898/status`; unreachable → `no dantesync`, never blocks playback.
- Acceptance is on the RECEIVER (`genlock-fifo audit … locked=1`, camera-box
  #1300), never our own counters. Open questions go to camera-box#1294.
- Pacing (#147) runs on the exact-rational 100 ns grid: sleep target from
  `strict_next_boundary_100ns`, stamp = the serviced boundary, never
  `floor(now)` at emission, never a stamp > the wall read before the send.
  The ns gate `genlock_emit_gate` + its 43 vectors are a reference port of
  camera-box's DECIMATOR — never use an epoch-multiple ns grid as a clock
  (it drifts 10 ns/s against the second-anchored stamp grid). Box test
  2026-09-13 01:43: idle/paused outputs held 30/s, but a PLAYING output ran
  ~27/s with every frame late (p99 15 s, max 40 s) — what the decode split
  below fixed. (The `genlock_pacing` switch is deleted, #221 lane 3.)
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
  now emits through `HandoffSink` (`paced_output.rs`): it hands the
  stamped frame to a BOUNDED handoff (`submit_handoff.rs::SubmitQueue`, depth
  `SUBMIT_HANDOFF_BOUND=2`) in ~µs and stays on the grid; a dedicated submit
  thread owned the submitting `FrameSubmitter` (SDK per-instance affinity + the
  async double-buffer holdover stay single-threaded; since #147 it lives for
  the whole pipeline — see "The paced output services every boundary between
  scopes" below) and did the blocking
  `send_audio`+`send_video_async` (#221 lane 3: it now delivers to the
  program bus; `SP-program` does the NDI submit). It works BECAUSE median submit (25 ms) < the
  33.3 ms grid slot: the submit thread's ~40 fps capacity vs 30 fps demand drains
  the p99 spikes out of a shallow queue. `late_frames` is measured HONESTLY at
  the submit thread (`submit_late_100ns` = stamp → submit-start, floored),
  never at the handoff; a full handoff COALESCES to the freshest stamp
  (`handoff_policy` — drop the stalest unsent job, count a submit-side `dropped`).
  The health doc's `PacingStats` is `merge_pacing_stats`: late/max_late/iter_p99/
  dropped from the submit thread, seq/repeats/resyncs/relatches/lag/prep from the
  pacer (`/api/v1/ndi/health` shape unchanged); the paced heartbeat reads a
  submit-side snapshot (`emit_heartbeat_paced`). The pure decisions
  (`submit_handoff.rs`) are Linux-tested + mutation-scored; the
  `SharedHandoff` + consumer are cross-platform in `paced_output.rs`
  (Linux-tested over a recording sink, `paced_output_tests.rs` `Recorder`,
  since #221 lane 3 took the NDI sender away; only the blocking wait and the
  thread lifecycle are `mutants::skip`). Acceptance = box test 6
  (pacing ON, stems child resident, 60 s): `late_frames` < 1 % of
  `seq`, `resyncs`/`dropped`/`audio.underruns` 0, `lock_state=LOCKED`; then the
  flag stays ON and camera-box#1302 gets the receiver verdict.
- Submit-call cost gauge (#168 round 2) and the burn-id QR overlay (#151):
  DELETED with the per-playlist senders (#221 lane 3, the top section). Box
  test 6 had shown the NDI SDK `send_video_async` costing ~25 ms median /
  75 ms p99 per 2560×1440 frame with a stems child resident: since #223
  `SP-program` sends one 1920×1080 canvas, and its cost is
  `health.timing.submit_us` on `GET /api/v1/program`.
- Dashboard genlock indicator (#150→#164→#176): the header `GlobalLockBadge`
  (`sp-ui/src/components/ndi_health.rs`) is **ALWAYS visible** — grey
  `● GENLOCK OFF` when NO output reports pacing (since #221 lane 3 pacing is
  the only path, so: no playlist pipeline reporting yet), else
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
  `genlock-global-badge`. **What it covers since #221 lane 3** (release 0.71.0
  review): every input of the summary is a playlist PIPELINE's 60 s window,
  i.e. its pacer and its delivery to the program bus (`submit_handoff.rs`),
  never an NDI wire. `SP-program`'s own sender is NOT in the badge: its
  health is `GET /api/v1/program` `health` (`resyncs`, `late_dropped`,
  `timing`) and its receiver `degraded_reason`. With nothing playing (e.g.
  "OBS manuál" on program) the badge reads LOCKED "no live output" while
  the clock is ok.
- SDK-clocked wall-clock AUDIO emitter (#192, rounds 1–5) and the round-4
  video catch-up (`av_catchup.rs`): DELETED (#221 lane 3, the top section).
  What they taught that still holds: a coarse `thread::sleep` on the box
  overshoots by several ms (parked cores) TIME_CRITICAL or not, so a grid
  thread spins its last ~2 ms (`pacer_spin.rs`); a song transition must
  never leave an audio slot empty (the paced standby pair, below); a seek
  must not deliver pre-target frames (`SplitSyncedDecoder` discards video
  `< target` after a keyframe-aligned seek, `MAX_SEEK_DISCARD_FRAMES = 600`,
  `split_sync.rs` — still in force).
- Allocation-free steady state (#203, round 2a): the wall's page-fault storm
  under a resident heavy child was the per-frame `Vec<u8>` alloc/free (VirtualAlloc
  demand-zero faults + VirtualFree TLB shootbacks). The submit holdover is now a
  `playback::frame_buf::SharedFrame` (`Arc<Vec<u8>>`, NEVER `Arc<[u8]>` which
  copies) — `FrameSubmitter.prev_frame: Option<SharedFrame>`, sent via the new
  additive `NdiSender::send_video_async_slice(&[u8])`, so the holdover is a
  refcount hold with ZERO pixel copy (since #221 lane 3: `SP-program`'s
  submitter, the only one). Rules for anyone touching `submitter.rs` /
  `pacer.rs`: the field-order SAFETY note still holds (sender drops before
  `prev_frame`); idle Black is handed on by shared reference through
  `PacedSink::submit_shared` (`Standby::Black{dims, &SharedFrame}`,
  `service_standby` clones the Arc = a refcount bump per idle slot; the ONE
  black `SharedFrame` per pipeline is cached in its paced output,
  `PipelineOutput::standby_black_nv12` → `frame_buf::BlackNv12`, built once).
  The NV12 decoder/handoff/pacer-repeat pool (a cross-crate `sp-decoder`
  change) is round 2b, built on this `SharedFrame` seam.
- **Round 5 (#192): a SEEK must not deliver pre-target frames.** A seek
  forwarded a keyframe-aligned video seek, so `MediaFoundationVideoReader` landed
  on the PREVIOUS keyframe (`< target`) and `SplitSyncedDecoder::next_synced`
  delivered those pre-target frames (a visible jump-back). Fix (pure,
  Linux-tested, still in force): `seek` records `pending_video_target_ms`;
  `next_synced` decode-and-DISCARDS video frames `< target` (bounded by
  `MAX_SEEK_DISCARD_FRAMES = 600`, never spins) before pairing, so the first
  delivered frame is `>= pos`. (Its other half, the emitter's click-free
  `audio_edge_fade::EdgeFade`, is deleted with the emitter, #221 lane 3.)

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
  `SubmitJob::from_paced` (the handoff, `frame.video.clone()`) → the program
  bus → `SP-program`'s `prev_frame` holdover (when the canvas passes it
  through; a fit or a fade paints a pooled buffer, #223). No pixel copy on the
  pipeline side.
- **SDK-holdover safety invariant (unchanged from 2a):** a recycled buffer may
  be reused ONLY after every `Arc` is gone. Recycling fires exactly in
  `PooledBuf::Drop`, which the `Arc` runs only on the LAST holder drop — the
  submitter installs the new `prev_frame` (dropping the old Arc) AFTER the async
  call returns, so the buffer the SDK still points at is never recycled early.
  Keep `FrameSubmitter.sender` declared BEFORE `prev_frame` (field drop order).
- The idle NV12 black stays OUT of the pool as a taker (built from its own
  buffer, never `take`). It lives in the pipeline's paced output
  (`frame_buf::BlackNv12`) for the pipeline's life (#147); its single drop at
  pipeline end recycles one bounded black buffer, which is harmless.

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
  for a measurement (#221 lane 3 deleted the switch itself). A restart =
  `gh run rerun --job <LATEST Deploy job id>` — look the id up each time
  (`gh run view <run> --json jobs`; a rerun mints a NEW job id). The dabing 12–16 kHz single-snapshot E2E assertion that
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
  `pipeline: loop-stats ndi_name="SP-slow" … page_faults_per_min` and
  `ndi: genlock … late=` (lateness/min); `SP-program`'s own NDI submit cost
  is `health.timing.submit_us` on `GET /api/v1/program` (the per-playlist
  `submit_call_us_max` went with the per-playlist senders, #221 lane 3).
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
UNLOCKED "clock not ok"; `!pacing` → UNLOCKED "pacing disabled";
`resyncs_w>0` → DEGRADED "resync in 60 s" (#221 lane 3 deleted the
`connections==0` → "no receiver" rule: a playlist has no NDI output);
`slots_w==0`
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
  on the paced path (the only one since #221 lane 3) — so feeding it
  as the source made a 23.976-fps output expect 0 % repeats and falsely DEGRADE on
  the structural 20 % conversion (box read 22.9.2026 17:56 UTC). The event +
  snapshot carry both: `nominal_fps` (output nominal) and `source_fps` (decoder).
  **Sourcing `source_fps`:** threaded from the decode PRODUCER via `open_tx`
  (`run_decode_producer` reads `decoder.frame_rate()`).
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
- **The window starts over on a resume or a seek (#150, ROZHODNUTÉ
  5984539219).** `EventWindow::push` clears the ring only on a counter
  DECREASE, and the pacer keeps its counters through pause, play and seek.
  So `lock_for_heartbeat` first calls `EventWindow::restart_on(decoding,
  pacing.seeks)`, which clears the ring when either changed since the last
  heartbeat:
  - `decoding` flipped: after Paused→Playing the window held the standby
    minute (a repeat on every slot), and rule 7 read DEGRADED for up to
    ~52 s (24 fps, a pause ≥ 7.5 s). Now Play reads LOCKED from its first
    heartbeat;
  - `PacingStats::seeks` moved: the pacer's seek count, the one seek signal
    the heartbeat can see. `Pacer::anchor_seek` arms it and the seek's
    FIRST FRESH FRAME counts it (`count_settled_seek`, one bool check in
    `service`'s fresh-emit arm), never the re-anchor itself: whatever the
    refill did before that frame (a resync while the queue was empty, the
    held picture's fills) is already in the sample the window restarts
    from, so a heartbeat that lands inside the refill cannot split the
    seek's own events across the clear. A resync after that frame is a
    real one again. Every anchor (`anchor_at`: a new song's pre-roll, a
    Play) drops a seek still waiting, and `anchor_seek` arms it after its
    own `anchor()`: a seek a new song overtook is never counted at that
    song's first frame (review round 1), and a scrub of several seeks
    before a new frame counts once.
  - Rejected (main session): counting repeats only for decoding slots
    inside the pacer (the paced hot path, the `PacingStats` shape).
  - Box check: after a Pause→Play and after a dashboard seek of an output
    on program, `/api/v1/ndi/health` `lock_state` stays `LOCKED` and
    `pacing.seeks` grows by one per settled seek. The per-minute
    `ndi: genlock` line carries it too (`seeks=`, review round 2), so a
    DEGRADED → LOCKED flip at a seek reads from the log alone.
  - Tests: `lock_state_tests.rs` (`a_resume_after_a_standby_minute_…`,
    `a_seek_reads_locked_…`), `pacer_tests_preroll.rs`
    (`a_seek_is_counted_when_its_first_new_frame_goes_out`,
    `a_seek_a_new_song_overtook_is_never_counted`).

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
`timeBeginPeriod(1)` and the TIME_CRITICAL audio thread (the #192 emitter,
deleted by #221 lane 3) protected CPU time. None of them protects residency,
so round 9 adds two guarantees and one gauge.

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
  Each paced idle pipeline holds one NV12 black, about 3 MB. That totals about
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

- `SP-program` minutes with `health.timing.submit_us_max` ≤ 20 ms (before
  #221 lane 3: per-playlist sender minutes with `submit_call_us_max` ≤ 20 ms);
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
reader copy, `to_paced_frame` → pacer → handoff → submitter holdover — was
already pool/Arc-reused by #203 2b. Round 10 converted:

- `FrameSubmitter`'s `PacedSink::emit` is now an Arc bump into
  `submit_frame_at_boundary_owned`. It used to be a `to_vec` copy.
- `submit_frame_at_boundary(&[u8])` (test-only since #221 lane 3) and
  `PooledBuf::clone` copy into a pooled buffer:
  `PooledBuf::copy_from_slice` / `SharedFrame::copy_from_slice` →
  `frame_pool::take`.
- The #178 stream tap's vfeed hands each canvas it is done with back to the
  tap's pool (`StreamShared::recycle_frame`). Before, every watched frame was a
  fresh 337.5 KB alloc.
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
  sender's previous frame. With pacing, all ~10 outputs (then: one NDI
  sender per playlist) emitted on the same 33.3 ms boundary, so one slow
  sender delayed every other output's video and audio send. Since #221 lane
  3 `SP-program` is the only sender; the per-handle locks still keep its
  calls independent of the NDI input's receive handles (`ndi-input.md`).
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
  pacer reads it; the boundary chunk it submits still carries `None`, and the
  pacer stamps it on its boundary (#224, see "Every audio block is stamped on
  its boundary" below).
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
    the SAME decode seam and holds it for `preview_stream::decode_seam_lead_ms()`
    = `PACED_AUDIO_LEAD_MS − 40` = 210 ms. If the lead changes, this changes
    with it (the test pins both), or the preview plays its audio early.
  - **Scope.** The paced path is the only path since #221 lane 3 (the
    pacing-OFF path, `open_synced_decoder` → 40 ms, or 1540 ms with the
    wall-clock emitter, is deleted).
  - **A stall longer than ~250 ms still underruns.** Raise the lead only with
    a box measurement of the stall length, never as a blind bump.
  - **Resume flushes the cushion (known bound, unchanged design).**
    `audio_resume_reset` clears the buffer, and the decoder never re-delivers
    that audio. The map is kept through a pause (standby does not move
    `wall_start`), so after a pause of `D` < 250 ms the re-snap PADS up to
    `250 − D` ms of silence; before v4 that was ≤ `40 − D`. A pause ≥ the lead
    is unaffected, because the flushed audio is behind the wall line anyway.
    Keeping the buffer through Resume (re-snap drops only the paused-over
    media) would remove it. That is a Resume design change for the main
    session, not part of v4.
- **The anchor is local, on the DUE boundary** (`pacer_av_align.rs`). The first
  FRESH frame emitted on a new map fixes `(pts in samples, the boundary it is
  DUE at)`. The due boundary is the first grid boundary at or after
  `wall_start + pts`, computed as `strict_next_boundary_100ns(ws + pts − 1)`.
  It is NEVER the boundary the frame happened to be emitted at. The block at
  boundary B must then start at `anchor_media + (B − anchor_wall)`.
  - **The picture origin lands on that frame's due boundary (#148 v6,
    Approach 3, ROZHODNUTÉ 5834440419).** When the anchor is fixed,
    `Pacer::land_origin_on_grid` sets `wall_start = due(pts₀) − pts₀` (the lag
    re-anchor's rule). The frame is still shown at the same boundary, and every
    frame then presents at `due + (pts − pts₀)`: the audio's line. Without it,
    a start position or seek (the first frame's pts lands anywhere inside a
    frame) kept the picture up to one slot BEFORE the audio for the whole song.
    Box, SP-slow, 23.976 fps: `av_frame_offset` −5.0 / −25.7 / +16.0 (A/V gate
    −25…−27 ms).
    - Do NOT instead anchor the audio on the unrounded present time
      `wall_start + pts` (the rejected v6 draft). 30/60-fps frames are SHOWN at
      their due boundaries, so that led the audio by up to +33 ms after every
      seek.
    - It lands only together with the audio anchor (the first fresh frame with
      timed audio on a new map). A resnap keeps the anchor and so the origin:
      moving it onto a later off-grid 24-fps frame would shift the picture off
      the audio line.
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

## The WallClock anchor never steps the wall (#147, design comment 5827410168)

**The defect.** The box relatched 6× in one 20 s A/V take (SP-slow
`relatches` 0 → 6, `av_corrections` 1 → 200) while dantesync only slewed
(≤ 94 ppm, ≤ 2.3 ms). The cause was SongPlayer's own `WallClock`:

- The anchor was `(Instant::now(), Utc::now())`, two UNBRACKETED reads.
- A preemption between them (the box runs a heavy child at 60–108k page
  faults/s) paired a stale `Instant` with a later UTC read. The wall jumped
  FORWARD by the preemption time.
- The next clean re-anchor (every 100 frames) jumped it BACK.
- Any backward move that crosses the boundary just emitted relatches
  (`latched_boundary_100ns`), and the relatch re-stamps an earlier slot. It
  does not need a full 33 ms slot: the resample runs right after an emit.

**The rules now** (`playback/wallclock.rs`, pure math in
`wallclock_anchor.rs`):

- **Bracketed sampling.** Every anchor reads `m1 = Instant::now(); utc; m2 =
  Instant::now()` up to 8 times (`ANCHOR_MAX_ATTEMPTS`) and keeps the NARROWEST
  bracket, paired at its MIDPOINT (error ≤ width/2). It stops early at a
  ≤ 20 µs bracket, so the normal cost is one read. A chosen bracket > 200 µs is
  counted as wide.
  - A `ClockSource` fake that implements only `sample()` gets zero-width
    brackets through the default `read_bracketed`, so the existing
    sample-count tests are unchanged.
  - Override `read_bracketed` to inject a preempted read
    (`wallclock_test_clock.rs::VirtualClock`).
- **Bounded update.** A resample measures `delta = sample.utc − wall(sample.instant)`.
  - |delta| ≤ 1 ms (`ANCHOR_MAX_STEP_100NS`) applies as-is (normal slewing,
    ≈ 313 µs per 3.33 s resample at 94 ppm).
  - Beyond that only ±1 ms applies, and a `wallclock: re-anchor delta over 1 ms`
    WARN logs `delta_us`, `bracket_us`, `applied_us` and `carry_us`.
  - The remainder is NOT carried explicitly: the next resample re-measures it.
    Adding the old carry would count it twice.
- **A confirmed step is FOLLOWED in one event, in either direction (#147,
  design records 5845527884 (forward) and 5850063723 (backward)).** dantesync
  steps the fleet date in one coordinated step (1.12.0, from 26.9.2026: once a
  night at 04:00 local, up to ~±1.5 s, either direction, announced with a
  lead; no daytime steps), and every camera-box sender follows
  `CLOCK_REALTIME` at once. Slewing a step at 1 ms per resample kept our stamps
  ~300 ppm off the fleet: ~2.7 min for +50 ms (the 26.9. 09:48 A/V take failed
  inside that window), ~83 min for −1.5 s. The pure rule, ONE function for both
  directions, is `wallclock_anchor.rs::decide_anchor_step`:
  - a CLAMPED resample (|delta| > 1 ms, either sign) from a narrow bracket (not
    wider than `ANCHOR_WIDE_BRACKET`, 200 µs) applies the bounded 1 ms (a step
    ahead, or a 1 ms hold) and ARMS the step
    (`PendingStep { delta, applied, direction }`);
  - the NEXT resample follows the rest in ONE event (`applied = delta`, no
    clamp) when it is narrow too, still > 1 ms, and `delta₂ + applied₁` is within
    ±1 ms of `delta₁` — the same step seen twice (~3.3–6.7 s after the step).
    The UTC anchor takes the rest; since #224 part 2 the wall's regrid
    relabels its whole slots, so the timeline moves only by the remainder, in
    either direction (see "A date step relabels" below; before, a backward
    rest was ONE HOLD of its whole size). The tolerance alone implies the same sign, so
    the rule never compares directions (such a check would be redundant, and
    its mutants would be equivalent);
  - everything else stays the ±1 ms bound: a lone outlier moves the wall 1 ms
    (a step or a hold) and the next read takes it back out; a wide (preempted)
    sample never arms or confirms (it breaks the chain).
  - The follow logs INFO `wallclock: confirmed UTC step followed in one
    re-anchor (#147)` with `delta_us`, a SIGNED `step_us` (the whole step) and
    `direction=forward|backward`.
  - **Since #224 this resample path is only the fallback.** The per-boundary
    step probe (next bullet) follows a step over 2 ms at the boundary it
    lands; the resample only still sees one when every probe of those ~100
    boundaries was rejected, or a step of 1–2 ms (slewed as before).
  - Tests: `wallclock_tests_confirm.rs` (forward) and
    `wallclock_tests_confirm_backward.rs`: the exact ±1 ms tolerance both ways,
    wide / outlier / opposite-direction cases, the −1.5 s step walked at 1 ms
    resolution (since #224 part 2 a relabel: the timeline monotonic, no
    hold). Also `wallclock_tests_anchor.rs` (±50 ms) and
    `pacer_tests_wall_anchor.rs` (`run_step`: a followed −1.5 s step in the
    paced loop never pauses it).
  - Every `WallClock` follows (the pacer walls, the submit consumer's, the
    #209 `SP-program` sender's, the NDI input's, VBAN's). Since #224 each
    follows at its own next tick, so two walls sit apart for at most ~one
    boundary (before: one resample period, ~3.3 s). Since #224 part 2 they
    sit apart by at most the REMAINDER r (< one slot, see "A date step
    relabels" below), inside the program bus's 3-slot fill grace for ANY
    step: the old ±1.5 s discontinuity (a consumer counting `late_frames`
    against a held pacer, the program filling) is gone.
- **The step probe follows a date step at the boundary it lands (#224,
  design record 5890605448).** The 2-resample follow above kept every stamp
  3.3–6.7 s behind the fleet after a dantesync step (the receivers see it at
  once): cg OBS placed ~87 ms of audio late after the 02:00Z 89.7 ms step
  (#224 body; camera-box issue 1381 comment 5882384071), and
  the 10:36:30Z 297.7 ms step broke an A/V take. Now `WallClock::tick` takes,
  after the unchanged resample, ONE bracketed read per boundary and judges it
  with the pure `wallclock_anchor.rs::decide_step_probe`:
  - it measures against the anchor's LINE (`Anchor::line_at`: the wall's line
    extended back through a hold in progress), never the frozen wall value —
    so a probe inside a followed hold reads 0, not the remaining hold as a
    new step (the submit consumer ticks per job, also inside its own hold);
  - |delta| ≤ 2 ms (`STEP_DETECT_100NS`, the receivers' wall-step threshold)
    → quiet: slewing stays on the 100-frame resample and its ±1 ms bound;
  - over 2 ms from a bracket wider than 200 µs → rejected by width (a
    preempted read errs by ≤ half its bracket, so a narrow probe can never
    fake 2 ms), nothing armed;
  - over 2 ms from a narrow probe → a full anchor sample (best of 8) in the
    SAME tick; narrow and within ±1 ms (`same_step`, the one rule shared with
    `decide_anchor_step`) → FOLLOW in ONE event through
    `WallClock::regrid` (#224 part 2: the UTC anchor takes the whole step,
    the timeline only the remainder r — see "A date step relabels" below).
    Else rejected, nothing armed, so the next boundary follows a real step
    at once;
  - a step the resample saw first in the same tick (1 ms applied + armed) is
    followed by the probe right after it: the armed 1 ms counts toward the
    2 ms (a 2.5 ms step reads 1.5 ms after the resample) and into the total;
  - the armed 1 ms keeps counting for every probe until the next resample
    replaces `pending` (only while the probe reads that same step's rest);
  - a follow restarts the resample count (a fresh anchor), so no resample
    lands inside the hold it starts (review round 1: the submit consumer,
    ticking per job inside its own hold, re-measured the hold as a new step
    and followed it twice);
  - `last_detect_to_follow_us` is set by every follow, the resample path's
    too (after every probe was rejected); 0 when no over-2 ms probe preceded
    it.
  - Cost: one bracketed read (~3 clock reads) per boundary per wall; the
    confirming sample only on a suspected step.
  - Log: INFO `wallclock: UTC step followed at once by the boundary probe
    (#224)` with `delta_us`, `step_us`, `applied_us`, `direction`,
    `probe_delta_us`, `probe_bracket_us`, `bracket_us`, `detect_to_follow_us`;
    an unconfirmed probe logs INFO (`… was not confirmed by the anchor sample
    — rejected (#224)`), a wide one DEBUG.
  - Tests: `wallclock_tests_probe.rs` (WallClock over `VirtualClock`: +90 /
    +700 / −90 ms at the boundary they land, wide probe / wide confirmation
    then followed, a lone narrow outlier via `outlier_next_reads`, the
    resample-first step, probes after a relabelled backward step, 2 ms vs
    2 ms + 100 ns), `wallclock_tests_probe_rule.rs` (the pure rule at every
    boundary + the telemetry), `pacer_tests_wall_anchor.rs` (since #224
    part 2: a −1.5 s step never pauses, a +90 ms step never bursts, a real
    stall still catches up). The resample's own confirm tests now script a
    realtime outlier on the RESAMPLE's read (the probe runs after it in the
    same tick and reads the truth).
  - **`WallClock::settable` pairs the set value with a SYNTHETIC instant on
    one line** (review round 2): pairing it with the real `Instant::now()` let
    the probe read a set that outruns real time as a step and follow it,
    restarting the resample count (`program_output_tests::the_sender_thread_…`
    counted 2 ticks for 3). A set forward never reads as a step; a set BACK
    across a resample still shows a phantom hold in `wall_anchor_*` (the
    resample measures against the frozen wall) — never pin the anchor
    telemetry after a set back, the reads are unaffected.
  - **Harness gotcha:** the probe consumes one `VirtualClock` read per tick,
    so a `delay_next_reads` / `outlier_next_reads` meant for a resample must be
    scripted right before the resampling 100th tick (`resample_reading`
    helpers), and a persistent `step_utc` is followed by the next tick's
    probe, never by a resample.
- **Never backward.** A negative applied correction is a HOLD: the new anchor is
  `(instant + |applied|, wall(instant))`. The saturating read path freezes the
  wall for `|applied|`, then it runs exactly on the corrected line. That is
  ≤ 1 ms for a bounded resample. Since #224 part 2 a followed DATE step is
  never a hold (a relabel + a forward remainder); a follow holds only a
  residue of at most ~4 ms: a resample's armed 1 ms that already moved the wall
  further than r, a wall adopting another wall's N for a step it read up to
  3 ms smaller, or a backward step under 3 ms applied on its own (no epoch).
  A wall rejoining after an idle gap may hold its own drift (see "A date
  step relabels"). Never "simplify" a hold back to `(instant, wall − |applied|)`:
  that is a backward step, and right after an emit it relatches
  (`pacer_tests_wall_anchor.rs` asserts 0 relatches and 0 A/V corrections over
  10 000 boundaries with a preempted resample every other time, and across a
  followed −1.5 s step).
  - **What a hold does to the paced output** (a bounded resample's ≤ 1 ms, or
    a follow's residue of at most ~4 ms; the whole-step hold below is the
    pre-part-2 behaviour, kept for the mechanism): the pacer's wall
    resumes from the value it froze at. So the next boundary is simply the
    NEXT slot, serviced `|hold|` + one slot later: consecutive stamps, 0
    resyncs, 0 relatches, 0 A/V corrections. Only the real-time pause is
    visible. The pacer does not tick its wall while held (it ticks per serviced
    boundary). Neither do the `SP-program` sender, VBAN or the NDI input (they
    tick per wall boundary crossed). So no resample lands inside their own
    hold. The submit consumer ticks per job, and its next resample comes ≥ 100
    jobs (~3.3 s) after the follow (every follow restarts the count, #224),
    which is longer than a ≤ 1.5 s hold. Its step probe DOES run inside its
    hold (#224), and reads 0 there: it measures against the line, not the
    frozen value.
  - A resample NEVER runs inside a hold (#224 part 2 review round 2):
    `WallClock::tick` defers a due resample while `anchor.instant > now`, and
    it runs on the first tick after the hold (`frames_since_resample` keeps
    counting past 100). Measured against the frozen wall it would read the
    rest of the hold as a new backward step, cut it to 1 ms, and the probe
    would follow — and REGISTER — the rest as a fleet epoch (a rejoin's hold
    after a long idle gap can outlast the 100 ticks: 12 h at −94 ppm holds
    4.06 s; `a_resample_waits_out_a_rejoin_hold_longer_than_a_resample_period`).
  - **The boundary wait through a hold: spin budget, then yield (#147
    follow-up, design record 5852618200).**
    - Why it is needed: `pipeline_paced::sleep_to_boundary` coarse-sleeps to
      ~2 ms before the boundary, then waits in
      `pacer_spin::spin_to_boundary`. On the box the wall freezes a few µs
      PAST the emitted boundary (emit lateness), so `0 < until − now ≤
      interval` holds for either slot width (333 333 / 333 334) for the whole
      hold. The old unbounded spin therefore burned one core per paced thread
      through the ~1.5 s hold, at most once a night (a backward step).
    - The pure `spin_step(elapsed, delta, interval)` decides every check:
      - `delta ≤ 0` → Done;
      - `interval > 0 && delta > interval` → Bail (unchanged);
      - `elapsed ≤ SPIN_BUDGET` (3 ms, monotonic, from the start of the
        spin) → Spin;
      - otherwise → Yield (`thread::sleep(1 ms)`).
    - A normal boundary never reaches the budget, so its precision is
      unchanged. The spin normally starts ~2 ms (the coarse-sleep margin)
      before the boundary.
      - Worst case: a resample's ≤ 1 ms hold starts at the tick right after
        an emit. If the next wait's plan read lands inside that hold, the
        coarse sleep comes out short by the un-slept rest of the hold, so the
        spin starts up to 2 ms + that rest (< 1 ms) early. That is < 3 ms.
      - The inclusive budget covers it: the rest is always less than the
        whole hold, the coarse sleep's overshoot only shortens the spin, and
        std documents that `thread::sleep` never sleeps less. So that edge
        never yields and never logs the line below.
      - A hold near 1 ms is rare. It follows a UTC step or an outlier (a
        clamped resample, or the resample that takes it back out), never
        normal µs slewing.
    - A hold costs ~1 wake-up per ms, through the hold and through the slot
      after it. The wall resumes from its frozen value, so the awaited
      boundary is still ~one slot ahead, and the wait, already past its
      budget, keeps yielding.
    - That boundary goes out at most one yield after the wall REACHES it: up
      to ~1 ms nominally, plus the sleep's wake-up overshoot, which on the box
      can be several ms (the 19.9.2026 coarse-sleep finding above). It is the one
      boundary per hold without spin precision. It may count ONE late frame
      per paced thread per backward step, against `LATE_THRESHOLD_100NS` =
      2 ms (`pacer.rs`). Stamps stay contiguous either way.
    - A wait that yielded logs ONE INFO line, `paced: the wall stood still
      through a boundary wait` (`yields`, `spins`).
      - Since #224 part 2 a date step of either sign holds nothing (a
        relabel), so a step logs NONE. Before, a backward 04:00 step (~1.5 s
        hold) logged one per paced thread.
      - A follow's residue hold (of at most ~4 ms, rare) can outlast the 3 ms
        spin budget like any hold over ~1 ms: then ONE line for that
        boundary, next to the follow's INFO line. Any other line means
        something froze the wall (or the coarse sleep broke its contract):
        investigate it.
    - `pacer_spin.rs` is cross-platform and mutation-covered. It is NOT named
      `pipeline_paced_*`, because `.cargo/mutants.toml` excludes that
      substring.
    - Its thread test runs a real `Pacer` over a `WallClock::settable` wall
      that the test holds frozen. It witnesses every check through an observer
      hook and never times a check, so it is safe under ptrace. It asserts:
      the wait spins only within the budget, yields after it, never returns or
      spins again while the wall is frozen, yields at most once after the wall
      passes the boundary, and `yields × 1 ms ≤` the real time taken.
    - The virtual harness's `sleep_to` (`pacer_tests_wall_anchor.rs`) still
      has no spin at all.
  - **Test-harness gotcha:** `VirtualClock`'s preempted read places `m1` BEFORE
    a wall read the test made at the same virtual `t`. A backward hold at a WIDE
    bracket's midpoint then reads up to width/2 below that earlier read. It is
    an artifact of the harness, not a code path: in a real program every earlier
    read precedes `m1 ≤ midpoint`. Never assert `after == before` on a wide
    backward resample; assert the stats / the eventual follow instead.
- **Telemetry.** These are the PACER's wall clock (the one that stamps and paces):
  - `wall_anchor_max_step_us` — the largest MEASURED |delta|, i.e. what an
    unbounded re-anchor would have stepped;
  - `wall_anchor_wide_brackets` — anchors with every attempt disturbed;
  - `wall_anchor_slewed_us` — µs applied through clamped resamples;
  - `wall_anchor_steps_followed` — confirmed steps followed in one event,
    BOTH directions (#147);
  - `wall_anchor_last_step_us` — the whole step of the last one followed,
    SIGNED (≈ ±1 500 000 for a 04:00 dantesync 1.12.0 step, negative =
    backward);
  - `wall_anchor_holds_followed` — follows whose TIMELINE movement was a
    hold. Since #224 part 2 a followed date step moves the timeline forward
    by its remainder r (a backward step too), so this counts only residue
    holds of at most ~4 ms (a resample's armed 1 ms past r, an adopter reading the
    step up to 3 ms smaller, a backward step under 3 ms applied alone); it is
    NOT "the backward steps" any more;
  - `wall_anchor_last_hold_us` — how long the last such hold froze the
    timeline (of at most ~4 ms);
  - `wall_anchor_probes_rejected` (#224) — probes over 2 ms that were rejected
    (a wide probe, or a confirming sample that was wide or read another step);
  - `wall_anchor_detect_to_follow_us` (#224) — from the FIRST over-2 ms probe
    of the last followed step to its follow: ≈ 0 when the first probe
    confirmed, ~33 333 per rejected probe before it.

  They are on `/api/v1/ndi/health` `pacing` and on the `ndi: genlock` line.
- **What to read on the box.** `wall_anchor_max_step_us` in the hundreds of µs
  is dantesync slewing. Tens of ms with `wall_anchor_wide_brackets` climbing
  means preemption at anchor time, now outvoted or bounded. `steps_followed`
  +1 once a night (~04:00 local, dantesync 1.12.0) with `last_step_us` ≈ the
  announced step is the fleet date step, followed; the INFO log shows
  `direction=`, and since #224 part 2 `shift_slots` / `remainder_us` /
  `timeline_us` (every wall of the box logs the SAME `shift_slots` and a
  `remainder_us` within ~1 ms of the others, 3 ms at worst). Each wall
  anchors and follows independently, so every pacer and consumer logs its
  own follow — since #224 the `followed at once by the boundary probe` line,
  within one boundary of the step, `detect_to_follow_us` ≈ 0.
  `probes_rejected` climbing with no step is preemption at probe time. `wall_anchor_max_step_us` is a lifetime max: after
  the first ±1.5 s step it stays ~1 500 000; it does not drop back.
  `slewed_us` growing by more than ~1 ms per followed step means UTC steps
  that were NOT confirmed (a lone outlier, or preempted resamples).
- **Layout.** `PacingStats` lives in `playback/pacing_stats.rs` (split out of
  `ndi_health.rs` for the 1000-line cap) and is re-exported from `ndi_health`.

## `av_frame_offset` — SongPlayer's own emitted A/V relation (#148 v5)

Measurement only (design comment 5833341193). It was added to find out whether
the gate's steady −26 ms (`av_ms`, audio trails the picture) is produced by
SongPlayer's paced emission or further downstream. The receiver places audio
within 2 ms, and camera-box's own gate through the same OBS build reads ~0.

- **What it is.** At every productive paced boundary (fresh emit or repeat)
  `take_aligned_audio` records `audio_block_media_start − emitted_frame_pts` in
  ms (`pacer_av_align.rs::frame_offset_ms`):
  - the block start is the media head of the block handed to the sink:
    `head_media()` before `take_block` on the aligned path (output 0 = input 0,
    also on an underrun), or `expected` after a successful `align_to`;
  - the frame pts is the frame handed with it; on a repeat that is the
    repeated frame (`Pacer::service` passes `shown_pts`).

  Untimed audio, the anchor-less silence and the not-yet-aligned silence are
  skipped. Nothing reads the value back: no emission behaviour depends on it.
- **Sign.** Positive = the audio handed to NDI is from LATER media than the
  picture handed with it = the audio LEADS. This is the same sign as
  `av_align_err_ms` and the gate's `av_ms`, so a SongPlayer-caused −26 ms
  would read ≈ −26 here. (The design record's prose had the reading reversed;
  the formula is what ships.)
- **Window.** mean / min / max per UTC minute of the boundary stamp
  (`AvFrameOffset`). `stats()` reports the last COMPLETE minute (the minute
  before the pacer's current wall minute), so the `ndi: genlock` line logged in
  minute M carries ≈ the whole of minute M−1 (the pacer's wall minute and the
  heartbeat's UTC minute can disagree for ~1 ms at the rollover). A minute with
  no timed productive boundary (idle, paused) reads `0.0/0.0/0.0`.
- **Where.** `/api/v1/ndi/health` → `pacing.av_frame_offset_ms` /
  `av_frame_offset_min_ms` / `av_frame_offset_max_ms`, and the
  `ndi: genlock … av_frame_offset_ms=… av_frame_offset_min_ms=… av_frame_offset_max_ms=…`
  line (1 decimal).
- **Expected values (structural, not errors).**
  - A 30 fps source reads 0 (± 1 sample; the continuous correction's dead band
    lets it sit up to ±5 ms after a stall).
  - 24/25-fps content on the 30 fps grid reads a sawtooth of 0 … +33.3 ms
    (min 0, max ≈ one grid slot) with a mean of **+16.7 ms**. The picture is
    held on the grid (the newest frame whose present time ≤ the boundary)
    while the audio runs on the anchor line.
  - 23.976 fps reads min 0, max ≈ one source frame (41.7 ms) and a mean near
    +20.7 ms: the phase drifts through the grid within the minute.
  - Since #148 v6 these hold after ANY start position or seek, because the
    picture origin lands on the grid. A minute whose min sits clearly BELOW 0
    (not a stall) means the audio trails its picture line: a regression.
    `pacer_tests_av_offset.rs` derives every one of these analytically,
    including an off-grid landing.
  - A VIDEO stall reads POSITIVE: the pacer repeats the frozen frame while the
    250 ms audio cushion keeps the audio on its line, so the reading climbs
    +33.3 ms per stalled boundary (a minute's `max` of +100 … +250 ms = a stall
    in that minute). Only a stall longer than the cushion then leaves a
    NEGATIVE after-effect: the zero-filled blocks do not advance the head, the
    picture catches up by dropping frames, and the correction walks the
    negative reading back by ≤ 1 ms per block.
- **How to read it on the box.** Run the post-deploy A/V gate on SP-slow and
  note its UTC minute. Then read the `ndi: genlock ndi_name=SP-slow` line
  logged in the FOLLOWING minute (it carries the take's minute) from
  `C:\ProgramData\SongPlayer\songplayer.<date>.log`, or `/api/v1/ndi/health`
  once that minute has closed. Both use the same sign, so the part produced
  downstream of SongPlayer's sender ≈ gate `av_ms` − this mean:
  - mean ≈ the gate's `av_ms`: SongPlayer's emission produces the offset;
  - mean ≈ the structural value above while the gate reads −26: the error is
    downstream of the sender (fall back to design Approach 2).

  Not in the reading: the NDI timecodes themselves. Since #224 audio and
  video are both stamped on the serviced boundary (before, the audio carried
  the emit instant, later by the emit lateness, whole slots on a catch-up
  boundary). Cross-check `jitter_p99_us` and `lag` on the same line before
  attributing a residual to downstream.

## Standby = the same paced path as playing (#147, design comment 5841796900)

**The defect.** The cg OBS receiver read `recv-timing cap_avg` for
`sp-slow_video` at ~32 ms while SongPlayer was idle and ~15 ms while it played.
At every song start it fired `genlock-shallow-remeasure reason=rise`,
re-latched its depth from 2 to 3 frames, and slewed the audio for ~33 s. The
A/V gate landed inside that slew. The sender contract (camera-box
`docs/genlock-sender-contract.md`, §5–§6) asks for one cadence and phase,
whether the sender is idle or playing. The receiver's ASRC also follows the
audio ARRIVAL, so a standby with no audio restarted it at every song start.

**The rule.** On the paced path every boundary is the same audio + video pair,
whatever the state:

- **One audio block per standby boundary.** An emitting
  `Pacer::service_standby` (idle `Black` and paused `FrozenLast`; a starve
  takes none) hands the sink one block, `Pacer::standby_block` in
  `pacer_av_align.rs`:
  - `samples_per_boundary` zeros (1600 at 48 kHz, ~12.8 KB, under the round-10
    64 KB rule), built directly in interleaved form;
  - the song's channel layout when the grid buffer knows it, else stereo;
  - stamped on its boundary, like playing audio (#224).

  The video stamp comes from `resolve_emit_boundary`, exactly as for a playing
  repeat; a resync stamps `floor(now)`.
- **The EOS tail rides a standby boundary, never an audio-only send.** At a
  natural song end `decode_and_send_paced` calls
  `Pacer::hold_eos_tail_for_standby`, which holds the last partial boundary
  (`take_eos_tail`, #148 rework item 4). It then serves ONE more
  `FrozenLast` boundary (`serve_one_standby_boundary`); that boundary's block
  is the tail. The receiver therefore gets exactly one audio block per video
  boundary into the idle fill.
  - Removed with this: the old audio-only tail (`SharedHandoff` `eos_tail` and
    `submit_audio_tail`). Kept with it, it gave n+1 blocks per n boundaries at
    every song end.
  - A new map (`anchor`, lag re-anchor → `AvAlign::realign`) drops a held
    tail. With a Play already queued, the tail boundary is still served first,
    so nothing is lost; a stale tail is never replayed at a later pause.
  - A song that ends with no frame ever shown (`Starved`, e.g. a seek at EOS)
    has no frozen frame to pair with. Its tail rides the first idle `Black`
    boundary instead, or a new map drops it. It is still one block per boundary.
  - The song summary is logged BEFORE the tail boundary, so its frozen repeat
    is not counted as the song's.
  - `SharedHandoff`'s stop API is now just `stop()`; the queue, snapshot and
    counters are unchanged.
- **The idle fill uses the playing path's submit thread.**
  `pipeline_paced_idle::run_idle_wait` attaches a `PacedFeed` to the
  pipeline's paced output and emits through its `HandoffSink`, the same
  shape as `decode_and_send_paced` (see "The paced output services every
  boundary between scopes" below). The heartbeat goes through
  `emit_heartbeat_paced`. The black is `PipelineOutput::standby_black_nv12`,
  built once per pipeline.
- **No sync `send_video` and no BGRA.** The outer loop of `run_loop_windows`
  polls its commands without a wait (`try_recv`), so the idle fill starts on
  the very next boundary after start, a song end or a stop. (#221 lane 3
  deleted `idle_poll`, the SDK-clocked 5 s poll and its SYNTHESIZE BGRA
  standby.)

Tests:

- `pacer_tests_standby.rs` (over `FrameSubmitter` + `MockNdiBackend`, the
  real-wire rig):
  - idle, paused and playing boundaries produce the identical `MockNdiBackend`
    `send_audio spc=1600` → `send_video_async NV12` pair;
  - a whole paced lifetime has no sync send and no BGRA, with stamps b(1)…b(10)
    contiguous;
  - the standby stamp equals `resolve_emit_boundary`'s stamp and a playing
    repeat's stamp;
  - idle→play keeps the stamp and audio cadence contiguous;
  - a song end sends its EOS tail as the next standby block: 6 boundaries →
    6 blocks. With nothing buffered nothing is held, and a new song drops a
    held tail.
- `submitter_tests_standby.rs`: `SP-program`'s cached NV12 black.
- `pacer_tests_preroll.rs`:
  - a slow decoder open is filled with standby pairs (b(1)…b(9) contiguous,
    the song from the boundary after readiness);
  - a decoder that is ready at once starts on the very next boundary;
  - song end → next song keeps one pair per boundary;
  - `PrerollGate` reads the open result once, waits for the first frame, and
    ends at once on a failed open;
  - the song anchors on the waited boundary even when the clock moved past it;
  - a 17 ms first frame is preceded by the fill, not a hole;
  - a pause before the first frame is filled on every boundary;
  - a seek refill holds the pre-seek picture, and a new song drops the hold.
- **One path for the shared-picture standby.** The idle Black arm and
  `fill_starved` (pre-roll black, starve fill, held seek frame) both go through
  `Pacer::emit_standby_pair` (`on_emit` + `standby_block` + `submit_shared`).
  - The paused `FrozenLast` repeat of a real frame stays a playing-style repeat
    (counts `repeats`), with the same `standby_block`, but goes through
    `sink.emit_standby` (#215: a standby pair, not live for the program's cue
    gate; the default is `emit`, so the NDI submit is unchanged).
  - Both end in `submit_frame_at_boundary_owned`.
- **Song-change gap WARN.** `decode_and_send_paced` WARNs
  `paced: song change left > 8 boundaries unserviced (grid resync)` when its
  pre-roll resynced. `run_idle_wait` logs
  `paced idle: > 8 boundaries went unserviced …` for its idle stretch.
  - Grep the box log for both after a deploy.
  - Neither may ever appear during normal song changes.

Never "fix" a song-start re-latch by dropping the standby audio, or by sending
standby from another thread or in another format. That re-creates the cadence
change.

**The song-start pre-roll (ROZHODNUTÉ 5842127369).** A song start used to
leave a hole. The Play command stopped the idle fill, and
`decode_and_send_paced` then blocked on `open_rx` while the decoder opened.
Only then did it anchor, and the first boundaries could still be `Starved`.
Now:

- **Submit thread first.** `decode_and_send_paced` starts the #168 submit
  thread BEFORE the open.
- **`Pacer::preroll` fills every boundary** (`pacer_preroll.rs`). It services
  each boundary with the standby pair (the cached NV12 black + one silent
  block) through the same `HandoffSink`.
- **One readiness check per slot.** `preroll` asks `PrerollGate::poll` right
  before each wait: has the decoder opened (the `open_rx` result, read once)
  AND is its first frame buffered (`SharedQueue::is_primed`: a frame or EOS,
  never popping; or the producer died)?
- **Anchor on readiness, on the waited boundary.** When the gate says yes it
  calls `anchor_at(until)`, the boundary that wait was for, so a pts-0 first
  frame lands there. It never re-reads the clock (`anchor()`), because a
  preemption past the boundary would then skip it.
- **The standby fill: no starve is a hole.** The pre-roll's black stays in the
  pacer as its `StandbyFill`. Every `Starved` boundary goes through
  `Pacer::fill_starved`, which sends the black + one audio block (both the
  `service` and the `service_standby` FrozenLast starve arms). This covers:
  - a first frame whose pts lands after the anchor (a start position
    mid-frame), which then shows on its due boundary;
  - a seek's refill: `Pacer::anchor_seek` keeps the last pre-seek frame as the
    fill's `hold`, so the refill shows that frozen picture + silence, never a
    black flash and never a hole (the next song's pre-roll drops the hold);
  - a Pause queued during the pre-roll;
  - an empty file or a first-frame decode error.

  A pacer that never ran a pre-roll (bare unit tests) still starves
  silently.
- **A failed open ends the pre-roll at once:** stop + join the submit thread,
  then `DecodeResult::Error`.
- **Commands are held.** Commands queued during the pre-roll wait for the emit
  loop, the same as the old blocking open wait.
- **Coverage.** This covers idle→play, stop→play and song end→next song
  (after the EOS-tail boundary). Every boundary carries one audio+video pair.
- **Where it runs.** The pre-roll logic is pure and Linux-tested
  (`pacer_tests_preroll.rs`, `pacer_queue_tests.rs`); only the wiring in
  `pipeline_paced.rs` is Windows glue.

**Mutation-gate shape.** `preroll` uses `let … else`, never a `match` with a
`_` arm. Deleting the `Wait` arm would never call `poll`, so the mutant would
run to a 300 s TIMEOUT.

**Between songs: see the next section.** The window between two scopes used
to be unserviced (the old per-scope submit join + the producer join + the next
decoder open, 51–84 ms = 1–3 skipped slots on the box, 26.9.). The next
pre-roll's catch-up burst then overflowed the 2-deep handoff of a brand-new
submit thread, which coalesced away a stamp — camera-box's `stamp_gap`.

**Box acceptance** is read from camera-box's audit:

- no `genlock-shallow-remeasure` and no `shallow_latches` increment at a song
  start;
- `audio_pairing_offset_ms=0`;
- the same `cap_avg` idle and playing;
- one audio arrival per video boundary across a natural song end into idle
  (the EOS-tail wiring is Windows-only and mutation-excluded, so the box is its
  only check);
- then the A/V gate within ±20 ms on 5 consecutive songs.

## The paced output services every boundary between scopes (#147, design record 5845527884)

**The rule.** A paced pipeline has ONE submit thread for its whole life
(`paced_output.rs::PacedOutput`), never one per song or idle stretch:

- **Lifetime.** `PipelineOutput::handoff` (`paced_output.rs`, owned by the
  pipeline thread) spawns it on the first paced scope and returns the same
  `Arc<SharedHandoff>` after that; dropping the `PipelineOutput` stops it
  (drain the queue) and joins it. Since #221 lane 3 the consumer delivers to
  a `BoundaryOut` (the program bus), so there is no NDI sender, twin or
  flush on the pipeline side any more.
- **A dead thread is respawned by the next scope.** The thread only exits on
  stop, so a finished one panicked: `PipelineOutput::handoff` (called at
  every scope start) joins it, logs ERROR `paced output thread is gone —
  respawning it` (the join logs `paced output thread panicked`), and spawns a
  fresh one. A
  dead submit side costs at most the rest of one scope (the old per-scope
  `thread::scope` re-raised the panic at the song's end — also dark until
  then, and it took the pipeline thread down).
- **Scopes attach, never spawn.** `decode_and_send_paced` and `run_idle_wait`
  each hold a `PacedFeed` (RAII): `attach()` returns the newest stamp serviced
  or still queued (`PacedFeed::continue_after_100ns`), and the scope's pacer
  calls `Pacer::continue_grid_after(last)`
  (`pacer_preroll.rs`), so its first boundary (pre-roll or idle standby) is
  exactly one slot after it. Dropping the feed (end of song, command queued
  in idle, error, unwind) detaches. No flush, no join between songs.
- **The consumer fills only while DETACHED** (`paced_grid.rs::PacedGrid`,
  pure): once `strict_next(last_serviced) + ¼ slot` (`fill_grace_100ns`,
  83 333 × 100 ns) passes with no job queued, it services that boundary itself
  — the last submitted picture (else the standby black) + one silent
  `samples_per_boundary` block in the last audio layout, BOTH stamped exactly
  on that boundary, through the same submit + #209 program-bus path (a fill is
  offered like a standby pair). The fill's audio is stamped on the boundary,
  not with the raw send instant (§6): it leaves a grace late, and a raw-wall
  stamp would put that ~8 ms excursion into the receiver's audio timeline and
  A/V pairing for every filled slot. Every paced sender stamps its audio on
  its boundary the same way since #224. Woken > 8
  slots late, it resyncs to `floor(now)` and counts the hole (WARN `paced
  output: > 8 boundaries went unserviced between two scopes`); the resync rule
  is the pacer's own exact-grid emit gate (`paced_grid::fill_boundary`).
- **Logging is bounded per window**: INFO `paced output: no pacer attached —
  servicing boundaries` at a window's first fill, INFO `paced output: a pacer
  feeds again after the consumer's fills` (with `fills=`) when the next job
  ends it, DEBUG per fill in between — a stalled window never floods the log.
- **A fill counts as a late frame** (`late_frames`: it leaves ~8.3 ms after
  its stamp, > the 2 ms floor). That is honest and tiny — a few per song
  change against 1 800 slots a minute.
- **While a pacer is attached it owns every boundary.** The consumer never
  fills then: a fill would steal the boundary of an emit that is merely late
  (> 8.3 ms), and the pacer's aligned audio block for it would be lost (a
  33 ms dropout mid-song). The pacer's own catch-up and > 8-slot resync are
  unchanged (the WARN path for a hung box).
- **The handoff over is atomic.** A fill reserves its stamp under the handoff
  lock (`HandoffState::next_step`), and `attach()` reads, under the same lock,
  the NEWER of `last_serviced` and the newest job still QUEUED — at a natural
  song end the EOS-tail boundary can sit behind a slow submit when the idle
  scope attaches ~1 ms later, and continuing after anything less re-emits it
  (a stale drop, or a coalesce into a real hole; #147 review round 2). So a
  boundary is never serviced twice or skipped. The song-change window (in
  which a stamp gap counts into `song_change_unserviced_slots`) closes only on
  the first stamp AFTER that point — the old pacer's queued tail never closes
  it, so a hole before the new pacer's first stamp is still counted.
- **A job at or before the last serviced stamp is never sent** (the output's
  stamps only increase); it counts as a submit-side `dropped`.

**Telemetry** (`SubmitCounters` → `merge_pacing_stats` → `PacingStats`, on
`/api/v1/ndi/health` `pacing` and the `ndi: genlock` line):

- `song_change_unserviced_slots` — grid slots nobody serviced across a
  detach→attach window (a fill resync, or a gap before the next pacer's first
  job). **Must read 0.**
- `consumer_fill_pairs` — boundaries the consumer serviced itself; a few per
  song change / idle→play is normal (the window between two scopes: the old
  producer's MF + stems teardown, then the next scope's setup — the next
  decoder's open itself runs in the producer during the ATTACHED pre-roll),
  a steady climb while a song plays is not (nothing fills while attached).
- `dropped` at a `wall_anchor_steps_followed` increment must stay +0: since
  #224 part 2 a date step never makes a pacer emit back to back (its timeline
  moves by r < one slot), so the 2-deep handoff has nothing to coalesce. A
  REAL stall still catches up (up to 8 back to back) and may.

**Tests** (`paced_output_tests.rs`, single-threaded over a recording sink, the
`Recorder` of the bus delivery since #221 lane 3, on ONE settable clock; 8×2 / 12×2 song frames and a 4×2 black name each boundary's
picture): a song change with 3 slots between the scopes AND a decoder whose
open takes 3 more pre-roll slots, play → pause → resume → paused song change,
and idle → play all give stamps with Δ = exactly one slot, the fills holding
the last picture (same buffer) + silence stamped on the boundary and
`song_change_unserviced_slots = 0`; plus the > 8-slot resync count, the
attached pacer owning the grid, stale jobs, stop draining the queue, the fill's
audio layout, the live thread filling on its own, the spawn-once + drop
order, and a gone thread respawned by the next scope; the consumer delivers
to a `Recorder` (`BoundaryOut`), and `paced_output_tests_bus.rs` delivers to
a real `ProgramBus` (#221 lane 3). `paced_grid_tests.rs` pins every
comparison of the pure grid.

**Box acceptance** (#148 A/V series): across the E2E scene cuts the
`ndi: genlock` line reads `song_change_unserviced_slots=0`, and camera-box
reports 0 `stamp_gap` on sp-* sources.

## Every audio block is stamped on its boundary (#224)

A boundary's audio block belongs to that boundary's timeline instant: its
first sample plays at the boundary, like the picture stamped on it. So every
paced sender stamps the audio `timecode` with the boundary, never the instant
it happens to be sent:

- the pacer (`service`, `service_standby`, the starve / standby pair): the
  stamped boundary, also after a resync (`floor(emit_now)`);
- the submit consumer's fill (#147, already);
- `SP-program`: its own standby pair (released up to the 3-slot fill grace
  late — its audio used to read up to ~100 ms late) and its mixed blocks are
  stamped on their boundary; a forwarded source job keeps the source's stamps
  (now its boundary too);
- the NDI input: its FrameSync block on the boundary it serves
  (`NdiInput::service(B, bus)`).

The camera-box contract §6 says "raw wall clock at submission": for an
on-time emit that IS the boundary (the paced emit spins to it). A catch-up
burst after a stall (or, before #224 part 2, a date step) used to stamp every block with ~the same
emit instant, so cg OBS (timecode audio mode) placed the burst on top of
itself — `audio_place_err_ms` / the placement sawtooth on camera-box issue
1381. VBAN has no timecode (packets are scheduled from the pair's video
stamp). Tests pin it: `pacer_tests.rs` (the mixed
catch-up/resync run, with the never-future-dated check read from the
settable wall at each emit), `pacer_tests_lane4.rs`, `pacer_tests_standby.rs`,
`program_output_tests.rs`, `program_bus_tests.rs`, `ndi_input_tests*.rs`.

## Merge gate for pacing/decode/NDI/audio changes: the post-deploy A/V gate (#147)
A change to pacing, the submitter, decode, the mixer, NDI or the audio path
merges only with `e2e/post-deploy-av-sync.spec.ts` green. That spec records the
OBS program and requires |A/V| ≤ 40 ms and zero 50 ms dropout blocks against
the original sidecars. Method, thresholds and how to read the `AV-SYNC …`
output: `.claude/rules/obs-ndi-health.md` "Post-deploy A/V gate (#147)".

## A date step RELABELS time, it does not move content (#224 part 2)

Design record 5899388193; camera-box confirmed the receiver side
(5900288123); the trigger was the 20:58Z +260 ms step (5898834252: a VBAN
burst of ~80 packets, a 270 ms hole + relock in the cg OBS recording). The
root cause was `pacer.rs` `present = wall_start + pts`: content was mapped
onto the LABEL clock, so a forward step became a catch-up burst of up to 8
slots and a backward step a pause of |S|, on every paced sender (pacer,
submit consumer, `SP-program`, NDI input, VBAN).

- **One continuous internal TIMELINE; fleet labels only at the NDI wire.**
  P = 10⁷/30 (one slot, 100 ns), D(K) = ⌈K·P⌉ (`fleet_shift::shift_100ns`).
  A confirmed step S (probe or resample path, either sign, any armed 1 ms
  included) splits into N = ⌊S/P⌋ whole slots (a RELABEL) and the remainder
  r = S − (D(K+N) − D(K)), 0 ≤ r ≤ one slot (`fleet_shift::split`). Table at
  K = 0: +260.3 ms → 7 / 26.97 ms, +219.03 → 6 / 19.03, +90 → 2 / 23.3, +50 →
  1 / 16.7, −19.8 → −1 / 13.53, −51.039 → −2 / 15.63, −1.5 s → −45 / 0.
- **`WallClock`** keeps its UTC anchor/probe/resample/confirm logic; the UTC
  anchor takes the whole S (`WallClock::regrid`). `now_100ns()` returns the
  timeline `UTC − D(K_w)` (it passes the timeline anchor to
  `ClockSource::read_100ns`, so the settable test clock, which ignores the
  anchor, never sees a relabel). The timeline moves by `applied − ΔD` through
  `apply_anchor_step`: r forward, or a residue HOLD of at most ~4 ms — never
  backward, never S. Every existing call site keeps working unchanged.
  `WallClock::line_100ns` reads the timeline's LINE through a hold (VBAN's
  clock reads it); a regrid's `last_jump_100ns` is signed.
- **The registry** `fleet_shift::FleetShift` (process-wide `global()`, a
  `OnceLock`; `WallClock::system()` uses it, `WallClock::new` builds a
  private one, `WallClock::with_fleet` injects one — tests NEVER use the
  global one). The first wall to confirm a step registers an epoch {S, N}
  (ONE INFO line `fleet shift: a date step registered`); a later wall
  adopts the ΣN of the run of its unapplied epochs (none included) whose
  summed S lies CLOSEST to its own reading, when within
  `STEP_RESIDUE_100NS` = 3 ms (`fleet_shift::adopt`; a tie keeps the
  shorter run) — two walls either side of a slot multiple still move by ONE
  N; a wall two epochs behind adopts both. The difference is the two walls'
  line errors, each ≤ 1 ms (a lone-outlier resample, review round 1) +
  ≤ 0.31 ms slewing lag: it is NEVER a new epoch, and neither is any step
  within 3 ms of nothing registered (each wall applies it alone, N = 0), so
  a registered epoch is always over 3 ms. (Registering such a residue,
  −1.3 ms → N = −1, left every other wall's stamps a slot stale for good; at
  2 ms the fuzz still split K with two opposite outliers.) An adopter's r
  can leave [0, P] by ≤ 3 ms; a wall that missed a whole epoch and sees two steps as one can
  jump more than a slot (never in practice: walls follow within a boundary,
  steps are hours apart).
- **Joining the fleet: the published line** (review round 1). A wall that
  was not WATCHING the clock cannot tell its own drift from a date step, so
  it must never register one. Every `WallClock::tick` publishes the wall's
  line (`FleetLine`: its anchor, K_w, epochs applied) into the registry.
  `FleetShift::join` gives a wall built now the line published at most
  10 s ago (`WALL_REJOIN_IDLE`) with that wall's K — so a wall built between
  a step and its registration starts on the pre-step line and follows the
  step itself — else its own sample at the current K (`FleetShift::current`).
  A wall that ticks again more than 10 s after its last tick REJOINS the
  same way (`WallClock::rejoin`: a jump ahead or ONE hold onto the joined
  line, `regrids` +1 so its tick's net line movement lands in `moved_100ns`
  and VBAN owes it, the resample count restarted like a follow's, nothing
  registered, INFO `wallclock:
  ticked again after over 10 s idle — rejoined the fleet's line`). That is
  the legacy per-frame submit wall between songs: idle 20 min at ±30 ppm it
  drifted ±36 ms, and registering that relabelled every paced sender (33 ms
  future-dated stamps). At ≤ 94 ppm a wall ticking within 10 s drifts under
  1 ms, below the 2 ms threshold; every loop wall ticks at least every
  ~100 ms. The pacers, the submit consumers, the program and VBAN walls
  always keep a fresh line published. Known bound (review round 2): a wall
  anchored on its OWN wide sample (no fresh line — the first wall of the
  process, or a rejoin with every wall idle — and all 8 attempts preempted
  over 6 ms) registers that anchor error as an epoch at its first probe;
  every other wall joins its line and adopts it, so K stays one fleet-wide
  value (the stamps stay right, the timeline moves once by the remainder).
- **The wire edge** is `FrameSubmitter::submit_frame_at_boundary_owned`: the
  pair's internal boundary b becomes `floor_boundary(b + D(K_F))`
  (`fleet_shift::wire_stamp_100ns`), K_F read ONCE per pair from the
  submitter wall's registry. The audio stamp moves by the VIDEO's relabel,
  `audio + (wire(b) − b)` (review round 1: flooring it broke the program's
  forwarded source offsets), so audio = video wherever the source stamped
  them equal (every paced path). D rounds up so an on-grid b
  lands EXACTLY K slots later; N rounds down, so a stamp is never
  future-dated: a pacer wall that has not followed yet stamps at most r
  stale (it used to be S stale). Since #221 lane 3 this is `SP-program`'s
  submitter, the ONLY wire edge (the per-playlist senders, the legacy
  `submit_nv12` and the #192 emitter's own labels are deleted). `SubmitJob`,
  the bus keys and VBAN's `due` stay internal.
- **What each output does** (pinned in virtual time):
  - pacer: the boundary after the step comes r early (ONE interval shrinks
    by r), content one frame per boundary, internal stamps contiguous, the
    wire jumps N+1 slots once (+260.3 ms: 8; −19.8 ms: 0 = the same wire
    stamp twice, camera-box: `stamp_dup` +1, no relock); a REAL stall still
    advances the timeline by the real gap and is caught up (+200 ms → 6
    back to back): stall vs step is told apart by structure, there is no
    "just stepped" flag, and `pacer.rs` catch-up / resync / 1 s re-anchor are
    untouched (`pacer_tests_wall_anchor.rs`);
  - paced output + program bus with walls following at different ticks:
    filled / late_dropped / coalesced / resyncs / handoff `dropped` all 0
    (`program_bus_tests_regrid.rs`);
  - NDI input: its `WallVbanClock::new` follows the wall, so its next
    boundary comes at most ONE early, never a burst
    (`ndi_input_tests_regrid.rs`);
  - VBAN: `WallVbanClock::slewing` (policy SlewRemainder, `vban-out.md`).
- **Readers that compare realtime with stamps read the timeline**:
  `fleet_shift::timeline_now_100ns()` (UTC − D(K_F)) in
  `program_bus::persist_and_cut` (the cut fallback) and
  `scene_off.rs` (`scene_off` / `scene_off_due`).
- **Logs and API fields that SHOW a stamp show the WIRE stamp**
  (`fleet_shift::wire_100ns` / `label_100ns`): `ProgramBus::status()` / the
  cut answer (`cut_boundary_100ns`, `health.last_stamp_100ns`,
  `transition.active.start_boundary_100ns`, via `ProgramStatus::on_wire`),
  the paced output's fill / resync lines, the NDI input's relatch / resync
  WARNs, VBAN's substitution WARN. `ProgramCore::status()` itself stays
  internal. The per-minute `av_frame_offset` window keys on the pacer's
  timeline minute, which sits D(K) off the UTC minute after a step.
- **Telemetry**: `PacingStats.fleet_shift_slots` (the pacer wall's K) and
  `last_regrid_remainder_us` (its last r), also on the `ndi: genlock` line;
  every follow's INFO line carries `shift_slots`, `remainder_us`,
  `timeline_us` (the timeline's own movement); VBAN `slew_owed_us`.
- **K accumulates** over the process lifetime (the net of every step; a
  restart = a deploy resets it to 0). The timeline then sits D(K) off UTC —
  harmless, every internal comparison is timeline against timeline.
- **Tests**: `fleet_shift_tests.rs` (the table, the knife-edges, a sweep of
  0 ≤ r ≤ one slot, the wire mapping, the registry),
  `wallclock_tests_regrid.rs` (walls on one registry: the 1.3 ms residue,
  the join race, the 20 min idle rejoin at ±30 ppm with and without a step
  in the gap, the exact 10 s threshold, the line through a hold),
  `wallclock_tests_probe.rs` / `_confirm*.rs` / `_anchor.rs` (the relabel
  at the wall), `submitter_tests_regrid.rs` (floored, never above the
  virtual fleet clock, for a followed and a lagging wall; audio = video; a
  forwarded audio offset survives the relabel),
  and the output tests above. Pins come from a scratch Python model of the
  wall + the split + each harness.
- **Box acceptance** at a controlled camera-box step of each sign: every
  wall logs the same `shift_slots` and ~the same `remainder_us`; the resync /
  relatch / dropped / filled / late_dropped deltas are 0; a dev1 VBAN capture
  has 0 bursts and 0 gaps over ~5.2 ms; camera-box reports
  `released=followed` with a residual ≈ r and relocks unchanged; an A/V gate
  take spanning the step passes with 0 dropouts.
