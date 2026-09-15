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
- Burn-id QR overlay (#151, run_id **911014**): the paced emit paints a QR of
  `P{run_id}.{frame_id}.{gen_ts_ns}.{crc32}` bottom-right (side `0.28·h`, margin
  `40/1080·h` — camera-box `payload.rs` + `burn-geom.hpp`, ported into
  `sp_core::genlock::burn`; luma-only 16/235, chroma neutral 128) so the fleet's
  `recording-verdict` proves contiguity for SP-originated frames.
- The burn is **default OFF, NEVER persisted, paced-path ONLY**: toggled per
  output via `POST /api/v1/ndi/burn {output,on}` (204 / 404 / 409 "pacing
  disabled"), read fresh every boundary through a shared `Arc<AtomicBool>`
  (`NdiBurnRegistry`), surfaced as `burn_on` in `/api/v1/ndi/health`. A QR must
  never reach the LED wall in production — a structural guard keeps the legacy
  `decode_and_send` path from ever referencing the overlay.
