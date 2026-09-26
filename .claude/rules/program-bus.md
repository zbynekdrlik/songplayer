---
paths:
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/playback/paced_output*.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/program_control.rs"
  - "e2e/program-control.spec.ts"
---

# Program bus + NDI `SP-program` (#209, B1 of EPIC #174)

SongPlayer is the master switcher: its own NDI output `SP-program` carries the
playlist output cut to it. Design record: #209 comment 5844972899.

## How a boundary reaches the program

- The paced submit thread (`paced_output.rs::PacedConsumer::submit`, the
  pipeline-lifetime consumer since #147) copies its boundary job (`program_bus::program_copy`: an `Arc` bump of the
  `SharedFrame` + the one audio block) BEFORE its own submit moves the frame
  into the holdover, and offers it right AFTER that submit
  (`ProgramBus::offer(pid, job)` — it takes NO clock, see below). Only a
  source that can own a
  program boundary pays for the copy, but EVERY paced source records its
  progress on every boundary (`touch`, inside `program_copy`). Without that,
  the source you cut to looks absent until its first owned frame lands, and
  a slow first frame (> the sender's b+1 ms check) turned the cut boundary
  black (#209 review finding).
- Both the playing path and the idle fill go through that ONE submit thread,
  so an idle source offers its own #147 standby pair and a cut to it carries
  that pair — no special case. Since #147 (design record 5845527884) the
  thread also services the boundaries BETWEEN two scopes (a song change, a
  decoder open, idle→play) with the held picture + silence, and offers those
  fills the same way: a cut source no longer black-fills the program at its
  song changes.
- The bus reaches the submit thread as a process-wide `OnceLock`
  (`program_bus::install`, done once in `PlaybackEngine::start_program`), NOT
  through `PlaybackPipeline::spawn` — `pipeline.rs` is at the 1000-line cap.
  Only ONE test in the whole sp-server test binary may call `install`
  (`the_installed_bus_is_the_first_one`); a second one would race it.

## Ownership, order, fill (all pure, `ProgramCore`)

- **One clock domain: the stamps.** The API's realtime clock and a submit
  thread's `WallClock` can both sit off the pacer walls after a UTC step: an
  unconfirmed step slews in at ≤ 1 ms per resample, and a CONFIRMED forward
  step (the ~+50 ms dantesync fleet date step) is followed in one event at
  each wall's second resample after it (#147) — the walls' resample phases
  differ, so for ≤ ~3.3 s two walls sit up to one step (~1.5 slots) apart,
  inside the 3-slot fill grace below.
  So: a cut is placed from the newest stamp of the program's segment sources,
  the source cut to, or the program itself (`from`; the caller's clock only
  when nothing was seen); a boundary is declared missed BY TIME only in
  `release` on the `SP-program` sender's own long-lived wall, which it ticks
  ONCE PER GRID BOUNDARY (`program_output::BoundaryTicker`) — the pacer walls'
  cadence, so both slew a step in at the same rate (ticking per loop wake
  slewed 2x faster and black-filled the owner after a forward step — third
  review); `offer` reads no clock (it forwards and fills only a gap its owner
  is already past, `owner_passed`). Residual: a pipeline CREATED mid-slew
  anchors at the stepped time — after a backward step its stamps trail the
  program wall by the unslewed rest, so the grace fills its boundaries until
  the walls converge (rare: a step + a new playlist pipeline + a cut to it). Never pass a submit thread's clock into the bus again —
  the re-review showed it black-fills the owner's own boundaries after a
  forward step.
- Cut state = `(first_stamp, pid)` segments. A cut lands on
  `strict_next(strict_next(from))` = next boundary + `CUT_LEAD_SLOTS` (1).
  The previous owner keeps every stamp before it. A second cut on the same
  boundary REPLACES the first (the replaced source never owned anything); a
  cut back to the source that still owns that boundary just cancels the
  pending cut.
- A stamp-ordered reorder buffer releases strictly one boundary after the
  other: the new source's first frame waits for the old source's last one.
- A missing boundary is filled with the program's own standby pair: on the
  offer path only when its owner already touched/offered a LATER stamp (one
  source works in stamp order) or the reorder buffer overflows; on the
  sender's wall once it is reached AND (that | no owner | the owner is quiet
  > 1 s | 3 slots passed). More than 8 missed slots resync (like the pacer) —
  measured against the sender's wall, or the waiting frame on the offer path
  — never a black burst.
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

- **Mock call-log gotcha (CI fail 26.9.):** a test that asserts the mock sender's LAST
  call (e.g. `send_video_flush`) must keep the owning output alive past the assertion —
  a thread closure that drops it appends `send_destroy` after the flush. Return the output
  from the thread (`let _out = thread.join().unwrap();`).
