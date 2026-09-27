---
paths:
  - "crates/sp-server/src/playback/program_transition*.rs"
  - "crates/sp-server/src/playback/program_follow*.rs"
  - "crates/sp-server/src/playback/scene_off*.rs"
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/playback/pacer_tests_live.rs"
  - "crates/sp-server/src/playback/nv12_fit.rs"
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
The addendum (design record 5855841198, after box run 1 in 5855833871): the
picture dissolves across different sizes, and a fade waits for the incoming
song's first real pair.

## A cut is a transition WINDOW (`program_bus.rs`)

- `ProgramCore::cut` still records the `(first_stamp, pid)` segment of #209:
  `to` owns every boundary from the cut boundary on. It ALSO pushes a
  `program_transition::Window::cued` of the current spec: `from` (the source
  ON AIR — see the cue gate below; `None` for a fade up from nothing), `to`,
  `cut_100ns` = the cut boundary, `start_100ns` = the first MIXED boundary,
  `n_slots`, an exclusive `end`, and its `cue`. The window takes `from`'s
  pairs over `[cut, end)` (`Window::covers`); only `[start, end)` is mixed
  (`Window::slot`).
- The window half of the core is the child module `program_bus_window.rs`
  (`window_at`, `on_air`, `window_step`, `open_cue`, `commit_held`,
  `commit_mix`; 1000-line cap).
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
  A window whose cue still WAITS is truncated too and FROZEN (`Cue::Frozen`:
  it never opens, its boundaries stay held), and the new window fades out of
  the source really on air, the frozen window's `from` (`ProgramCore::on_air`).
  A cut back to that source needs no window: it just takes the boundaries over
  (segment only).
- `prune` drops a window once its last boundary is served
  (`transitions_done`, a Cut included; a frozen window too, a same-slot
  replaced one never). `Window::covered()` is the number of MIXED boundaries:
  the open window's (possibly truncated) span, `n_slots` while its cue waits,
  0 once frozen; `served` and the dashboard `progress` count against it, so a
  superseded fade still ends at 100 %.
- Grid math is exact: `sp_core::genlock::{grid_index_100ns,
  grid_boundary_100ns}` (tested in sp-core `genlock_tests_grid_index.rs`).
  Slots are 333 333 or 333 334 × 100 ns wide, so NEVER `start + k · interval`.

## The cue gate (#215 addendum B2, `program_bus_window.rs`)

The box run 1 finding: a playlist whose scene comes on program starts a NEW
song. Its first decoded pair arrives ~10 boundaries after the cut (the paused
song's teardown = ~4 paced-output fills, then the new song's pre-roll = ~6
standby pairs while the decoder opens, see "B1" below), so a fade laid on the
cut boundary mixed the outgoing song against silence.

- `SubmitJob::live` marks a pair of the source's OWN content: the pacer's
  `PacedSink::emit` (a decoded frame with its song audio, a stall repeat
  included) and an NDI input capture. Never live: `PacedSink::emit_standby`
  (the paused frozen picture, and via `default_submit_shared` the idle and
  pre-roll black, a starve fill, a held seek frame), the paced consumer's
  fill (`PacedConsumer::fill_job`) and the NDI input's standby pair. The
  default `emit_standby` is `emit`, so `FrameSubmitter` and emit-only sinks
  see what they always saw; `HandoffSink` overrides it (live = false).
- A Fade window starts `Cue::Waiting { deadline }` (`Window::cued`), with
  `deadline` = the cut + `CUE_WAIT_MAX_SLOTS` (15 = the design's
  `CUE_WAIT_MAX` 500 ms) and its `end` the latest it can reach (the cut +
  15 + `n_slots`). A Cut is `Cue::Open` at once and zero-length: it never
  waits.
- While it waits, a boundary is decided once the INCOMING pair is here or
  missed (its first live pair may be the one):
  - `to`'s pair is live → the cue OPENS there (`Window::open`: `start` = that
    boundary, `end` = start + `n_slots`), and the boundary is mixed as slot 0;
  - `to` decided and the boundary is the deadline → the cue opens anyway
    (`cue_timeouts` +1, WARN `the incoming source sent no live pair in time`);
  - otherwise the boundary is HELD (`commit_held`): `from`'s own pair at full
    level as a `ProgramJob::Source` (counted `forwarded`), or the program's
    standby pair when `from` missed (`filled`); `to`'s pair is dropped;
  - both missed and neither here: filled like any boundary (the core's fill +
    resync path).
- `open_cue` drops `from`'s pairs past the window's new end (they belonged to
  the wait's worst case and would count toward the reorder bound forever).
- `transition.cue_wait_boundaries` = the LAST opened window's wait (0 = live
  on the cut boundary, 15 = timed out; more only if a > 8-slot resync jumped
  past the deadline); `cue_timeouts` counts the timeouts. A frozen window
  never opens and never touches them. NOT every timeout is a fault: a
  dashboard cut to a playlist whose scene is not on program (it stays paused,
  no new song) or to the NDI input with no source offers only standby pairs,
  so it waits the full 15 boundaries, WARNs and counts one. Read the box's
  `cue_timeouts` +0 check on the cg OBS scene-change path only.
- A WAITING cue opens only on its own window's live pair: once a later cut
  froze it, a live pair of its incoming source inside it is held like any
  other (review round 1). `cut` reads `on_air(boundary)` BEFORE
  `windows.retain`, so a same-slot re-cut that drops the waiting window still
  fades out of that window's outgoing source. `on_air` takes the first
  WAITING or FROZEN window whose span reaches the new boundary — cut on or
  before it, end on or after it — else the selected source:
  - a frozen window's end may lie on (a cut back, then a same-slot re-cut
    that drops the cut back's segment, review round 3) or AFTER the new
    boundary: cut boundaries are NOT monotone. A cut is placed after the
    newest stamp of every source still involved, and a source that drops out
    (its window served) no longer pushes it later, so a later cut can land a
    slot or more BEFORE an earlier one (review round 4, X running two slots
    ahead: cut back on b(18), the next cut on b(17));
  - a window cut AFTER the new boundary is one this cut replaces: its
    outgoing source held nothing on program yet, so it never decides (X four
    slots ahead: a Cut to B on b(17), a waiting fade B → C on b(18), then a
    cut to D on b(17) fades out of A, not B — review round 4).
- Logs: INFO `the incoming source is live — the fade starts` (from, to,
  waited) / WARN `… sent no live pair in time …` per opened cue, INFO `a later
  cut froze the fade still waiting for its cue`, and from the sender one INFO
  `the fade's mixed boundaries went out` per run of mixed boundaries
  (`boundaries`, `fitted`, `max_picture_us` = the worst fit + blend time on
  the `SP-program` thread).
  The dashboard "Prechod" line shows no counters, so it is unchanged; a
  waiting window shows as `active` with 0 %.
- `hold_for` reports a waiting window's LATEST end, so `scene_off.rs` keeps
  the outgoing playlist playing through the wait with no new timer; its
  re-check finds the window over, or asks again.

### B1: the silence before a new song is its PRE-ROLL, not a defect

Traced on #215 (comment 5856377673): after `paced output: a pacer feeds
again`, `Pacer::preroll` (`pacer_preroll.rs`) services every boundary with the
standby pair (the 1920×1080 black + silence) until `PrerollGate::poll` sees the
decoder opened (MF reader, the stems — `karaoke: mixing stems` ~93 ms after
`starting playback` — and `open_paced_decoder`) AND its first frame buffered.
The song's first decoded pair then carries FLAC block 0 (`split_sync` pairs
the first frame with every chunk up to pts + 250 ms; the A/V anchor starts the
block at the frame's media time). `pacer_tests_live.rs` pins both halves. What
would shorten the wait (main-session decisions, not built): overlap the paused
song's producer teardown (`stop` + `join`, ~4 fills) with the new song's open,
or resume the paused song on scene-on instead of `SelectAndPlay`.

## The mix (`program_transition.rs` pure, `program_output.rs` on the sender thread)

- Audio: equal power, `a = a_from·cos θ + a_to·sin θ`, θ = π/2 · (j + ½)/N over
  the WHOLE window (N = n · 1600 samples, j runs across boundaries), so the gain
  never steps at a boundary edge. A mono side feeds both channels; a missing
  side is silence. The audio stamp is `to`'s, else `from`'s.
- Picture: Q8 weight `w = round(256 · (k + ½)/n)`, blended
  `(f·(256 − w) + t·w + 128) >> 8` on Y and UV alike into a `frame_pool`
  buffer, always in the INCOMING side's layout. When the two layouts (width,
  height, stride, length) differ, the outgoing picture is first fitted into
  the incoming one (#215 addendum A) — there is no midpoint cut any more:
  - `FitPlan` (`fit_nv12_into` is its one-shot form) places the picture with
    `nv12_fit::aspect_fit` — the SAME placement the #178 preview letterbox
    (`preview_stream::placement_for`) uses: each axis the destination capped by
    the aspect-scaled other axis, floored to even, centred on even offsets (a
    chroma sample covers its 2×2 luma block; 2560×1080 into 1920×1080 → rows
    134..943), studio-black bars Y 16 / UV 128. Only the pixel paths differ:
    the preview copies nearest-neighbour on the decode thread (cheap by rule),
    the program fit is bilinear and runs only on mixed boundaries;
  - bilinear in Q8 at the pixel CENTRES (`HALF_PIXEL_Q8`), luma and the
    half-resolution chroma each on their own grid, clamped at the edges;
  - the column taps are built once per pair of layouts: `ProgramOutput` keeps
    the plan (`fit`, `fit_plans` counts builds) while the pair stays the same,
    fits into a reused scratch `Vec` (`fitted`), and logs one DEBUG line per
    plan naming both layouts;
  - a source or destination that is not whole NV12 for its layout gives the
    black canvas alone, never a panic (a zero-size one simply draws nothing);
  - the cost per mixed boundary is the black canvas, the fit and the blend,
    each a full pass over the destination; if the box's `max_picture_us`
    reads high, fuse the fit into the blend and paint only the bars.
  The cost is one bilinear fit per mixed boundary on the program thread (≤ 9
  for 300 ms).
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
  (`UPSTREAM_TIMEOUT` 3 s). **But only while cg OBS is up (`obs_up`).** A
  call made while cg OBS is away waits in the OBS client's command queue
  (served only while connected, 64 deep), and a full queue blocks its other
  senders (`title::push_title`'s blocking `send`) — the review round-2
  finding. A retry that fails logs at debug; the first failure WARNs. The OBS
  identify subscribes Scenes (4) | Transitions (16) | Outputs (64) = 84;
  without Transitions cg OBS never sends those events.
- `obs_up` starts as `Upstream::is_configured()`: without an OBS client cg OBS
  is never up and nothing is read, retried or caught up (a call there returns
  `None` at once, so this saves only log noise). With a client it starts up,
  so the start's reads count. `Disconnected` sets it down, and EVERY other
  event sets it up: only a live connection sends them, so a `Connected` lost
  in a lagged broadcast cannot leave it down (review round 3).
- Follow = `ObsEvent::SceneChanged` (SongPlayer's own derived event, its
  playlists from `check_scene_items`) → `remote::map::scene_action` → cut via
  `persist_and_cut`, unless the program already shows that source.
- The follow CATCHES UP to cg OBS's current scene (`GetCurrentProgramScene` +
  `Upstream::scene_playlists` → `follow_scene`, in `Follow::follow_current_scene`,
  which is UNGATED: only `FollowLoop::catch_up` calls it) at start, when
  `program_follow_obs` flips false → true, and after a lagged broadcast (a lost
  `SceneChanged`; the #170 poll only repairs an event cg OBS itself dropped).
  It never polls the scene otherwise. This replaces the event-night watcher
  `%TEMP%\sp_follow.ps1`, which polled the scene every 200 ms.
- **The task is `FollowLoop`** (`on_event`, `on_tick`, `resync`,
  `catch_up`, `drain`). Rules:
  - a catch-up always cuts with the spec APPLIED JUST BEFORE it: at start and
    after a lag `resync` reads the transition → `apply_spec` → catch-up; a poll
    loads the settings → retries a pending read → `apply_spec` → catch-up on
    the flip. (Catching up first cut with the previous spec — round 2.)
  - `drain` drops every event still queued (they are older than the read that
    follows; a stale `SceneChanged` would cut back to a scene cg OBS already
    left), keeps `obs_up` and the newest dropped `SceneChanged`
    (`missed_scene`, forgotten again on a `Connected` — the new connection
    re-reports its scene — or a `Lagged` — a newer one may be lost), and
    returns whether cg OBS's transition must be read again
    (`rereads_transition`: `Connected` or a transition event, or a `Lagged`
    inside the drain). `resync` drains before its transition read. `catch_up`
    ALWAYS drains (following or not) what queued since — during resync's read,
    or, for the switch-on catch-up, since the task last handled an event;
    while a drain asks for it, it re-reads + applies the transition and drains
    again, at most `MAX_CATCH_UP_REREADS` (3) times — still changing after
    that, it sets `read_pending` for the polls. So a drain ALWAYS comes right
    before the scene read (review rounds 3 + 4).
  - `Follow::follow_current_scene(upstream, dropped)` follows `dropped` (the
    taken `missed_scene`) instead of cg OBS's answer only when cg OBS names NO
    scene, or names that SAME scene without its playlists: the dropped event
    may be the only news of the change, because the #170 poll does not repeat
    a scene the OBS client already stored. A dropped change of another scene
    is older than the named one and is never followed (review rounds 4 + 5).
    Known gap: an unanswered catch-up with nothing dropped is not retried —
    the program waits for cg OBS's next scene change or reconnect. Reading
    the OBS client's own `ObsState` (`connected`, `current_scene`,
    `active_playlist_ids`) instead of asking cg OBS would remove it; that
    needs `ObsState` wired to the engine through `lib.rs`, which #215's design
    keeps untouched, so it is the main session's call (review round 5).
  - the catch-up runs only while `obs_up` and following. Switched on while cg
    OBS is away, the follow catches up on the reconnect instead: the OBS
    client's connection step 6 reports cg OBS's program scene as a
    `SceneChanged`.

## API + UI

- `GET /api/v1/program` (and the cut answer) → `transition {kind, duration_ms,
  n_slots, source (obs|setting|fallback), active {from, to,
  start_boundary_100ns, n_slots, served_slots, progress} | null,
  transitions_done, mixed_boundaries, side_fills, cue_wait_boundaries,
  cue_timeouts}` and `follow {enabled, mode,
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

- `program_transition_tests.rs` (pure, exact pins; the fit's from a scratch
  Python model of `FitPlan`), `program_bus_tests_transition.rs`
  (the #209 rig: A 4×2, B 8×2, C 6×2, standby 2×2; `take_all` renders each job as
  `src W` / `fill` / `mix k/n F>T`; its helpers are `pub(super)`),
  `program_bus_tests_cue.rs` (the cue gate: `standby()` is a non-live pair;
  it reads `core.pending` / `core.from_pending` directly),
  `pacer_tests_live.rs` (which pacer pairs are `emit` vs `emit_standby`, and
  the B1 pin), `program_output_tests.rs` (the mixed
  boundary on the mock sender, the fitted blend, the plan reuse),
  `scene_off_tests.rs`, `program_follow_tests.rs`
  (pure + `Follow`), `program_follow_tests_task.rs` (the task end to end) and
  `program_follow_tests_loop.rs` (`FollowLoop`'s steps awaited one by one; the
  helpers are `pub(super)`) — a fake cg OBS at `ObsCommand::Remote`: scripted
  transition replies, a settable program scene for `GetCurrentProgramScene`
  (`scene_unanswered` answers it with a failure), `playlists_of` for the scene
  lookups (`lookup_unanswered` drops the reply); every call is logged in
  order; `during_read` = one batch of events
  per transition read, broadcast while the fake answers it, i.e. what reaches
  the queue while the task waits — `api/program_tests.rs`, and the mock
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
- **Send events only after the start's catch-up has drained the queue** —
  it drains following or not, so an event sent after only the first request
  can be dropped by that drain (that it is not is then luck of the
  current-thread scheduler, not the test's sync). With the follow ON wait for
  the whole catch-up (`REQUEST`, `PROGRAM_SCENE`, `ScenePlaylists:<scene>`);
  with it OFF wait for the spec the start applied (`spec_becomes`): the drain
  follows `apply_spec` with no `.await` between (rounds 4 + 5).
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
owner confirms on the PA and the wall. Addendum (run 2): no `differ in size`
WARN (only the DEBUG fit line), the incoming song's FLAC block 0 inside the
window, no true-zero run between the outgoing fade and the incoming song, and
`cue_wait_boundaries` ≤ 15 with `cue_timeouts` +0 — about 10–11 for a paused
playlist that starts a new song (the fills + the pre-roll, B1 above), 0 for an
already playing one. `program_follow_obs` is the setting
switched on for events (design record); the watcher script
`%TEMP%\sp_follow.ps1` is retired once it is on.

**Also check the common production path: a PAUSED incoming playlist.** Every
off-program playlist is paused when its scene leaves program; on its
scene-go-on the engine starts a NEW song for it, at the same moment as the cut.
Until that song's first decoded pair, its paced output offers fills, then
pre-roll standby pairs (black + silence) — none of them live, so the cue gate
HOLDS those boundaries (the outgoing song on program at full level) and the
fade starts on the first live pair. The wait shows up as
`cue_wait_boundaries` (~10–11, see B1) and must stay under the 15-boundary
bound (`cue_timeouts` +0). During a 2560×1440 ↔ 1920×1080 fade also read the
sender's `max_picture_us` line (well under one 33 ms slot) and
`health.coalesced` +0.
