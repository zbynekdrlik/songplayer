---
paths:
  - "crates/sp-server/src/remote/**"
  - "crates/sp-server/src/obs/remote_call.rs"
  - "crates/sp-server/tests/remote_control.rs"
  - "e2e/settings-remote.spec.ts"
---

# Companion remote control — obs-websocket 5 subset (#213, C of EPIC #174)

The Stream Deck buttons live in Companion. Its OBS module speaks obs-websocket 5
(`SetCurrentProgramScene` per button, with scene-list and program-scene
feedback). SongPlayer serves a compatible subset. Once Companion's OBS
connection points at SongPlayer, the same buttons cut `SP-program` (#209),
with no button rebuilt. Design record: #213 comment 5850413908 (Approach 1).
Research (spec + the Companion module v3.15.3 and 4.0 beta): #213 comment
5850492736.

## What Companion needs (read from its source — do not "simplify" these away)

- **The `obswebsocket.json` subprotocol MUST be echoed.** obs-websocket-js
  requests it. Without it the client fails with "Server sent no
  subprotocol", which the module reports as "Outdated OBS version".
  `protocol::negotiate_subprotocol`:
  - offered → echo it;
  - nothing offered → JSON, no header;
  - msgpack only → HTTP 400.
- **`GetVersion` must succeed with a `supportedImageFormats` ARRAY.** v3 runs
  `forEach` on it and v4 runs `map`, both unguarded.
- **`GetStudioModeEnabled` must succeed.** v3 reads `.studioModeEnabled`
  unguarded. A failure there is "could not get OBS info", which puts the
  module into a reconnect loop.

  Both are answered natively. Studio mode is reported OFF on purpose: Companion
  then sends `SetCurrentProgramScene` per button. The preview requests and
  `TriggerStudioModeTransition` are not mapped.
- **Every other request may fail.** `sendRequest` catches the error and
  returns `undefined`. They get a well-formed `204` whose comment names the
  request. Each type is logged ONCE and listed in
  `remote.unsupported_requests`.
- **Auth close reasons** carry obs-websocket's own wording, because the module
  matches them with a regex:
  - "…missing an `authentication` string…" → "Missing password";
  - "Authentication failed." → an authentication failure.

  Both close with 4009.
- **Feedback** comes from `CurrentProgramSceneChanged {sceneName, sceneUuid}`
  and the scene list from `GetSceneList` + `SceneListChanged {scenes}`. Both
  events are passed through verbatim from cg OBS, with intent Scenes (4).
- v4 sends batches (op 8, SerialRealtime) and a `Reidentify` (op 3) when a
  meter feedback appears. Both are served. Batches always run serially and in
  order; `haltOnFailure` is honoured.
- A `Reidentify` without `eventSubscriptions` KEEPS the current ones.
  obs-websocket's `SetSessionParameters` changes them only when the field is
  present; the spec's "= All" is the INITIAL default.

## Architecture

- `remote/protocol.rs` (pure) holds:
  - the ops, close codes and status codes;
  - Hello / Identified, parse and `check_identify`;
  - `Reply` (its own status, or cg OBS's `requestStatus` passed through);
  - the route table: native / forward / `SetProgramScene` / unsupported.

  `FORWARDED_REQUESTS` = GetSceneList, GetCurrentProgramScene, GetInputList,
  GetSceneItemList, GetGroupSceneItemList. A test pins it in sync with
  `route`.
- `remote/map.rs` (pure) is the scene → `SceneAction` decision:
  - cg OBS did not switch → keep;
  - exactly one playlist → cut to it;
  - otherwise → "OBS manuál" (-1) while `InputSettings::active()`;
  - else keep + a WARN.
- `remote/session.rs` runs one client in a `select!` that is `biased` toward
  events. A cg OBS event that arrived before a client message is therefore
  delivered under the subscriptions active when it arrived.
  - Requests of one session run in order.
  - Remote scene presses are serialized across ALL sessions
    (`Facade::cut_order`).
- `remote/mod.rs` holds:
  - the settings;
  - `RemoteShared` (the telemetry, on `ProgramBus::remote()` like `vban()` /
    `input()`);
  - `Upstream`;
  - `Facade`;
  - `serve` (the accept loop; sessions live in its `JoinSet`, so dropping
    it drops them);
  - `run_remote_config_task`.
- **cg OBS is reached ONLY through SongPlayer's existing OBS client**, never
  over a second connection:
  - `ObsCommand::Remote(RemoteCall::{Request, ScenePlaylists})` is executed
    by `obs/remote_call.rs` on the client's own write half and dispatcher.
  - `ScenePlaylists` is `obs::scene::check_scene_items` over the same
    `NdiSourceMap`. So "the scene shows exactly one playlist" means what
    SongPlayer's scene-go-on playback means by it.
  - Every raw op=5 event cg OBS sends is broadcast as `ObsEvent::Raw` on the
    existing `obs_event_tx`; the engine bridge ignores it.
- **Never block the engine's OBS queue.** `Upstream::call` uses `try_send`
  (a full queue returns 207 at once) and waits at most `UPSTREAM_TIMEOUT`
  (3 s). A call whose requester gave up is skipped on the OBS side
  (`reply.is_closed()`). A stale `SetCurrentProgramScene` queued while cg OBS
  was away must never switch cg OBS seconds after the press was answered
  "not ready".
- **Startup:** `PlaybackEngine::start_program` calls `remote::start_remote`
  with the engine's `obs_cmd_tx` + `obs_event_tx`. `lib.rs` is at 1000/1000:
  its `pub mod remote;` line replaced a redundant comment, and it gained no
  line.

## `SetCurrentProgramScene(X)`

1. Forward it to cg OBS (the migration-time behaviour: everything still on cg
   OBS follows). The client gets cg OBS's own answer: success, or e.g. `600`
   for an unknown scene. It gets `207` when cg OBS is not reachable.
2. Only when cg OBS accepted, look up X's playlists. A lookup that gets no
   answer reads as "no playlist". That is safe: cg OBS already shows X, and
   "OBS manuál" carries cg OBS's mix. It is logged as a WARN and marked
   `reason: lookup_failed` on the cut. That reason wins over
   `input_inactive`: with the input off it is a keep with `lookup_failed`.
3. Cut through `program_bus::persist_and_cut`, the ONE cut path shared with
   `POST /api/v1/program/cut`: persist first, then cut on the boundary after
   next. It is serialized on the bus (`cut_serial`), so an API cut and a
   remote cut never interleave and the persisted source is the one on
   program. A failed persist cuts nothing and returns `205`.
4. Record `remote.last_remote_cut {scene, action (playlist|input|keep),
   source, reason (not_switched|input_inactive|persist_failed|lookup_failed),
   cut_boundary_100ns, at_ms}`.

A request without `sceneName` (a `sceneUuid` only) is answered `300` and is
not forwarded: the cut is by scene name. Companion always sends the name.

## Settings, API, UI

- Keys in `sp_core::config`:
  - `remote_ws_enabled`: only `"true"` enables;
  - `remote_ws_port`: a non-zero `u16`, else 4456;
  - `remote_ws_password`: empty or whitespace = no auth, otherwise kept
    verbatim.
- The settings task re-reads them every 5 s (`listener_plan`):
  - a change rebinds, and the old listener is awaited to let go of the port;
  - disabling stops the listener;
  - a bind failure (the port is taken) shows as `remote.error` and is retried
    on every poll, logged once per distinct error.
- The listener binds `0.0.0.0` (Companion runs on another machine). It is
  opt-in and meant for a trusted LAN.
  - The password gates the WebSocket only. Like every other setting, it is
    stored in plain text and readable through the unauthenticated
    `GET /api/v1/settings`, so it does not protect against a hostile LAN.
  - `RemoteSettings`' `Debug` never prints the password.
- The surface is bounded:
  - a message or frame over `MAX_MESSAGE_BYTES` (1 MiB) ends the session
    unparsed;
  - ONE deadline, `IDENTIFY_TIMEOUT` (10 s), covers the WebSocket
    handshake AND the `Identify`:
    - a socket that never finishes the handshake is dropped;
    - a session that does not identify in time is closed with 4007;
    - in the `biased` select the deadline comes before the client's
      messages, so a ping stream cannot keep an unidentified session alive;
  - at most `MAX_UNSUPPORTED_LISTED` (64) unsupported request types are
    remembered;
  - client-chosen strings (request types, scene names) are clipped to 64
    characters in telemetry and logs (`clip`). The forward itself uses the
    full name.
- Testing the timeout without a wall-time window that correct code can fail:
  - a LATER client that never identifies is the witness. Its close, or its
    dropped handshake, proves the earlier client's deadline passed, and the
    identified earlier client must still be served
    (`an_identified_client_outlives_the_identify_timeout`);
  - on a short-deadline rig every connect / identify is RETRIED when the
    deadline wins first (`connect_in_time`, `identified_in_time`, bounded by
    the test timeout). A dropped handshake or a 4007 there is correct
    behaviour under a stall, not a failure.
- `GET /api/v1/program` → `remote {enabled, port, auth, listening, error,
  clients, requests, last_request, last_remote_cut, unsupported_requests}`.
  `enabled` / `port` / `auth` come from the STORED settings, so a save shows
  at once.
- Nastavenia has the fieldset `settings-remote` with `settings-remote-enabled`,
  `settings-remote-port` and `settings-remote-password`. The mock derives
  `remote` from the stored settings and runs no listener.

## Tests

- `protocol_tests.rs`, pure. Includes the spec's auth vector: password
  `supersecretpassword`, salt `lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=`,
  challenge `+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=` → auth
  `1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4=`, computed independently
  with Python hashlib. That spec password is the ONLY password in tests
  (the staging hook needs `# airuleset:secret-ok <reason>`).
- `session_tests.rs` uses REAL sockets (tokio-tungstenite), a real
  `ProgramBus` and pool, and fakes cg OBS at the `ObsCommand::Remote` channel.
- `mod_tests.rs` covers the settings, the telemetry, `Upstream` on a paused
  clock (the time-out leaves the call marked abandoned; a full queue never
  waits) and the settings task over real ports:
  - bind;
  - a same-port password rebind;
  - disable;
  - a taken port, retried;
  - shutdown.
- `tests/remote_control.rs` runs end to end: client → facade → the REAL
  `ObsClient` → `FakeObsServer`. The harness serves `GetSceneList` /
  `GetCurrentProgramScene` / `SetCurrentProgramScene` from `scene_list` /
  `program_scene`, logs `requests`, and has `push_event`.
- Every wait is bounded (`TIMEOUT` ≤ 20 s); no sleep is used as
  synchronization.

## Residuals (documented, not bugs)

- While cg OBS is disconnected, SongPlayer's OBS client does not drain its
  command queue (capacity 64).
  - Each facade call that happens then parks one `ObsCommand::Remote` there
    until the reconnect, where the abandoned calls are skipped.
  - The facade itself never blocks (`try_send`, 3 s bound).
  - The facade's traffic in an outage is a handful of calls: a Companion
    (re)connect or a button press. SongPlayer's own title `send().await`
    calls queue the same way.
  - Accepted. The fix would need the facade to know the OBS connection state,
    which the engine does not hold.

- In Studio Mode cg OBS can drop a `CurrentProgramSceneChanged` (#170).
  Companion's feedback then misses it exactly as it does when connected to cg
  OBS directly. SongPlayer's own ~2 s poll reconcile emits `SceneChanged`,
  which is not re-emitted as a raw event.
- When Companion connects while cg OBS is down, its scene list stays empty
  until cg OBS emits a `SceneListChanged` or Companion reconnects.
- After B4 (SongPlayer's program replaces cg OBS for every consumer), the
  forward matters only for the manual scenes. Moving the playback trigger
  from "OBS program scene" to "SP program source" is decided in B4.

## Box acceptance (the supervisor's job)

Use a real obs-websocket 5 client (Companion's OBS module, or `obsws-python`)
against `resolume:4456`, with `remote_ws_enabled=true`. It must:

- list cg OBS's scenes;
- on `SetCurrentProgramScene` to a baseline sp-* scene, cut `SP-program` to
  that playlist (program `filled` +0) AND switch cg OBS;
- on a manual scene, cut to "OBS manuál" while the input is enabled.

Box E2E rules apply: never leave the wall on a disruptive scene, and restore
the program scene afterwards.
