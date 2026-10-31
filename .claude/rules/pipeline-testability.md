---
paths:
  - "crates/sp-server/src/playback/pipeline.rs"
  - "crates/sp-server/src/playback/pipeline_paced.rs"
  - "crates/sp-server/src/playback/paced_producer*.rs"
  - "crates/sp-server/src/playback/pipeline_stub.rs"
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

## The decode producer's items are cross-platform (#257 step 2)

`pipeline_paced::run_decode_producer` (Windows: the MF reader, the STA
thread) only opens the readers and pushes. Every item it pushes and every
preview offer is built by `paced_producer::DecodeProducer` (`next_item`,
`seek`; `to_paced_frame`, `QueuedFrame`), which takes the cross-platform
`SplitSyncedDecoder` and is Linux-tested over mock readers
(`paced_producer_tests.rs`, and with the real pacer in
`pacer_tests_song_end.rs`) and mutation-gated: its file name keeps it out
of the `pipeline_paced` exclusion. A new producer decision goes there, never
into the Windows loop. It is on the no-sender and no-spawn source scans.

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

## Every Play is answered exactly once, in order (#229 follow-up)

The engine counts the Plays a pipeline was sent and has not answered
(`failure_backoff::PlayAnswers`): only the answer to the LAST Play sent
records a play or counts a failure. That needs exactly one `Started` or
`Error` per Play, in command order: the stub's one `Error`; on Windows the
pre-roll's `Started` or the failed open's `Error` (the pre-roll reads no
command). #277: a video that ends early before its first picture answers
with its `Error` instead of `Started` (checked right after the pre-roll,
`SharedQueue::failure`); after `Started` the one `Error` it may still send
carries `early_end::after_start`'s suffix and is counted as no answer. A
new answer path, a second answer, or an answer that can overtake an
earlier Play's breaks it (`open-failures.md`, "Which Play an answer belongs
to", "A video that ends early").

## An early end is decided in the producer, not the loop (#277)

`DecodeProducer::next_item` measures where the picture stopped against the
video reader's own duration (`early_end::EarlyEnd::of`) and returns no held
step for an early end; the Windows loop only WARNs, marks the queue
(`producer_failed`) and turns `SharedQueue::failure` into
`DecodeResult::Error`. Tests: `paced_producer_tests_early.rs` (mock readers
with a container duration apart from their frames).

## Heartbeat helpers carry `mutants::skip` only where the effect is glue

`emit_heartbeat_paced` (11 args, `pipeline_paced_submit.rs`, Windows-only)
is glue: it reads the pacer's stats and the paced output's snapshot and
sends one `HealthSnapshot` event. A NEW function you add and directly
unit-test should generally NOT carry `mutants::skip` if you're already
asserting its exact output.
