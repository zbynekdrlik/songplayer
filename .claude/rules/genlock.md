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
