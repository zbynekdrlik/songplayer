---
paths:
  - "crates/sp-server/src/playback/pipeline.rs"
  - "crates/sp-server/src/playback/pipeline_heartbeat_tests.rs"
  - "crates/sp-server/src/playback/pipeline_inline_tests.rs"
  - "crates/sp-server/src/playback/submitter.rs"
---

# pipeline.rs testability — generic `FrameSubmitter<B>`, not hardcoded `RealNdiBackend`

`decode_and_send` / `run_loop_windows` genuinely need `sp_decoder`
(MediaFoundation), so they stay `#[cfg(windows)]`-only with no Linux test
path — that part is a real, unavoidable platform constraint.

But `FrameSubmitter<B: NdiBackend>` itself is **already fully generic and
cross-platform** — `submitter.rs`'s own tests exercise it on Linux via
`sp_ndi::test_util::MockNdiBackend`. Helper functions that only touch the
submitter (heartbeat emission, frame-rate bookkeeping, anything that doesn't
call into `sp_decoder` directly) do **not** need to be hardcoded to
`FrameSubmitter<sp_ndi::RealNdiBackend>` + `#[cfg(windows)]` just because
they happen to live in the same file as the Windows-only decode loop.

**#133 found exactly this:** `emit_heartbeat` / `run_heartbeat_inner` /
`run_heartbeat_outer` were all needlessly narrowed to `RealNdiBackend` +
`cfg(windows)`, even though nothing in their bodies touches
MediaFoundation — pure `FrameSubmitter<B>` method calls
(`drain_window`, `sender().get_no_connections`, `nominal_fps`,
`last_submit_ts`, `frames_submitted_total`). Genericizing over
`B: sp_ndi::NdiBackend` and widening the cfg gate to
`#[cfg(any(windows, test))]` — the same pattern `should_run_heartbeat` /
`classify_bad_poll` already used — unlocked a real Linux-CI regression test
via `MockNdiBackend` for a bug that would otherwise have been "no Linux test
path, trust the Windows E2E suite" by default.

**When adding a new pipeline-thread helper: default to generic +
`cfg(any(windows, test))` unless the function body genuinely needs
`sp_decoder` types.** Check what it actually calls before reaching for
`#[cfg(windows)]` out of habit / proximity to `decode_and_send`.

Also note: `emit_heartbeat` / `run_heartbeat_inner` / `run_heartbeat_outer`
carry `#[cfg_attr(test, mutants::skip)]` — kept as-is for those three
(still exercised only indirectly through real MockNdiBackend-driven tests,
not worth re-litigating per PR), but a NEW function you add and directly
unit-test (like `run_heartbeat_paused`) should generally NOT carry
`mutants::skip` if you're already asserting its exact output.

## The #192 wall-clock audio emitter — pure core + generic send seam are Linux-tested

`playback/audio_emitter.rs` is the model for keeping a genuinely Windows-bound
feature (a TIME_CRITICAL OS thread that submits to the live NDI SDK) almost
entirely Linux-testable. The split:

- **Pure, cross-platform, `#[test]`-covered on Linux CI:** `AudioRing`
  (bounded FIFO, never drops), `AudioEmitter::tick(now) -> Emitted` (the
  wall-clock grid: one block or a full silence block per slot, grid timecodes,
  silence/late/jitter accounting), `push_blocking` (bounded back-pressure via
  `Mutex`/`Condvar` — driven with a real second thread in the test),
  `emitter_stats`, and the generic **send seam** `emit_one_block<B: NdiBackend>`
  which is exercised with `sp_ndi::test_util::MockNdiBackend` (assert the exact
  `send_audio(42,sr=48000,ch=2,spc=1600)` call list — the same
  `submitter_tests_timecode.rs` pattern). This is what lets a Linux test prove
  "the decode-side push does NOT send_audio; the emitter thread does" and
  "silence is a full 1600-sample block, not a gap".
- **Windows-only, no Linux test path (`mutants::skip`, box-verified):** ONLY the
  thread lifecycle in `pipeline_audio.rs` — the spawn +
  `THREAD_PRIORITY_TIME_CRITICAL`, the `WallClock` sleep-until + spin loop, and
  the `AudioEmitterThread` join-before-sender-drop guard. Nothing there decides
  behaviour; it just drives the pure core on a real clock/thread/sender.

The `sp_ndi::AudioSink` (a cloneable `{Arc<B>, handle}` audio-only send handle)
is the seam that makes this possible: the pure `emit_one_block` takes an
`AudioSink<B>`, so a `MockNdiBackend` sink drives it on Linux while a
`RealNdiBackend` sink drives it on the box — never a `#[cfg(windows)]`-narrowed
signature for logic that has no MediaFoundation dependency.

## #192 round 4 — video follows the audio clock (`av_catchup.rs`)

Round 3's 1.5 s cushion stops a producer stall reaching the speakers but leaves a
lasting A/V offset: the wall-clock emitter keeps its grid while the SDK-clocked
video, submitted at decode time, resumes ~1.4 s late and the loop (≈ real time
under a heavy child) never catches up. The ring depth IS the video's lag —
`lag_ms = target_depth_ms − ring_depth_ms`, `target = DEFAULT_TOLERANCE_MS +
AUDIO_LOOKAHEAD_MS` (`audio_emitter::target_ring_depth_ms()`, never a literal).

- **Pure, Linux-tested, mutation-scored (`playback/av_catchup.rs`):** the free fn
  `decide(ring_depth_ms, target_depth_ms, frame_ms, primed)` (Drop only when
  primed AND lag > one frame) and `CatchUp{primed, consecutive_drops}` — the
  prime latch at target − one frame (`depth + frame ≥ target` — exactly where `decide` already says Submit, so the priming frame is never dropped; the initial fill is not a
  stall) and the `MAX_CONSECUTIVE_DROPS` (75 ≈ 3 s) safety valve (submit one frame
  anyway so a stuck decoder never blacks the wall). `CatchUp::reset()` clears BOTH
  fields on the `clear_ring` sites (seek arm + new play). Exact-boundary tests on
  every threshold; no `while`, `.max()`/`.min()`-clamp-friendly, so a flipped
  comparison fails a mutant, never spins into a timeout.
- **Windows-only glue (`pipeline_audio::is_late_frame`, `mutants::skip`):** locks
  the ring, reads the live `AudioEmitter::ring_depth_ms()` (same helper the
  heartbeat logs), and applies `CatchUp::step`. The decode loop (`pipeline.rs`)
  has EXACTLY ONE `if` at the submit site: on `Drop` it still pushed the audio,
  skips `submit_nv12`, and calls `loop_stage.observe_drop(decode_us, audio_us)`.
- **Counter:** `catchup_dropped` accumulates per heartbeat window through
  `LoopStageMax::observe_drop` → `LoopStageStats` → `LoopStats` → the
  `pipeline: loop-stats` line (`catchup_dropped=N`) — the direct producer-stall
  meter. Box acceptance: a stall that drains `ring_ms` < 700 is followed within
  1 s by `ring_ms` ≥ 1400 and `catchup_dropped` > 0, with zero `silence_blocks`
  mid-song.
- **SDK-clocked path only.** The catch-up runs only when the wall-clock emitter
  exists — i.e. the `genlock_pacing == false` branch. The paced/genlock path has
  its own re-latch logic and is byte-for-byte untouched (see `genlock.md`).
