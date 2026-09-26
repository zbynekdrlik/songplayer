---
paths:
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/playback/pipeline_paced_submit.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/program_control.rs"
  - "e2e/program-control.spec.ts"
---

# Program bus + NDI `SP-program` (#209, B1 of EPIC #174)

SongPlayer is the master switcher: its own NDI output `SP-program` carries the
playlist output cut to it. Design record: #209 comment 5844972899.

## How a boundary reaches the program

- The paced submit thread (`pipeline_paced_submit.rs::run_submit_consumer`)
  copies its boundary job (`program_bus::program_copy`: an `Arc` bump of the
  `SharedFrame` + the one audio block) BEFORE its own submit moves the frame
  into the holdover, and offers it right AFTER that submit
  (`ProgramBus::offer(pid, job, submit_done)`). Only a source that can own a
  program boundary pays for the copy, but EVERY paced source records its
  progress on every boundary (`touch`, inside `program_copy`). Without that,
  the source you cut to looks absent until its first owned frame lands, and
  a slow first frame (> the sender's b+1 ms check) turned the cut boundary
  black (#209 review finding).
- Both the playing path and the idle fill go through that ONE submit thread,
  so an idle source offers its own #147 standby pair and a cut to it carries
  that pair — no special case.
- The bus reaches the submit thread as a process-wide `OnceLock`
  (`program_bus::install`, done once in `PlaybackEngine::start_program`), NOT
  through `PlaybackPipeline::spawn` — `pipeline.rs` is at the 1000-line cap.
  Only ONE test in the whole sp-server test binary may call `install`
  (`the_installed_bus_is_the_first_one`); a second one would race it.

## Ownership, order, fill (all pure, `ProgramCore`)

- Cut state = `(first_stamp, pid)` segments. A cut lands on
  `strict_next(strict_next(from))` = next boundary + `CUT_LEAD_SLOTS` (1),
  where `from = max(caller's now, newest stamp any source offered)`: the API
  reads the realtime clock, the sources stamp on their slewed `WallClock`, and
  after a UTC step the realtime clock can lag — the clamp keeps the cut off
  boundaries already emitted. The previous owner keeps every stamp before it.
  A second cut on the same boundary REPLACES the first (the replaced source
  never owned anything); a cut back to the source that still owns that
  boundary just cancels the pending cut.
- A stamp-ordered reorder buffer releases strictly one boundary after the
  other: the new source's first frame waits for the old source's last one.
- A missing boundary is filled with the program's own standby pair only once
  it is reached AND (no owner | the owner already offered a LATER stamp — one
  source offers in stamp order | the owner is quiet > 1 s | 3 slots passed).
  More than 8 missed slots resync (like the pacer), never a black burst.
- An owned frame for an already-served boundary is `late_dropped`. A frame
  from a source that no longer owns anything is `NotOwner` (its segment is
  pruned once the cut boundary is served).
- Bounds: program queue 10 (a full 8-slot gap fill + the frame), reorder
  buffer 16 (over it the missing boundary is forced). `release` is bounded to
  64 steps per call, so a mutant can never spin into a 300 s timeout.

## The sender + startup order

- `program_output.rs::run_program_loop` (Windows thread `program-output`,
  `mutants::skip`) wakes 1 ms after every grid boundary (`next_check_wait`) to
  release missed boundaries, and submits every queued job at once.
- `start_program` runs in `lib.rs::start` AFTER the #196 startup senders, so
  `SP-program` is created after every playlist sender and the per-playlist
  name→port order does not change across restarts. Exception: when the
  startup senders exceed their budget (the timeout branch), `SP-program` may
  be created while late playlist senders are still coming up.
- A dead program sender (no NDI SDK, sender creation failed, the SDK-clocked
  `genlock_pacing=false` path offers nothing) shows as `health.submitted`
  not rising and a stale `last_stamp_100ns` on `GET /api/v1/program`; the
  cause is in the log (`SP-program` lines).
- The selected source persists as setting `program_source` (written BEFORE
  the cut; a failed write cuts nothing) and is restored with
  `select_initial` (owns every boundary, `cut_boundary_100ns: null`).

## API + UI

- `GET /api/v1/program` → `{ndi_name, source, previous, cut_boundary_100ns,
  health{forwarded, filled, late_dropped, resyncs, coalesced, cuts,
  submitted, connections, last_stamp_100ns}}`; `POST /api/v1/program/cut
  {"source": pid}` → 200 + that body, 404 unknown playlist.
- `/api/v1/ndi/health` is unchanged (an array of per-pipeline snapshots
  consumed by sp-ui + e2e); where the program's health also belongs there is
  an open question on #209.
- sp-ui `ProgramControl` (dashboard, testids `program-control`,
  `program-source`, `program-cut` + `data-playlist-id` + `aria-pressed`,
  `program-error`) polls `GET /api/v1/program` through `store::poll_into`.
  The mock (`e2e/mock-api.mjs`) keeps program state in memory —
  `POST /__mock/program-reset` in `beforeEach`/`afterEach`.

## Tests

`program_bus_tests.rs` drives `ProgramCore` + a real `ProgramOutput` over
`MockNdiBackend`: source A frames are 4×2 NV12, B 8×2, the program's standby
black 2×2, so the `send_video_async(…,WxH,…)` call strings name the owner of
each boundary. Keep that pattern for any new case.
