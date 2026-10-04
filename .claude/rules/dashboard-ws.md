---
paths:
  - "crates/sp-server/src/api/websocket.rs"
  - "crates/sp-server/src/playback/dashboard_replay*.rs"
  - "crates/sp-server/src/playback/tests_ws_replay.rs"
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
- `runtime_pipeline.rs::remove_pipeline` forgets the playlist (`forget`).
- **No gap between the replay and the live stream:** `handle_ws` subscribes to
  the event bus BEFORE it builds the replay, and the engine records BEFORE it
  broadcasts. A message is in the replay, arrives live, or both (the same value
  twice, harmless).

## The replay

`DashboardReplay::replay(&playlists)`, `playlists` = EVERY row of the
`playlists` table (`SELECT id, playback_mode … ORDER BY id`, inactive ones too:
the UI lists them all):

- a playlist whose last recorded state is not `Idle`: its last `NowPlaying`
  (when it had one) FIRST, then its state;
- any other playlist: an explicit `Idle` (transport `Idle`), no `NowPlaying`;
- a recorded playlist the list does not name (the DB read failed) follows, by
  id, with its recorded mode.

Why each rule:

- **Every playlist gets a state** so the Player can tell "not known yet" from
  "nothing plays" without a new message type (the UI's `state_known`,
  `sp-ui-frontend.md` "The Player never claims a state it was not told").
  Before #225 Idle playlists were skipped as "noise".
- **The song before the state**: each WS message is its own browser task, so
  a state that lands first renders a "known, no song" Player for a moment.
- **The mode is the DB's**: a mode change (`SetMode`) broadcasts no state, so
  the recorded one can be stale.
- **Never the NDI-health sample again.** The old builder
  (`playback_state_replay`, deleted) read `ndi_health_registry.snapshots()`,
  refreshed on the 5 s health tick. Once the Player's badge reads the WS state,
  a reload right after a cut would show the old program state, in the label
  AND the badge, until the playlist's next state change. The recorded message
  is exactly what a live client last got, including the raw transport (#201).

## Tests

- `playback/tests_ws_replay.rs` (the #225 RED): an engine on a `test_state()`
  DB plays a song (state, `Started`, a position tick past the throttle);
  `on_connect_replay` must return its NowPlaying, then its state with the DB's
  mode, then the idle playlist's `Idle`. It reads the GLOBAL registry, so it
  uses playlist ids no other test uses (22 501 / 22 502) and filters the
  replay to them (other tests record into the same global).
- `playback/dashboard_replay_tests.rs`: the rules on a private
  `DashboardReplay::default()` (one test per rule, every listed mutant mapped),
  plus the engine glue on the global (ids 22 511 / 22 512): `send_dashboard`
  records AND still broadcasts, `remove_pipeline` forgets.
- The mock (`e2e/mock-api.mjs` `sendReplay`) models the same replay: every
  active playlist + the Dabing playlist (500), playlist 1 playing "Never Gonna
  Give You Up" on program, a running tick's items on top. A spec that needs a
  late replay or a late first song uses `POST /__mock/ws-replay {delay_ms,
  now_playing_delay_ms}` and resets it with `{}`.
