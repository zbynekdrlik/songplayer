---
paths:
  - "crates/sp-server/src/playback/program_transition*.rs"
  - "crates/sp-server/src/playback/program_follow*.rs"
  - "crates/sp-server/src/playback/scene_off*.rs"
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/program_control.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "e2e/program-control.spec.ts"
  - "e2e/settings-program-transition.spec.ts"
---

# Scene transitions on `SP-program` (#215, B5 of EPIC #174)

The owner's rule: "žiadne trhané zvuky a obrazy sú neprípustné v produkcii".
A program cut is no longer a hard switch on one boundary. It is a crossfade of
audio AND picture that follows cg OBS's scene transition, and the outgoing
playlist keeps playing until the fade is over. Design record: #215 comment
5853036223 (Approach 1); the implementation notes are in comment 5853106216.

## A cut is a transition WINDOW (`program_bus.rs`)

- `ProgramCore::cut` still records the `(first_stamp, pid)` segment of #209:
  `to` owns every boundary from the cut boundary on. It ALSO pushes a
  `program_transition::Window` of the current spec: `from` (the outgoing
  source, `None` for a fade up from nothing), `to`, `start` = the cut
  boundary, `n_slots`, and an exclusive `end`.
- On each window boundary the outgoing source's pair goes to `from_pending`,
  the incoming one to `pending`. The boundary is released as ONE
  `ProgramJob::Mix` once each side is here or MISSED. "Missed" uses the #209
  per-source rules (`source_missed`): the source passed the boundary, it is
  absent for 1 s, or its 3-slot grace ran out (on the sender's wall only).
  A missed side is left `None` and mixed against the standby (studio black in
  the present side's EXACT layout, stride padding included —
  `black_nv12_into` — + silence, `side_fills`). With NEITHER side here, the
  boundary is filled like any other (`ProgramJob::Standby`). Either reorder
  buffer over 16 forces the boundary.
- **A Cut is a zero-length window**: no boundary is mixed, the output is the
  #209 cut byte for byte. `ProgramCore::new()` starts with a Cut
  (`SpecSource::Fallback`), so every #209 test still runs a Cut.
- A second cut: a window that has not started at the new boundary is replaced
  (a cut back to the source that still owns it cancels it); a running one is
  TRUNCATED at the new cut boundary, and the new window starts there. The new
  cut is also placed after the newest stamp of every window's `from`, so the
  outgoing source never has a waiting pair for a boundary that left its window
  (a stale `from_pending` entry would count toward the reorder bound forever).
- `prune` drops a window once its last boundary is served
  (`transitions_done`, a Cut included). `Window::covered()` is the number of
  boundaries a (possibly truncated) window covers; `served` and the dashboard
  `progress` count against it, so a superseded fade still ends at 100 %.
- Grid math is exact: `sp_core::genlock::{grid_index_100ns,
  grid_boundary_100ns}` (tested in sp-core `genlock_tests_grid_index.rs`).
  Slots are 333 333 or 333 334 × 100 ns wide, so NEVER `start + k · interval`.

## The mix (`program_transition.rs` pure, `program_output.rs` on the sender thread)

- Audio: equal power, `a = a_from·cos θ + a_to·sin θ`, θ = π/2 · (j + ½)/N over
  the WHOLE window (N = n · 1600 samples, j runs across boundaries), so the gain
  never steps at a boundary edge. A mono side feeds both channels; a missing
  side is silence. The audio stamp is `to`'s, else `from`'s.
- Picture: Q8 weight `w = round(256 · (k + ½)/n)`; same layout (width, height,
  stride, length) → `(f·(256 − w) + t·w + 128) >> 8` on Y and UV alike, into a
  `frame_pool` buffer. Different layouts → the picture CUTS at the window's
  midpoint (`w ≥ 128` → `to`) while the audio still crossfades, logged once per
  run (`ProgramOutput::size_cut`, reset by any non-mixed boundary).
- `mix_audio_block` collects its samples instead of pre-sizing the `Vec`: a
  capacity formula is an equivalent mutant.

## The outgoing playlist keeps playing (`scene_off.rs`)

`handle_scene_change(pid, false)` no longer pauses at once. It asks
`ProgramBus::hold_for(pid)` (only for a Playing pipeline):

- `Hold::Until(t)`: `pid` is the `from` of a window (or a Cut) not served yet.
  `t` = one slot after the window's end. It is re-checked at `t`.
- `Hold::OnProgram`: `pid` is still the program's source. The follow task and
  the #213 remote control cut only AFTER cg OBS switched, and cg OBS's scene
  event reaches the engine first. One `CUT_SETTLE` (500 ms) re-check.
- `None`: pause now, exactly as before. Every playlist that is not the
  program's source takes this path.

The re-check is `PipelineEvent::SceneOffDue` on the engine's own channel (a
spawned sleep). If the scene is back on program by then, it does nothing.
`scene_off_step` / `scene_off_recheck` take `now_100ns`, so the tests drive
them at chosen stamps; only the wrappers read `utc_now_100ns()`.

## Following cg OBS (`program_follow.rs`)

- Settings (`sp_core::config`, re-read every 5 s):
  - `program_follow_obs`: only `"true"` follows (default off);
  - `program_transition`: `obs` (default) / `fade` / `cut`;
  - `program_transition_ms`: a positive integer, default 300 (= 9 slots),
    rounded to whole slots, at least 1 slot, at most 300 slots (10 s). ONE
    parse for the server and the Nastavenia form:
    `sp_core::config::program_transition_ms` (+ `MAX_PROGRAM_TRANSITION_MS` =
    10 000, pinned equal to 300 slots by a test). The form shows the value the
    server uses, so its `min` / `max` never refuse a save of a stored
    out-of-range value.
- The spec every cut uses (`effective_spec`): the `fade` / `cut` override,
  else cg OBS's transition (`cut_transition` → Cut, any other kind → a Fade of
  its duration, a fixed-duration one → `program_transition_ms`), else a Fade
  of `program_transition_ms` (`fallback`). `ProgramBus::set_transition` returns
  whether it changed, and only a change is logged.
- cg OBS's transition comes from `GetCurrentSceneTransition` through
  `remote::Upstream` (the existing OBS client, never a second connection). It
  is read at start (BEFORE the first spec is applied), on `Connected`, on
  `CurrentSceneTransitionChanged` / `CurrentSceneTransitionDurationChanged`,
  and after a lagged broadcast. A read that got no answer (`read_pending`) is
  asked again on the settings polls — the OBS client broadcasts `Connected`
  before its connection loop serves `ObsCommand::Remote` (the NDI map rebuild
  runs first, up to ~10 s), so the `Connected` read can time out
  (`UPSTREAM_TIMEOUT` 3 s). **But only while cg OBS is up (`obs_up`, from
  `Connected` / `Disconnected`) and an OBS client exists
  (`Upstream::is_configured`).** A call made while cg OBS is away waits in the
  OBS client's command queue (served only while connected, 64 deep), and a full
  queue blocks its other senders (`title::push_title`'s blocking `send`) — the
  review round-2 finding. A retry that fails logs at debug; the first failure
  WARNs. The OBS identify subscribes Scenes (4) | Transitions (16) | Outputs
  (64) = 84; without Transitions cg OBS never sends those events.
- Follow = `ObsEvent::SceneChanged` (SongPlayer's own derived event, its
  playlists from `check_scene_items`) → `remote::map::scene_action` → cut via
  `persist_and_cut`, unless the program already shows that source.
- The follow CATCHES UP to cg OBS's current scene (`GetCurrentProgramScene` +
  `Upstream::scene_playlists` → `follow_scene`) at start, when
  `program_follow_obs` flips false → true, and after a lagged broadcast (a lost
  `SceneChanged`; the #170 poll only repairs an event cg OBS itself dropped).
  It never polls the scene otherwise. This replaces the event-night watcher
  `%TEMP%\sp_follow.ps1`, which polled the scene every 200 ms.
- **The task is `FollowLoop`** (`on_event`, `on_tick`, `resync`). Rules:
  - a catch-up always cuts with the spec APPLIED JUST BEFORE it: at start and
    after a lag `resync` reads the transition → `apply_spec` → catch-up; a poll
    loads the settings → retries a pending read → `apply_spec` → catch-up on
    the flip. (Catching up first cut with the previous spec — round 2.)
  - `resync` first DRAINS the events still queued: they are all older than its
    reads, so every one is dropped — a stale `SceneChanged` would cut back to a
    scene cg OBS already left — except that `Connected` / `Disconnected` still
    set `obs_up`. It then reads, applies, and catches up only while `obs_up`.

## API + UI

- `GET /api/v1/program` (and the cut answer) → `transition {kind, duration_ms,
  n_slots, source (obs|setting|fallback), active {from, to,
  start_boundary_100ns, n_slots, served_slots, progress} | null,
  transitions_done, mixed_boundaries, side_fills}` and `follow {enabled, mode,
  ms, obs_transition {name, kind, duration_ms} | null, last_follow_cut}`
  (`last_follow_cut` has the #213 `RemoteCut` shape).
- Dashboard `ProgramControl`: the `program-transition` line, e.g.
  `Prechod: prelínanie 300 ms (podľa OBS)` / `(nastavenie)` / `(predvolené)`,
  `Prechod: strih (…)`, plus `— prebieha N %` while a fade runs.
- Nastavenia fieldset `settings-program-transition`: `…-follow-obs`
  (checkbox), `…-kind` (select obs/fade/cut), `…-ms` (number).
- The mock derives `transition` / `follow` from the stored settings. It runs no
  cg OBS and no sender: `/__mock/program-obs-transition` injects cg OBS's
  transition and `/__mock/program-transition-active` a running window; both are
  cleared by `/__mock/program-reset`.

## Tests

- `program_transition_tests.rs` (pure, exact pins), `program_bus_tests_transition.rs`
  (the #209 rig: A 4×2, B 8×2, C 6×2, standby 2×2; `take_all` renders each job as
  `src W` / `fill` / `mix k/n F>T`), `program_output_tests.rs` (the mixed
  boundary on the mock sender), `scene_off_tests.rs`, `program_follow_tests.rs` +
  `program_follow_tests_task.rs` (the task and `FollowLoop`; the helpers are `pub(super)`)
  (a fake cg OBS at `ObsCommand::Remote`: scripted transition replies, a
  settable program scene for `GetCurrentProgramScene`, `playlists_of` for the
  scene lookups; every call is logged in order), `api/program_tests.rs`, and the mock
  E2Es `program-control.spec.ts` + `settings-program-transition.spec.ts`.
- Pins were derived with scratch Python models of `ProgramCore` and the
  weight / blend math (rust-workspace.md, no-compile box). Re-derive them with
  your own model when the Rust changes.
- A follow-task event is proven processed by a LATER event whose own effect is
  observable (a cut, a request to cg OBS), never by a sleep. The lagged-event
  test uses a broadcast of capacity 1 and three synchronous sends: the
  current-thread test runtime cannot run the task between them.
- **Send an event to the task only after it SUBSCRIBED** (wait for its first
  request to the fake cg OBS). The task subscribes when it is first polled, so
  an event sent right after `start()` reaches no receiver: `send` fails (the
  test's `expect` panics) and the task never sees it (round 2).
- To prove a state change ended a periodic action (e.g. no retry after
  `Disconnected`), sync on a LATER poll's observable effect (store a setting,
  `spec_becomes`), drain what was already running, then sync on one more poll
  and assert nothing followed. Events are handled before polls (`biased`).
- The engine tests never assert a "not yet" against the wall clock: the bus
  runs on fixed stamps, the steps are driven at chosen instants, and the real
  re-check timer is only bounded from below.

## Box acceptance (the supervisor's job, after the event)

Two playing playlists, a scene change via Companion/remote with a 300 ms OBS
fade: `mixed_boundaries` +9, audio RMS never more than 3 dB below the quieter
source, a dev1 VBAN capture with no zero-run ≥ 5 ms across the change, and the
owner confirms on the PA and the wall. `program_follow_obs` is the setting
switched on for events (design record); the watcher script
`%TEMP%\sp_follow.ps1` is retired once it is on.

**Also check the common production path: a PAUSED incoming playlist.** Every
off-program playlist is paused when its scene leaves program, and it resumes
on its own scene-go-on, at the same moment as the cut. Until its decode lands,
its paced pipeline offers its held last frame + silence (`paced_output.rs`
between scopes), so the first part of the fade can mix a frozen picture and
silence on the incoming side. Measure the resume latency against the window
(program `side_fills`, the VBAN capture); the design record's box case (two
PLAYING playlists) does not cover it (review round 1, #215).
