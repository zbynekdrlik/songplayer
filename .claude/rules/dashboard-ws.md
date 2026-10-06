---
paths:
  - "crates/sp-server/src/api/websocket.rs"
  - "crates/sp-server/src/playback/dashboard_replay*.rs"
  - "crates/sp-server/src/playback/tests_ws_replay.rs"
  - "crates/sp-server/src/db/models_playlists.rs"
  - "crates/sp-server/src/api/routes_mode.rs"
  - "crates/sp-server/src/api/routes.rs"
  - "crates/sp-server/src/playback/playlist_mode*.rs"
  - "crates/sp-server/src/playback/startup_pipelines.rs"
  - "crates/sp-server/src/playback/runtime_pipeline.rs"
  - "sp-ui/src/pages/dashboard.rs"
  - "crates/sp-server/src/playback/engine_play.rs"
  - "crates/sp-server/src/playback/position_update.rs"
  - "crates/sp-core/src/ws.rs"
  - "sp-ui/src/store.rs"
  - "sp-ui/src/ws.rs"
  - "e2e/player-known-state.spec.ts"
---

# The dashboard WebSocket: what a new client is told first (#225)

A dashboard (or any WS client) that connects mid-song gets, right after
`ObsStatus` + `ToolsStatus`, the **on-connect replay**
(`api/websocket.rs::on_connect_replay`). It is the engine's last dashboard
message per playlist, so the first batch already carries the truth. Design
record: #225 comment 5917562242 (Approach 1), adapted in the
`Anchors-confirmed` comment 5975004341.

## One send point, one record

- Every `PlaybackStateChanged` and `NowPlaying` the engine sends goes through
  `PlaybackEngine::send_dashboard` (`playback/dashboard_replay.rs`): it records
  the message in the process-global `DashboardReplay`
  (`dashboard_replay::global()`, like `now_playing::global()`, #177; no engine
  field, `playback/mod.rs` is near the cap) and THEN broadcasts it. The send
  sites are `engine_play.rs::broadcast_state` (the one `PlaybackStateChanged`
  sender; its callers include `playlist_mode.rs::apply_mode` for a `SetMode`
  and `runtime_pipeline.rs::remove_pipeline`), `mod.rs::broadcast_now_playing_on_start`
  and `position_update.rs::maybe_broadcast_position_update`. A playlist with
  NO pipeline has its state told by the sibling `engine_play.rs::broadcast_idle`
  (`Idle` in its new mode, #225 unit 2: `broadcast_state` needs a pipeline;
  `apply_mode` calls it). A new sender of either message calls
  `send_dashboard` (a state: `broadcast_state` / `broadcast_idle`), never
  `ws_event_tx.send` directly, or a new client is not told it.
- `runtime_pipeline.rs::remove_pipeline` sets the pipeline `Idle` and tells
  the open dashboards through `broadcast_state`, then forgets the playlist
  (`forget`), so an open dashboard and a reload agree (review rounds 2-3).
  It forgets with or without a pipeline (unit 2): a pipeline-less playlist
  has a record once its mode changed, and a deleted one must not be
  replayed.
- A client that LAGS (the broadcast channel dropped messages for it)
  resubscribes (`event_rx.resubscribe()`) and is sent the replay again
  (`websocket.rs::send_replay`), so a dropped state change does not leave it
  stale until the playlist's next change. Resubscribe FIRST (review round 4):
  the buffered tail is older than the replay and would roll it back.
- **No gap between the replay and the live stream:** `handle_ws` subscribes to
  the event bus BEFORE it builds the replay, and the engine records BEFORE it
  broadcasts. A message is in the replay, arrives live, or both. A live message
  queued between the subscribe and the read is delivered AFTER the replay even
  when it is older than the replayed value, so a new client can briefly see the
  older value (a position a tick back, the previous state) until the next live
  message, a few ms later.

## The replay

`DashboardReplay::replay(&[(id, row mode)])`, EVERY row of the `playlists`
table with its mode (`db/models_playlists.rs::all_playlist_modes`, ascending,
inactive ones too: the UI lists them all; a failed read replays only the
recorded playlists):

- a playlist whose last recorded state is not `Idle`: its last `NowPlaying`
  (when it had one) FIRST, then its state;
- any other playlist: an explicit `Idle` (transport `Idle`), no `NowPlaying`;
- a recorded playlist the list does not name (the DB read failed) follows, by
  id;
- the mode is the one the engine plays: the recorded one, else the ROW's
  (`row_mode`, the mode its pipeline starts in; the default for an unlisted
  one, which has no row to read).

Why each rule:

- **Every playlist gets a state** so the Player can tell "not known yet" from
  "nothing plays" without a new message type (the UI's `state_known`,
  `sp-ui-frontend.md` "The Player never claims a state it was not told").
  Before #225 Idle playlists were skipped as "noise".
- **The song before the state**: each WS message is its own browser task, so
  a state that lands first renders a "known, no song" Player for a moment.
- **The mode is the one that plays** (review round 1, then unit 2). Round 1
  found the engine ignored the DB (idle ytlive, seeded `single`, read "Jedna
  skladba" while the engine played Continuous) and told the engine's mode.
  Unit 2 made the row the one truth (next section), so the recorded mode and
  the row agree and an unrecorded playlist replays its row's. A mode change
  is told at once (`apply_mode`), so the record is never stale.
- **Never the NDI-health sample again.** The old builder
  (`playback_state_replay`, deleted) read `ndi_health_registry.snapshots()`,
  refreshed on the 5 s health tick. Once the Player's badge reads the WS state,
  a reload right after a cut would show the old program state, in the label
  AND the badge, until the playlist's next state change. The recorded message
  is exactly what a live client last got, including the raw transport (#201).

## A playlist's mode: ONE persisted truth, its row (#225 unit 2)

Main's ROZHODNUTÉ: #225 comment 5975832356; how it is built: the
`Anchors-confirmed` comment 5976091780.

- **A pipeline starts in its row's `playback_mode`.**
  `db/models_playlists.rs::row_mode(id, name, stored)` parses it with
  `PlaybackMode::parse` (sp-core, the one parser; `from_str_lossy` delegates
  to it); an unknown value plays the default with a WARN naming the
  playlist. Both production creators pass it into `ensure_pipeline_inner`:
  `startup_pipelines.rs` (each `Playlist` of `get_active_playlists`) and
  `ensure_pipeline_for_playlist` (from the SELECT that decides the creation,
  so no second read). An existing pipeline keeps its mode. The sync
  `ensure_pipeline` (default mode) is `#[cfg(test)]`: no production path
  creates a pipeline without its row.
- **The row first, then the engine** (`api/routes_mode.rs`):
  - `PUT /api/v1/playback/{id}/mode` (the Player's mode select) and the WS
    `ClientMsg::SetMode` go through `persist_then_tell`: the UPDATE first;
    0 rows → 404; a failed write → an ERROR log + 500 (the Player's "Zmena
    režimu zlyhala" toast), and the engine is NEVER told, so it keeps the
    mode the row still holds. Only a written row sends
    `EngineCommand::SetMode`. The WS path tells either failure (an unknown
    playlist or a failed write) as a Slovak `ServerMsg::Error` broadcast to
    every open dashboard's error banner. An engine that is gone when told
    (shutdown) is an ERROR log; the row holds the mode for the next start.
  - The playlist update (`PUT /api/v1/playlists/{id}`,
    `routes::update_playlist`; there is NO PATCH route — axum answers 405)
    writes the row with its other fields, then calls `tell_engine` (before
    `EnsurePipeline` / `RemovePipeline`). A failed write logs ERROR too.
  - `requested_mode` is the one check of a requested mode — the mode
    route's body, the playlist update's and the playlist POST's field: an
    unknown one is 400 (a WARN naming the playlist, with at most 32 of its
    characters, escaped), a known one is stored by its canonical name
    (`as_str`; the POST's default when none is given), so every row the API
    writes parses.
  - `MODE_ORDER` (a `tokio::sync::Mutex`, like `cache::SONG_FILES`) is held
    across "write the row, tell the engine", across the playlist update and
    across the playlist DELETE, so the engine receives concurrent changes in
    the order the row took them (its command channel is FIFO), and no mode
    reaches it after it forgot a deleted playlist (the record would be
    replayed).
- **An unknown stored value** is the default everywhere; `row_mode` WARNs
  at a pipeline's start, `all_playlist_modes` (every dashboard connect)
  maps it quietly.
- **Every change is told.** `handle_command(SetMode)` →
  `playlist_mode.rs::apply_mode`: the pipeline's mode + `broadcast_state`;
  with no pipeline, `broadcast_idle` (`Idle` in that mode). Then it
  returns: `SetMode` is a no-op transition, and `apply_event` would WARN
  "no pipeline" for an inactive playlist.
- **Startup:** `startup_pipelines::startup_mode(playlists, id)` (pure,
  unit-tested) picks each pipeline's row mode for `create_startup_pipelines`.
- **On the box:** a deploy of this changes what plays only where a row is
  not `continuous` (main's check 4.10.2026: all 10 rows `continuous`). A
  fresh DB seeds ytlive `single` (`startup.rs::ensure_live_playlist_exists`),
  which it now plays. The integration check (main session, at the
  deploy; recorded on the ticket's LANE-RETURN): the replayed mode of
  every playlist equals its `GET /api/v1/playlists` `playback_mode`.
- **Never** send `EngineCommand::SetMode` / `PlayEvent::SetMode` for a mode
  the row does not hold (tests drive the engine directly; production never
  does).

## The client forgets on every socket (sp-ui)

`sp-ui/src/ws.rs` calls `store.forget_now_playing()` when a socket OPENS and
when it CLOSES (review round 2). The client reconnects without a page reload,
and after a server restart (every deploy) the new replay tells a playlist
`WaitingForScene` / `Idle` with no NowPlaying. Kept store entries would show
the old socket's song as if still loaded (a phantom title, a mounted mixer, an
enabled seek). Until the new replay lands the Player reads "Načítavam…". The
store also drops an entry's song on a live `Idle` (a fresh entry).

Two consequences, both review round 3:

- **The replay lands message by message**, a playing playlist's song a
  message BEFORE its state. So anything that decides from the store must not
  decide on a half-told one:
  - the Dashboard's auto-select (`pages/dashboard.rs`) keeps a valid
    selection until EVERY listed playlist's state is known
    (`selection::states_known`, the one predicate for such a reader, review
    round 4), else a song-but-no-state entry reads "nothing plays" and the
    work area flips to the first playlist by name;
  - the Player's preview Effect turns the preview off only once the state is
    known (`state_known && !is_decoding`). The `<video>` is unmounted while
    the state is unknown (its socket closes), but `preview_on` survives, so
    the preview is re-created by itself after the replay — no second click;
  - the Player's toggle and mode select claim nothing until the state is
    known (review round 4): a disabled "⏯" and a disabled "—" (value `""`,
    `sp_core::player_view::{play_pause, mode_value}`).
- **The mock must keep the server's order.** `handle_ws` writes the replay
  before it forwards anything from the bus, so a live message never
  overtakes it. The mock's `sendLive(ws, msg)` queues a live message for a
  socket that has not had its replay yet and flushes it right after the
  replay. Without that, a spec's `/__mock/now-playing` posted right after
  `page.goto` reached the client before the replay, whose `Idle` then wiped
  its song (the dub mixer specs).

## Tests

- `playback/tests_ws_replay.rs` (the #225 RED): an engine on a `test_state()`
  DB plays a song in Loop (`SetMode` driven straight into the engine, state,
  `Started`, a position tick past the throttle); `on_connect_replay` must
  return its NowPlaying, then its state in Loop (the recorded one; its row
  says `single`), then the idle playlist's `Idle` in its row's `loop` (its
  pipeline created from the row, unit 2). A second test: `SetMode` on an
  idle playlist is broadcast at once and replayed to the next client. They
  read the GLOBAL registry, so they use playlist ids no other test uses
  (22 501-22 504) and filter the replay to them (other tests record into the
  same global).
- `db/models_playlists.rs`: `all_playlist_modes` returns every id ascending
  with its row mode, inactive ones too; `row_mode` maps the stored names and
  an unknown one to the default.
  `tests_ws_replay.rs::a_failed_playlist_read_still_replays_what_the_engine_told`
  closes the pool and checks the fallback (websocket.rs is mutation-excluded).
- `playback/playlist_mode_tests.rs` (unit 2; ids 22 530-22 549 are this
  unit's, the WS test below takes 22 538 / 22 539): the real
  router + the engine fed the API's commands through
  `engine_dispatch::dispatch`: a row's `single` starts the pipeline in
  Single; the PUT writes the row, reaches the running engine and is told,
  and a fresh engine built as `lib.rs` builds it (`get_active_playlists` →
  `create_startup_pipelines`) starts in it; a playlist
  update reaches the engine and the replay; an unknown row value plays
  Continuous; a pipeline-less playlist is told its new mode and forgotten on
  DELETE; an unknown mode is 400 for the mode route and the playlist update;
  a `RAISE(ABORT)` trigger (the row refuses the write) → 500, the engine
  untold; an unknown playlist → 404. Review round 1: a POST takes a known
  mode only, by its canonical name, and so do a mode route `Single` / a
  playlist update `LOOP`; `MODE_ORDER` is pinned in the safe direction: two
  tests hold it (the mode route and the playlist update, then a DELETE,
  must not finish within 200 ms, then finish once it is released), and
  review round 2's `the_order_is_held_until_the_engine_is_told` fills the
  engine's channel so each of the three blocks in its tell after its
  write: the order must still be held (`try_lock` fails). Every playlist
  update in these tests is a `PUT /api/v1/playlists/{id}` (review round 3:
  five tests had sent PATCH, a 405, and never reached the handler).
  `api/websocket.rs::a_ws_set_mode_saves_the_row_then_tells_the_engine`
  covers the WS path (its error text Slovak), and
  `startup_pipelines.rs::a_startup_output_starts_in_its_own_rows_mode` the
  startup choice.
- `e2e/player-known-state.spec.ts` (review round 2): `/__mock/ws-replay
  {no_song: [{playlist_id, state, transport}]}` + `/__mock/ws-drop` (closes
  every dashboard socket): after the reconnect a playlist waiting with no song
  reads "Nič nehrá", never the old socket's song; a live `Idle` clears the
  song it had. Review round 3: a running live preview comes back by itself
  after a reconnect; an unpinned Dashboard keeps the playing playlist
  selected through it (a MutationObserver log of `workspace-title`).
- `playback/dashboard_replay_tests.rs`: the rules on a private
  `DashboardReplay::default()` (one test per rule, every listed mutant mapped),
  plus the engine glue on the global (ids 22 511 / 22 512): `send_dashboard`
  records AND still broadcasts, `remove_pipeline` tells Idle and forgets.
- The mock (`e2e/mock-api.mjs` `sendReplay`) models the same replay: every
  active playlist + the Dabing playlist (500), playlist 1 playing "Never Gonna
  Give You Up" on program, a running tick's items on top, every mode
  `Continuous` (every mock row is `continuous`). A spec that needs a late
  replay or a late first song uses `POST /__mock/ws-replay {delay_ms,
  now_playing_delay_ms}` and resets it with `{}` (it also holds the mock's 2 s
  playlist-1 song interval). Every live message goes through `sendLive`
  (above).
- The mock's limits: playlist 1 is always replayed playing, whatever
  `/__mock/set-playing` or the program last said, and an off-air ▶
  (`playingOffAir`) is not replayed. A spec that needs another state after a
  reconnect sets it with `no_song`. Modelling the server's last-broadcast
  record in the mock would leak state across specs (one mock process serves
  the whole serial run).
