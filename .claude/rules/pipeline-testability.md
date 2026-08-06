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
