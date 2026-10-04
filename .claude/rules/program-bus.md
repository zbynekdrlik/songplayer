---
paths:
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_on_air*.rs"
  - "crates/sp-server/src/playback/program_authority*.rs"
  - "crates/sp-server/src/playback/runtime_pipeline.rs"
  - "crates/sp-server/src/playback/engine_play.rs"
  - "crates/sp-server/src/api/routes_status.rs"
  - "sp-ui/src/components/player.rs"
  - "e2e/player-off-program.spec.ts"
  - "crates/sp-server/src/playback/scene_catalog*.rs"
  - "crates/sp-server/src/playback/legacy_cg*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/playback/program_canvas*.rs"
  - "crates/sp-server/src/playback/band_pool*.rs"
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
- #215: every offered pair carries `SubmitJob::live` — `true` only for the
  source's own decoded content (the pacer's `PacedSink::emit`, an NDI input
  capture), `false` for every standby pair (`emit_standby`, the
  `default_submit_shared` black / fill / held frame, `fill_job`, the input's
  standby). The cue gate (`program-transition.md`) opens a fade on the first
  live pair; ownership and forwarding ignore the flag.
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
  unconfirmed step slews in at ≤ 1 ms per resample, and a step over 2 ms
  (the dantesync fleet date step) is followed in one event by each wall's
  step probe at its own next tick (#224; before, at each wall's second
  resample, #147). Since #224 part 2 a follow RELABELS (`genlock.md` "A date
  step relabels"): the whole slots move only the wire labels, every wall's
  timeline moves by the remainder r (< one slot), so for ≤ ~one boundary two
  walls sit at most r apart — inside the 3-slot fill grace for ANY step
  (`program_bus_tests_regrid.rs`: 0 fills / late drops / coalesces /
  resyncs with the program wall following a boundary before the source).
  The stamps the bus keys on are internal (timeline) boundaries; a reader
  without a wall uses `fleet_shift::timeline_now_100ns()`: the cut fallback
  in `persist_and_cut`, the scene-go-off re-check.
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
- #215: every cut ALSO opens a transition window (a crossfade of both
  sources over `n` boundaries, `ProgramJob::Mix`); a Cut is the zero-length
  window, i.e. exactly the rules above. The window, the mix, the deferred
  scene-go-off pause and the OBS follow are in `program-transition.md`.
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
- #210: `ProgramOutput::serve` hands each boundary's audio block to VBAN
  FIRST, then does the video side (#223: a source picture's canvas fit, a
  mixed boundary's picture, the NDI submit), for every job kind — by
  structure: `split` (the audio side, no
  video work) → `feed_vban` → `submit_video` (`vban-out.md` "Data path"; pinned by
  `program_output_tests_order.rs` with a held NDI send). It returns the
  boundary's `BoundaryMarks`, which the loop records
  (`ProgramBus::record_timing` → `health.timing`, the rate-limited WARN of a
  VBAN hand-off after its block's first packet was due, over VBAN's send
  latency L, #210 part 2; `vban-out.md` "The program boundary's timing"). `ProgramOutput::submit` is only the tests' shorthand
  (`#[cfg(test)]`, no clock, returns the stamp).
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

## `SP-program` is ALWAYS 1920×1080 (#223)

The owner's rule (ROZHODNUTÉ 28.9.2026 on #223): `SP-program` is always
1920×1080. cg OBS, the Presenter, the stage displays and the stream never get
a 1440p or a 4K picture, and its size never changes at a cut, a fill or a
fade. Before, it carried the on-air source's own size (2560×1440, 2560×1080,
2560×1280, 2048×858, 1920×960 in one evening on the box). Design record:
#223 comment 5972302641 (this slice; the full design, `SP-program-MAX`
included, is comment 5872871751).

- **The canvas** (`playback/program_canvas.rs`, `Canvas`): the program's ONE
  picture layout, w×h NV12, stride w, w·h·3/2 bytes. Production builds the
  sender with `ProgramOutput::fhd(sender)`: `PROGRAM_STANDBY_W/H` =
  1920×1080, 3 110 400 B (the standby's own constant; the FHD tests use the
  same constructor). `ProgramOutput::new(sender, w, h)` takes any size: the
  unit tests use small canvases, so they stay fast.
- **Every picture** `ProgramOutput::submit_video` sends is a canvas picture:
  - a forwarded source pair's picture (a paced source's, the NDI input
    "OBS manuál"'s, a paced standby/fill pair's);
  - the program's standby black (the canvas black itself);
  - a fade's picture, in ONE pass from both sides (`Canvas::fade`, #223
    follow-up, design record 5973498519): each side as it is when it is a
    canvas picture, fitted into the canvas as it is read when it is not, the
    canvas black when it is missing, the incoming side blended over the
    outgoing one (`program-transition.md`, "The mix"). The mix is exactly the
    canvas's bytes.
- **Pass-through.** A picture with the canvas's size and stride and AT LEAST
  its bytes (a decoder buffer may carry slack; the buffer's OWN length
  counts, not what its layout claims) goes out as the SAME allocation: no
  copy. The paced idle black, the program standby and a
  1920×1080 song decoded on a 1920 stride are all canvas pictures.
- **The fit.** Any other picture is fitted into the canvas: placed by
  `nv12_fit::aspect_fit` (aspect kept, centred on even offsets, bars Y 16 /
  UV 128), scaled bilinear by the #215 fused kernel as a plain fit
  (`mix_nv12_into` with `Paint::Fit(Side::Fitted)`: ONE side, every byte the
  fitted picture's, nothing else read — #223 follow-up; before, it also
  read the 3.1 MB canvas black as an unused second side) — in the sender's
  `mix_bands` row bands on its band pool, into a `frame_pool` buffer.
  A larger picture is scaled down, a smaller one up (the 720p dabing songs),
  a 16:9 one fills the canvas; 2560×1080 sits on rows 134 to 943. A padded
  stride at 1920×1080 is a repack (a fit at scale 1 copies every visible
  byte). A buffer short of the canvas's bytes is not whole NV12: it is drawn
  as the canvas black, never read past its end.
- **Plans.** One `FitPlan` per source layout; the canvas keeps the
  `FIT_PLANS_KEPT` = 2 used last (both sides of a fade fit every boundary and
  rebuild nothing) and logs ONE INFO line per new plan: `program output: a
  picture of a new size — fitted into the SP-program canvas` (`width`,
  `height`, `stride`, `canvas_width`, `canvas_height`, `plans_built`). The
  thread-start INFO line names the canvas (`width`, `height`, `mix_bands`,
  `mix_workers`).
- **Cost, on the box.** The fit is video-side work: `serve` = `split` →
  `feed_vban` → `submit_video`, so VBAN gets the boundary's block before any
  fit. `submit_video` reads `submit_start` BEFORE the picture work, so
  `health.timing.submit_us` (`vban-out.md`) is the fit (or the fade's
  picture) + the NDI submit.
  - **Row bands from the first deploy.** The lane's dispatch said to reuse
    the kernel's row bands; the slice's design record named them as the
    first lever only AFTER a box measurement (main session to confirm). A
    single band is likely over budget: box run 3 measured a fitted 1440p
    fade picture at 13.2–20.0 ms on 6 bands (#215 comment 5860381820).
  - **What it costs.** One pass per picture on EVERY forwarded boundary of
    a source that is not 1920×1080 (most of the catalog is 1440p), and one
    pass per fade boundary whatever its sides. The bands run on the
    sender's persistent band pool (`band_pool.rs`, #223 follow-up): band 0
    on the `SP-program` thread, band i on `program-mix-<i>`, the K − 1 = 5
    workers started once with the output and joined when it is dropped —
    no thread start per picture. Before the follow-up each painted pass
    spawned K − 1 scoped threads (~150 thread starts a second with a 1440p
    song on program), a fade with a non-canvas incoming side took two
    passes, and a plain fit also read the canvas black. SpeedHQ now encodes
    FHD, not 1440p (44 % fewer pixels), which pays part of it back.
  - **Box check, a 1440p song on program, then a 1920×1080 one:**
    - the program's `submit_us_max` / `submit_over_5ms`;
    - the size a receiver gets: any NDI receiver of `SP-program` (NDI Studio
      Monitor on the box) reads 1920×1080 with a 1440p and with a 2560×1080
      song on program (the thread-start INFO line prints only the configured
      canvas; #223's planned plain-receiver probe, slice S2, automates it);
    - the collateral on the other threads (on Windows every thread start
      runs each loaded DLL's thread attach under the loader lock): the
      on-program source's paced `pipeline: loop-stats … submit_call_us_max`
      and its `ndi: genlock … late=` line, against the same reads before
      this deploy;
    - a 300 ms AND a 1 s fade between two 1440p songs, the regression case:
      a fade between two same-size non-FHD songs was one byte-for-byte
      blend before #223, two passes after it (10.7–30.7 ms per boundary,
      `submit_us_max` up to 86 ms in the E2E's fades, #223 comment
      5973492929), and is ONE pass on the band pool since the follow-up
      (design record 5973498519). Its acceptance: `max_picture_us` under
      ~12 000 for a 9-boundary fade, steady `submit_us_max` near the
      pre-#223 15 ms, and `health.filled` / `late_dropped` / `coalesced`
      (and `cue_timeouts`) +0 over the fades.
  - **Levers, in order**, if `submit_us_max` passes one slot (33 333) or the
    collateral moves: the first two are built (#223 follow-up: persistent
    band workers, a one-pass fade); next, one band for a plain fit if the
    band hand-off (K − 1 worker wakes per picture) ever dominates; then the
    output buffer's memset (`out.resize` in `mix_nv12_into`: one serial
    3.1 MB write on the `SP-program` thread before the bands start, ~0.3 ms
    at FHD, ~4× that on a 4K canvas) — the bands could initialise their own
    part instead (it needs `MaybeUninit` slices, i.e. `unsafe`).
- **Out of this slice** (later #223 slices, comment 5872871751): the
  `SP-program-MAX` output for the LED walls (max(FHD, native)), its
  `MaxSide`, Spout, the zero-receiver gate, downloads above 1440p.
- Tests: `program_output_tests_fhd.rs` (the production canvas: 2560×1440,
  1280×720 and 2560×1080 sources → 1920×1080 stride 1920, the quadrants
  scaled not cropped, the 21:9 bars; pass-through as the same allocation,
  with slack; a padded stride repacked, a short buffer drawn black; a fade
  between two sizes drawn in the canvas; a 1440p↔1440p fade, one plan, both
  sides scaled), `program_canvas_tests.rs` (the layout, the pass-through
  rule, the two kept plans, the fit at every band count reading its picture
  alone, `Canvas::fade`'s one-pass pins and its plan order),
  `band_pool_tests.rs` (the persistent workers: where each band runs, the
  waits, a panic, the shutdown), `program_output_tests.rs` (small
  canvases), and
  `ndi_input_tests_fhd.rs` (the input's 4×2 picture goes out 1920×1080). The
  wire picture's bytes are read through `FrameSubmitter::held_frame`, the
  async holdover the SDK still points at.

## API + UI

- `GET /api/v1/program` → `{ndi_name, source, previous, cut_boundary_100ns,
  health{forwarded, filled, late_dropped, resyncs, coalesced, cuts,
  submitted, connections, last_stamp_100ns, timing{…} (#210, the sender's
  per-boundary stage timing, `vban-out.md`)}, vban{…} (#210), input{…}
  (#212), remote{…} (#213), transition{…} + follow{…} (#215),
  legacy_cg{shown} (#221 L4a)}`;
  `POST /api/v1/program/cut {"source": pid}` → 200 + that body, 404
  unknown playlist. Source `-1` is the #212 NDI input "OBS manuál" (404 unless
  it is enabled with a source) — see `ndi-input.md`.
- The ONE cut path is `program_bus::persist_and_cut` (persist first, then cut),
  shared by the API and the #213 Companion remote control (`remote-control.md`).
  #221: it takes the scene name the cut is published with ("What is on air"
  below). #221 L4a: the API reaches it through the switch path
  (`program_switch::switch_source`, `via=dashboard`, under `switch_order`,
  recorded as `remote.last_remote_cut`, a playlist mirrored to cg OBS) —
  `remote-control.md` "The dashboard cut on the same path".
- #221 L4a: `ProgramBus::legacy_cg()` is SongPlayer's record of what it told
  cg OBS to show (`playback/legacy_cg.rs`, `remote-control.md`), served as
  `legacy_cg {shown}` on both program answers; `restore_selected_source`
  records the restored playlist there. Until B4 step 6.
- The stamps `ProgramBus::status()` and the cut answer show
  (`cut_boundary_100ns`, `health.last_stamp_100ns`,
  `transition.active.start_boundary_100ns`) are WIRE stamps
  (`ProgramStatus::on_wire`, #224 part 2): what a receiver sees, the
  internal boundary + D(K_F), floored. `ProgramCore::status()` stays
  internal.
- `/api/v1/ndi/health` is unchanged (an array of per-pipeline snapshots
  consumed by sp-ui + e2e); where the program's health also belongs there is
  an open question on #209.
- sp-ui `ProgramControl` (dashboard, testids `program-control`,
  `program-source`, `program-cut` + `data-playlist-id` + `aria-pressed`,
  `program-error`) polls `GET /api/v1/program` through `store::poll_into`.
  The mock (`e2e/mock-api.mjs`) keeps program state in memory —
  `POST /__mock/program-reset` in `beforeEach`/`afterEach`.

## What is on air (#221 L1, design record 5873773896 §1b, §1d)

- `ProgramBus` publishes an `OnAir {seq, source, scene}` on a
  `tokio::sync::watch` (`playback/program_on_air.rs`; `on_air()` = a
  receiver, `on_air_now()` = a copy). It is published INSIDE `cut` (under
  the state lock, so in cut order) and `select_initial`, and `seq` grows on
  EVERY publication — also a cut to the source already selected (a bus
  no-op: `ProgramCore::cut` returns `false`, `health.cuts` does not move),
  so a same-scene press is still an event, and a manual → manual press
  (-1 → -1) publishes the new scene name.
- Always `send_modify`, never `send`: `send` DROPS the value while no
  receiver exists, and `restore_selected_source` runs in `start_program`
  before any task subscribes.
  `the_startup_selection_is_published_before_anyone_subscribes` pins it.
- Every publisher names the scene: `persist_and_cut(pool, bus, pid, scene)`
  / `ProgramBus::cut(pid, now, scene)` / `select_initial(pid, scene)`. A
  press passes the scene pressed (a playlist's by its catalog name;
  "OBS manuál" itself passes none); the restore the playlist's catalog scene
  (`scene_catalog::scene_of_source`, `None` for -1), the dashboard cut the
  same through `switch_source`'s catalog read (L4a); the OBS follow the cg
  OBS scene it follows — only when it CUTS (`follow_scene` skips a source
  already on program), so a followed manual → manual change publishes
  nothing and the published scene stays the earlier one (matters for L3's
  feedback while the follow still runs; L5 deletes the follow). Tests that
  do not care pass `None`.
- `program_on_air::program_scene_name(&OnAir)` is the ONE name resolver:
  the scene, else "OBS manuál" (`PROGRAM_INPUT_LABEL`) for -1, else none.
  It never asks cg OBS.
- The scene catalog (`playback/scene_catalog.rs`): a scene is a PLAYLIST
  scene when exactly one ACTIVE playlist's `ndi_output_name` equals it,
  ignoring ASCII case (only ASCII folds); its name is that NDI name
  lowercased. An empty or shared NDI name names no scene (WARNed once per
  process per conflict). Pinned with the 10 live names.
- `ProgramBus::switch_order` (a `tokio::sync::Mutex`) orders the scene
  switches of the #221 switch path (`remote-control.md`). It is separate
  from `cut_serial`, which only orders persist + cut.

## The playback authority (#221 L4b, design record 5873773896 §1e)

SongPlayer's own program decides what PLAYS; cg OBS's scene detection starts
and pauses nothing (the OBS→engine bridge, `scene_change_commands` and
`EngineCommand::SceneChanged` are deleted; a cg OBS disconnect pauses
nothing).

- **On air** = `program_on_air::on_air_set(&on_air, legacy_cg.shown)`:
  SP-program's source when it is a playlist (-1 "OBS manuál" is none) ∪ the
  playlist SongPlayer last told cg OBS to show. The union exists only until
  B4 step 6 (the legacy consumers still take cg OBS): a dashboard cut to -1
  while cg OBS shows sp-fast keeps sp-fast playing (the input carries it),
  and so does a mirror that failed. Right after every playlist press BOTH
  playlists are on air until cg OBS answers the mirror (tens of ms); the
  outgoing one is then held by its window (`Hold::Until`) as before.
- **The task** `program_authority::run_program_authority` (spawned in
  `start_program`, after the restore and the startup re-mirror) watches
  `ProgramBus::on_air()` and `legacy_cg().shown()`. Its first value (the
  restored program) and every change of either become
  `PipelineEvent::OnProgram(bool)` on the engine's OWN event channel
  (`on_air_changes(previous, current, cut_to)`: OFF for every playlist that
  left, then ON for every playlist that entered and for `cut_to`, the
  source of a NEW publication (the task tracks `seq`) = the re-kick, so a
  press of the scene already on air plays a playlist paused out of band).
  **A member nobody cut to is never re-kicked** (review round 1, F1; it
  amends the design record's "ON for every member", a main-session call):
  with the union, the outgoing playlist cg OBS still shows, one a dashboard
  cut to -1 keeps on air, and one a manual press keeps a moment would each
  be re-kicked, and a playlist the operator PAUSED would start a new song
  (its resume point lost). A shown-only change (the mirror's OK) re-kicks
  nothing: OFFs, and an ON only for a playlist that entered (an older
  press's OK landing after a newer cut). It ends on shutdown or when the
  engine's channel is gone.
- **The engine drops a stale event** (`PlaybackEngine::on_program`): ON
  only while the playlist is on air, OFF only while it is not — "on air"
  being the set the task last DIFFED (`OnAirPlaylists`, the engine's
  `on_air`, written by the task BEFORE it sends that value's events), never
  the live bus (review round 1, F2). The bus can change and change back
  between two task wakes (the watch coalesces) and the task then sends
  nothing, so an event checked against the live bus could be dropped with
  no newer one behind it; against the diffed set a dropped event always
  has one. So once the task diffed a cut to it, the selected source is
  never taken off program — `Hold::OnProgram` / `CUT_SETTLE` are deleted
  (`program-transition.md`) — and a hold's re-check (`scene_off_recheck`)
  leaves a playlist in the diffed set alone even while its scene is still
  off: its ON is queued behind the re-check (review round 2: A→B, A
  pressed again inside B's window). The window left is the task's own wake
  latency: an OFF or a re-check handled after a cut back but before the
  task diffed it pauses the playlist, and the ON then starts a new song.
- **The wall after an OFF** (`scene_off::wall_after_scene_off`, review
  rounds 3-4): the incoming ON comes at the press and the outgoing OFF only
  at the mirror's OK, so another playlist may already be on program with
  its title and line up. With one on program (in the diffed set) its due
  title is re-synced, none due fades the outgoing title, and its line is
  re-sent at once; details in `program-transition.md`, "The wall after an
  OFF".
- **One wall owner** (release 0.69.0 review 🟡 2, design record 5908252887
  item 2). The on-air set can hold two playlists (a failed or late mirror, a
  dashboard cut to "OBS manuál"), and there is ONE LED wall, ONE title clip
  and ONE stage display. So:
  - `program_on_air::wall_owner(&on_air, shown)` (pure, next to
    `on_air_set`) = SP-program's source when it is a playlist, else
    `legacy_cg.shown`, else none — always a member of the set, none only
    for an empty set;
  - the authority publishes it with the set (`OnAirPlaylists::publish`,
    before that value's events; the log line carries `wall_owner`);
  - `OnAirPlaylists::may_write_wall(pid)`: while there is an owner, only it
    writes the shared outputs — the `ShowSubtitles` / `HideSubtitles`
    dispatch and the Presenter push (`position_update.rs`), the song-end /
    PlayVideo clear of both (`clear_lyrics.rs`), both title timers
    (`title_timers.rs::WallGate`, read when they fire), the Play re-sync
    (`resync_after_play`) and a re-sync's title candidates and lines
    (`recovery.rs::title_candidates` / `on_program_lines`, so also the
    Resolume recovery and `wall_after_scene_off`);
  - the other member keeps playing for the consumers that still take cg OBS
    (until B4 step 6) and keeps its per-playlist karaoke WS, but writes none
    of them; its dispatch forgets what it last sent, so the moment it owns
    the wall its current line goes out;
  - the owner can change through the new owner's ON alone (a cut to B
    while cg OBS still shows A), and the old owner then writes nothing —
    its hide timer, song-end clear and Presenter pushes included. So the
    ON of the playlist that owns the wall (`OnAirPlaylists::owner`)
    re-syncs the WHOLE wall at once (`scene_off::wall_after_owner_on`,
    review rounds 1-2): the title (a playing owner's scene-on already
    sends its `Resync`; an owner that plays nothing sends one naming no
    title), the line (`resync_wall_lines`, the line step
    `wall_after_scene_off` also runs: the owner's line, or one
    `HideSubtitles`) and the stage display (`resync_presenter`: the
    owner's line at its last position, recorded as its Presenter dedup
    key, or `presenter::push_empty`). Nothing of the old owner stays
    frozen on the title clip, `#sp-subs` or the stage display;
  - the owner can also change through an OFF alone (a cut to "OBS
    manuál" while cg OBS still shows another playlist: the owner goes
    from 4 to 7 with only OFF(4)). `wall_after_scene_off` re-syncs the
    title and the line to the playlists still on program (the owner's)
    and ends with `resync_presenter(owner)` (review round 3); after the
    OFF of a member that did not own the wall that repeats the owner's
    current line, or clears the stage display while the owner is in a
    blank stretch (its dispatch would hold its last line there): the
    display goes blank like the wall until the owner's next line (review
    round 4; rare: a failed mirror, then cg OBS put on the program's own
    scene);
  - with NO owner (nothing on air, or before the authority's first value:
    the engine's unit tests) nothing is restricted, as before: a playlist
    whose OFF is still queued writes until its OFF re-syncs the wall, and a
    song played off program by hand feeds the Presenter
    (`lyrics-display.md` "Who may send a line"). While a playlist owns the
    wall, a hand-played off-program song no longer does.

  Pinned by `program_on_air_tests.rs` (the owner table),
  `program_authority_tests.rs::the_authority_publishes_the_wall_owner_with_the_set`
  and `tests_wall_owner.rs` (a child of `tests_scene_change.rs`: the line +
  Presenter, a playlist that comes to own the wall, the song-end clear, both
  title timers, the new owner's ON (`the_new_owner_s_on_re_syncs_the_wall_s_line`,
  `a_new_owner_that_plays_nothing_takes_the_old_owner_s_title_down`), an
  owner change by an OFF (`an_owner_change_by_an_off_re_syncs_the_stage_display`)
  and the recovery). A test that only `replace`s the diffed set
  (`#[cfg(test)]`) publishes no owner.
- **A runtime pipeline** (`EnsurePipeline`) of a playlist already on air
  whose scene is not flagged runs `handle_scene_change(pid, true)` itself
  (its ON came before it existed). An ON for a playlist with NO pipeline
  creates it (`ensure_pipeline_for_playlist`, then the same guard): the
  #196 startup senders run before `start_program`, but past their 45 s
  budget the rest were never created (review round 1, F3).
- **A manual ▶ claims nothing** (`PlayEvent::Start`: WaitingForScene +
  Start → SelectAndPlay; `handle_engine_play` fires `VideosAvailable` +
  `Start`). Off air the playlist plays OFF program: `scene_active` stays
  false, no title goes to the wall, the WS state is `WaitingForScene` with
  transport `Playing`, which the Player labels "Hrá mimo programu"
  (`sp-ui` `player.rs`; "Čaká na scénu" otherwise). The mock's `/play`
  models it from its program source (`e2e/player-off-program.spec.ts`).
  An off-air ▶ on a playlist with no normalized video yet parks it in
  WaitingForScene, and `on_video_processed` wakes only on-program
  pipelines: the operator presses ▶ again once a video is ready (before
  L4b the ▶ claimed program, so the download started it).
- **The WS state follows a scene flip** (`engine_play.rs::broadcast_scene_flip`,
  called at the end of `handle_scene_change`, review round 6). A ▶'d
  playlist that then goes on air stays `Playing`, so `apply_event` (which
  broadcasts only a raw state change) told the dashboard nothing and it
  kept "Hrá mimo programu" for the rest of the song; a hold stays `Playing`
  until its pause. The flip itself is broadcast when the raw state did not
  change but the wire state (`play_state_to_ws`) did, so a pause or a new
  song is still broadcast once. The mock models it: a program cut to a
  playlist its `/play` started off air broadcasts `Playing`. Every engine
  `PlaybackStateChanged` goes through `engine_play.rs::broadcast_state`
  (review round 7): a new broadcast site calls it, never builds the
  message inline. #225: it sends through `send_dashboard`, which records
  it for the WS on-connect replay (`dashboard-ws.md`), so a new dashboard
  is told the same state; there is no separate replay builder any more.
- **`/api/v1/status`**: `active_scene` = the one resolver,
  `active_playlist_ids` = the on-air set, ascending
  (`api/routes_status.rs::on_air_fields`; `routes.rs` is at the cap). A
  post-deploy read right after a press must wait for the set to SETTLE (at
  most one playlist on air): `readEngineActiveScene` /
  `waitEngineActiveScene` in `post-deploy.spec.ts`, the A/V gate's baseline
  poll (`length === 1`).
- **The startup re-mirror** (main-session decision 1, comment 5884501960):
  `program_switch::remirror_on_air`, in `start_program` once the OBS link
  is attached, sends the restored playlist's catalog scene to cg OBS ONCE
  through the ticketed mirror (a ticket under `switch_order`,
  `Upstream::enqueue(…, supersedes = true)`, `startup_answer` +
  `confirm_mirror`), so `legacy_cg.shown` (seeded by the restore) is what
  cg OBS was told. A restored -1, nothing restored, or a playlist whose
  catalog names no scene sends nothing; no `last_remote_cut` (nothing was
  pressed). If cg OBS does not connect within the mirror's wait (3 + 4 s)
  the call is dropped as abandoned and `shown` keeps the seeded value.
- Tests: `program_on_air_tests.rs` (the set + changes tables),
  `program_authority_tests.rs` (the task over a real bus: first value,
  press + confirm, re-kick, -1 keeps cg's playlist, the diffed set,
  shutdown / gone engine; the engine's stale check against the diffed set
  on an in-memory DB, the lazy pipeline, and — task + engine together — a
  paused playlist through cuts that did not press it),
  `tests_runtime_pipeline.rs`,
  `tests_play_video.rs` (the off-air ▶), `routes_tests.rs` (status),
  `program_switch_tests.rs` (the re-mirror).

## Tests

`program_bus_tests.rs` drives `ProgramCore` + a real `ProgramOutput` over
`MockNdiBackend`: source A frames are 4×2 NV12, B 8×2, the program's standby
black 2×2. Since #223 the sender puts every picture on the wire in its 2×2
canvas, so the wire size no longer names a boundary's owner: the rig's
`Program` records the size of the job each boundary handed the sender
(`shown_dims`: a source's own size, the standby's 2×2, a mix's incoming side
else its outgoing side), and `shown_dims` also asserts that every wire
picture IS the 2×2 canvas. Keep that pattern for any new case. Its rig
helpers (`b`, `job`, `frame`, `program`, `drain`, `shown_dims`, …) are
`pub(super)` and reused by the #215 siblings `program_bus_tests_transition.rs`
and `program_bus_tests_cue.rs`. A source whose own allocation must reach the
wire (`a_cut_to_an_idle_source_carries_its_standby_pair`) uses a canvas-sized
picture, as the paced idle black is FHD in production.

- **A held or slow `SP-program` NDI submit (#210):** `program_output_tests_order.rs`
  `HookedNdi` wraps `MockNdiBackend` and runs a hook inside every `send_audio`
  (the pair's first NDI call): a gate that holds the submit, or
  `SettableClock::advance` for a submit that costs N on a wall the test holds.
  Reuse it rather than adding a hold to sp-ndi's mock (a new `test_util`
  accessor needs its own sp-ndi test, `rust-workspace.md`).
- **Mock call-log gotcha (CI fail 26.9.):** a test that asserts the mock sender's LAST
  call (e.g. `send_video_flush`) must keep the owning output alive past the assertion —
  a thread closure that drops it appends `send_destroy` after the flush. Return the output
  from the thread (`let _out = thread.join().unwrap();`).
