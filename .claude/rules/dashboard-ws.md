---
paths:
  - "crates/sp-server/src/api/websocket.rs"
  - "crates/sp-server/src/playback/dashboard_replay*.rs"
  - "crates/sp-server/src/playback/tests_ws_replay.rs"
  - "crates/sp-server/src/db/models_playlists.rs"
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
  sender), `mod.rs::broadcast_now_playing_on_start` and
  `position_update.rs::maybe_broadcast_position_update`. A new sender of either
  message calls `send_dashboard`, never `ws_event_tx.send` directly, or a new
  client is not told it.
- `runtime_pipeline.rs::remove_pipeline` tells the open dashboards the
  playlist is `Idle` (through `send_dashboard`), then forgets it (`forget`),
  so an open dashboard and a reload agree (review round 2).
- A client that LAGS (the broadcast channel dropped messages for it) is sent
  the replay again (`websocket.rs::send_replay`), so a dropped state change
  does not leave it stale until the playlist's next change.
- **No gap between the replay and the live stream:** `handle_ws` subscribes to
  the event bus BEFORE it builds the replay, and the engine records BEFORE it
  broadcasts. A message is in the replay, arrives live, or both. A live message
  queued between the subscribe and the read is delivered AFTER the replay even
  when it is older than the replayed value, so a new client can briefly see the
  older value (a position a tick back, the previous state) until the next live
  message, a few ms later.

## The replay

`DashboardReplay::replay(&playlist_ids)`, the ids of EVERY row of the
`playlists` table (`db/models_playlists.rs::all_playlist_ids`, ascending,
inactive ones too: the UI lists them all; a failed read replays only the
recorded playlists):

- a playlist whose last recorded state is not `Idle`: its last `NowPlaying`
  (when it had one) FIRST, then its state;
- any other playlist: an explicit `Idle` (transport `Idle`), no `NowPlaying`;
- a recorded playlist the list does not name (the DB read failed) follows, by
  id;
- the mode is the ENGINE's: the recorded one, else `PlaybackMode::default()`.

Why each rule:

- **Every playlist gets a state** so the Player can tell "not known yet" from
  "nothing plays" without a new message type (the UI's `state_known`,
  `sp-ui-frontend.md` "The Player never claims a state it was not told").
  Before #225 Idle playlists were skipped as "noise".
- **The song before the state**: each WS message is its own browser task, so
  a state that lands first renders a "known, no song" Player for a moment.
- **The mode is the engine's, never the DB's** (review round 1): the engine
  never reads `playlists.playback_mode` (every pipeline starts at
  `PlaybackMode::default()`, `runtime_pipeline.rs`) and `SetMode` is not
  written back, so the DB value is not what plays (idle ytlive, seeded
  `single`, read "Jedna skladba" while the engine played Continuous). A mode
  change broadcasts the playlist's state (`handle_command` → `broadcast_state`),
  so the open dashboards learn it at once and the record is never stale.
  The engine ignoring the DB mode is a separate, older gap.
- **Never the NDI-health sample again.** The old builder
  (`playback_state_replay`, deleted) read `ndi_health_registry.snapshots()`,
  refreshed on the 5 s health tick. Once the Player's badge reads the WS state,
  a reload right after a cut would show the old program state, in the label
  AND the badge, until the playlist's next state change. The recorded message
  is exactly what a live client last got, including the raw transport (#201).

## The client forgets on every socket (sp-ui)

`sp-ui/src/ws.rs` calls `store.forget_now_playing()` when a socket OPENS and
when it CLOSES (review round 2). The client reconnects without a page reload,
and after a server restart (every deploy) the new replay tells a playlist
`WaitingForScene` / `Idle` with no NowPlaying. Kept store entries would show
the old socket's song as if still loaded (a phantom title, a mounted mixer, an
enabled seek). Until the new replay lands the Player reads "Načítavam…". The
store also drops an entry's song on a live `Idle` (a fresh entry), and the
dashboard's auto-select keeps its selection while nothing is known
(`pages/dashboard.rs`), so a reconnect does not flip the work area to the
first playlist by name.

## Tests

- `playback/tests_ws_replay.rs` (the #225 RED): an engine on a `test_state()`
  DB plays a song in Loop (`SetMode`, state, `Started`, a position tick past
  the throttle); `on_connect_replay` must return its NowPlaying, then its
  state in Loop (the DB says `single`), then the idle playlist's `Idle` in
  Continuous (the DB says `loop`). A second test: `SetMode` on an idle
  playlist is broadcast at once and replayed to the next client. They read
  the GLOBAL registry, so they use playlist ids no other test uses
  (22 501-22 503) and filter the replay to them (other tests record into the
  same global).
- `db/models_playlists.rs`: `all_playlist_ids` returns every id ascending,
  inactive ones too. `tests_ws_replay.rs::a_failed_playlist_read_still_replays_what_the_engine_told`
  closes the pool and checks the fallback (websocket.rs is mutation-excluded).
- `e2e/player-known-state.spec.ts` (review round 2): `/__mock/ws-replay
  {no_song: [{playlist_id, state, transport}]}` + `/__mock/ws-drop` (closes
  every dashboard socket): after the reconnect a playlist waiting with no song
  reads "Nič nehrá", never the old socket's song; a live `Idle` clears the
  song it had.
- `playback/dashboard_replay_tests.rs`: the rules on a private
  `DashboardReplay::default()` (one test per rule, every listed mutant mapped),
  plus the engine glue on the global (ids 22 511 / 22 512): `send_dashboard`
  records AND still broadcasts, `remove_pipeline` forgets.
- The mock (`e2e/mock-api.mjs` `sendReplay`) models the same replay: every
  active playlist + the Dabing playlist (500), playlist 1 playing "Never Gonna
  Give You Up" on program, a running tick's items on top, every mode
  `Continuous` (what the engine plays unless told). A spec that needs a late
  replay or a late first song uses `POST /__mock/ws-replay {delay_ms,
  now_playing_delay_ms}` and resets it with `{}` (it also holds the mock's 2 s
  playlist-1 song interval).
- The mock's limits (no spec depends on them): playlist 1 is always replayed
  playing, whatever `/__mock/set-playing` or the program last said, and an
  off-air ▶ (`playingOffAir`) is not replayed. Modelling the server's
  last-broadcast record in the mock would leak state across specs (one mock
  process serves the whole serial run).
