---
paths:
  - "crates/sp-server/src/playback/pipeline.rs"
  - "crates/sp-server/src/playback/pipeline_heartbeat_tests.rs"
  - "crates/sp-server/src/playback/pipeline_inline_tests.rs"
  - "crates/sp-server/src/playback/pipeline_tests_no_sender.rs"
  - "crates/sp-server/src/playback/paced_output.rs"
  - "crates/sp-server/src/playback/submitter.rs"
---

# pipeline.rs testability — the decode loop is Windows-only, its decisions are not

`run_loop_windows` / `decode_and_send_paced` genuinely need `sp_decoder`
(MediaFoundation), so they stay `#[cfg(windows)]`-only (and `mutants::skip`)
with no Linux test path — that part is a real, unavoidable platform
constraint. Everything they DECIDE lives in cross-platform code with Linux
tests: the `Pacer`, the paced output (`paced_output.rs`), the heartbeat's
`should_run_heartbeat` / `classify_bad_poll` (`#[cfg(any(windows, test))]`).

**When adding a new pipeline-thread helper: default to cross-platform (or
`cfg(any(windows, test))`) unless the function body genuinely needs
`sp_decoder` types.** Check what it actually calls before reaching for
`#[cfg(windows)]` out of habit / proximity to the decode loop (#133 found
three heartbeat helpers narrowed for no reason, which hid a Linux-testable
bug).

## A playlist pipeline has NO NDI sender (#221 lane 3)

SongPlayer broadcasts only `SP-program` (NDI, `program_output.rs`) and
`SP-program-MAX` (Spout). A playlist's pipeline feeds the program bus and
nothing else:

- The paced consumer delivers every boundary to a `BoundaryOut`
  (`paced_output.rs`): in production `InstalledBus` (the process-wide
  program bus, `program_bus::installed()`, through `offer_to_bus`), in the
  tests a `Recorder` or a bus of the test's own (`paced_output_tests_bus.rs`).
  Never give the pipeline side an NDI sender again — the structural guard
  `pipeline_tests_no_sender.rs` fails on any of `NdiSender`,
  `FrameSubmitter`, `new_with_clocking`, `send_video_async`, `send_audio`,
  `get_no_connections`, `ndi_backend` in the pipeline-side sources (comments
  included, so do not even name them there).
- `FrameSubmitter<B>` (`submitter.rs`) is `SP-program`'s submitter. It stays
  generic over `B: NdiBackend`, so its tests run on Linux over
  `sp_ndi::test_util::MockNdiBackend`; its `PacedSink` impl and the
  borrowed-slice `submit_frame_at_boundary` are `#[cfg(test)]` (the pacer
  tests' real-wire rig), production submits `submit_frame_at_boundary_owned`.
- The SDK-clocked path (`decode_and_send`, the `genlock_pacing` switch, the
  #192 wall-clock audio emitter, the #192 round 4 `av_catchup`) is DELETED
  (#221 lane 3, owner directive "delete legacy directions"): pacing is the
  only path. Its history is in the git log of `pipeline_audio.rs`,
  `audio_emitter.rs` and `av_catchup.rs` (deleted at 0.71.0-dev.16).

## Heartbeat helpers carry `mutants::skip` only where the effect is glue

`emit_heartbeat_paced` (11 args, `pipeline_paced_submit.rs`, Windows-only)
is glue: it reads the pacer's stats and the paced output's snapshot and
sends one `HealthSnapshot` event. A NEW function you add and directly
unit-test should generally NOT carry `mutants::skip` if you're already
asserting its exact output.
