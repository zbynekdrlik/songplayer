---
paths:
  - "crates/sp-server/src/remote/**"
  - "crates/sp-server/src/playback/program_switch*.rs"
  - "crates/sp-server/src/playback/scene_catalog*.rs"
  - "crates/sp-server/src/obs/remote_call.rs"
  - "crates/sp-server/tests/remote_control.rs"
  - "e2e/settings-remote.spec.ts"
  - "e2e/obs-driver.ts"
---

# Companion remote control — obs-websocket 5 subset (#213, C of EPIC #174)

The Stream Deck buttons live in Companion. Its OBS module speaks obs-websocket 5.
SongPlayer serves a compatible subset. Once Companion's OBS connection points
at SongPlayer, the same buttons cut `SP-program` (#209), with no button
rebuilt. Design record: #213 comment 5850413908 (Approach 1). Research (spec +
the Companion module v3.15.3 and 4.0 beta): #213 comment 5850492736.

#221 (owner ruling, comment 5872272050: Companion switches scenes directly in
SongPlayer; cg OBS is only the NDI input "OBS manuál"; design record comment
5873773896, lanes L1 + L2 + L3 here). The box's 17 page-13 buttons of the
`cg_obs` connection are STUDIO-MODE buttons: each is `preview_scene(X)`,
`wait 10 ms`, `do_transition`, and "ytfast" [0/0] also sends
`set_transition_duration 2000`. The facade serves exactly that, and (L3)
Companion's feedback is SongPlayer's OWN program: `CurrentProgramSceneChanged`,
`SceneTransitionStarted` / `SceneTransitionEnded` and `GetCurrentProgramScene`
come from `SP-program`, never from cg OBS.

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

  Both are answered natively. #221: studio mode is reported ON. v3.15.3's
  `do_transition` sends `TriggerStudioModeTransition` ONLY while its cached
  studio mode is true (else it logs "The Transition action requires OBS to
  be in Studio Mode" and sends nothing), and the cache comes only from
  `GetStudioModeEnabled` at connect and `StudioModeStateChanged` — which the
  facade never emits and never passes through. OFF (the #213 choice) left
  every page-13 button dead.
- **Every other request may fail.** `sendRequest` catches the error and
  returns `undefined`. They get a well-formed `204` whose comment names the
  request. Each type is logged ONCE and listed in
  `remote.unsupported_requests`.
- **Auth close reasons** carry obs-websocket's own wording, because the module
  matches them with a regex:
  - "…missing an `authentication` string…" → "Missing password";
  - "Authentication failed." → an authentication failure.

  Both close with 4009.
- **Feedback** comes from `CurrentProgramSceneChanged {sceneName}` (v3.15.3
  reads only `sceneName`: `index.js` 420-425 sets `scene_active`), and the
  scene list from `GetSceneList` + `SceneListChanged {scenes}`. #221 L3: the
  program-scene event is SongPlayer's own (`remote/studio_events.rs`, below);
  cg OBS's is NOT passed through. `SceneListChanged` is still cg OBS's,
  passed through verbatim with intent Scenes (4).
- `SceneTransitionStarted` / `SceneTransitionEnded` only set Companion's
  `transition_active` variable (`index.js` 530-539, no field read); the
  post-deploy E2E driver waits for the Ended.
- The module reads only `sceneName` from `CurrentPreviewSceneChanged` and
  from `GetCurrentPreviewScene` (`index.js` 426-430, 676-686).
- v4 sends batches (op 8, SerialRealtime) and a `Reidentify` (op 3) when a
  meter feedback appears. Both are served. Batches always run serially and in
  order; `haltOnFailure` is honoured.
- A `Reidentify` without `eventSubscriptions` KEEPS the current ones.
  obs-websocket's `SetSessionParameters` changes them only when the field is
  present; the spec's "= All" is the INITIAL default.

## Architecture

- `remote/protocol.rs` (pure) holds:
  - the ops, close codes and status codes;
  - Hello / Identify / Identified, parse and `check_identify`;
  - `Reply` (its own status, or cg OBS's `requestStatus` passed through);
  - the route table: native / forward / `SetProgramScene` /
    `GetProgramScene` (L3) / `SetPreviewScene` / `GetPreviewScene` /
    `TriggerTransition` / `SetTransitionDuration` / unsupported;
  - the pure request checks (#221): `scene_name` (missing → 300),
    `transition_duration`, `preview_scene_data`, `no_scene` (604,
    obs-websocket's `InvalidResourceState`), and (L3)
    `program_scene_data` / `nothing_on_program` (604).

  `FORWARDED_REQUESTS` = GetSceneList, GetInputList, GetSceneItemList,
  GetGroupSceneItemList (L3: `GetCurrentProgramScene` is native now). A test
  pins it in sync with `route`. The studio requests are never forwarded.
  `passthrough_intent` passes only cg OBS's `SceneListChanged`.
- `remote/studio_events.rs` (#221 L3) holds the facade's OWN events (see
  "Program feedback" below): `FacadeEvent`, `run_program_feedback`,
  `announce_transition`, `wait_transition_end`.
- `remote/map.rs` (pure) is now only the #215 OBS follow's scene →
  `SceneAction` rule. Its `KeepReason` labels (`not_switched`,
  `input_inactive`) are shared with the switch path.
- `playback/program_switch.rs` is the ONE switch path of a press (below).
  `playback/scene_catalog.rs` says which scene is a playlist's.
- `remote/session.rs` runs one client in a `select!` that is `biased` toward
  events: cg OBS's, then the facade's own (`Facade::events`). An event that
  arrived before a client message is therefore delivered under the
  subscriptions active when it arrived, and before that message's answer.
  - Requests of one session run in order.
  - Each session keeps its OWN preview scene (`Session::preview`).
  - Scene presses are serialized across ALL sessions by the bus's
    `switch_order` (`ProgramBus::switch_order`, held for the whole switch).
  - Events a request causes for THIS client (the preview event) are sent
    after its response (after a batch's response for a batch).
- `remote/mod.rs` holds:
  - the settings;
  - `RemoteShared` (the telemetry, on `ProgramBus::remote()` like `vban()` /
    `input()`; `status(&settings, &on_air)` names `program_scene`);
  - `Upstream`;
  - `Facade` (#221 L3: with `events`, the facade's own broadcast);
  - `serve` (the accept loop; sessions AND the listener's program feedback
    task live in its `JoinSet`, so dropping it drops them);
  - `run_remote_config_task`.
- **cg OBS is reached ONLY through SongPlayer's existing OBS client**, never
  over a second connection:
  - `ObsCommand::Remote(RemoteCall::Request)` is executed by
    `obs/remote_call.rs` on the client's own write half and dispatcher.
  - #221 deleted `RemoteCall::ScenePlaylists` (the cg scene-item lookup):
    the facade never asks cg OBS which playlists a scene shows.
  - **The newest press reaches cg OBS last (#221 review rounds 1–5)** — for
    switches cg OBS answers within the OBS client's 2 s (see Residuals). The
    connection loop hands every `Remote` call to ONE forwarder per connection
    (`remote_call::forwarder` → `run_calls`, in the connection's task set).
    A playlist press's mirror is not awaited by the facade, so each of these
    traps would let it land after a later press and leave cg OBS on the
    older scene:
    - **a task PER call** (the #213 shape) can start the newest first: a
      multi-thread tokio worker runs the task it spawned last from its LIFO
      slot. The forwarder writes each frame (`Dispatcher::send`) before it
      takes the next call. Never spawn per call again;
    - **cg OBS runs its messages out of order**: obs-websocket
      (`WebSocketServer::onMessage`) runs every incoming message on a
      `QThreadPool` with no per-client order. The forwarder waits for the
      ANSWER of a scene switch (`ORDERED_REQUESTS` =
      `SetCurrentProgramScene`, at most 2 s) before it writes the next call;
      a getter's answer is awaited in a task of its own.

  - **A superseded switch is never sent (round 3, narrowed in round 4).**
    While cg OBS answers slowly (its UI thread busy), each queued switch
    would hold the forwarder up to 2 s, so the newest mirror could wait
    behind them until its waiter gave up (`wait_mirror`) and be skipped. So
    a switch with a later, still wanted MIRROR queued behind it
    (`RemoteCall::Request.supersedes`, set only by `program_switch`'s mirror
    through `Upstream::enqueue(…, true)`) is
    answered with nothing and never written. A manual press's forward
    supersedes NOTHING: its `SP-program` cut depends on cg OBS's answer,
    which may be a refusal (600) or come too late — a mirror it replaced
    would be lost. A superseded mirror's waiter logs at debug, not WARN.
  - **An awaited switch keeps its requester's verdict (round 5).** Every
    call carries its requester's `deadline` (`Upstream::enqueue`: now + the
    upstream timeout). A manual press's switch (ordered, not a mirror) with
    less than the forwarder's answer timeout left — e.g. queued behind an
    unanswered mirror (2 s) with ~1 s of its 3 s left — is answered with
    nothing and never written (`RemoteCall::too_late`): a switch cg OBS
    answers within its 2 s never lands after the facade answered the press
    "not ready" (a keep; for a later answer see Residuals). A
    mirror goes out however late (its cut already happened), and a getter is
    written while its requester still waits (it changes nothing in cg OBS).
    A call whose requester already gave up is skipped (`reply.is_closed()`).
  - **A mirror's waiter outwaits the forwarder.** `record_mirror` waits with
    `Upstream::wait_mirror` = the upstream timeout + `MIRROR_EXTRA_WAIT`
    (4 s = a switch in flight + the mirror's own answer, each ≤ 2 s; pinned
    to `2 × DEFAULT_RESPONSE_TIMEOUT` by a test), so a mirror that waited
    in the queue and was then answered within the OBS client's 2 s is
    recorded `ok`, not `not_ready`.
  - The forwarder's answer timeout is a parameter (`run_calls(…,
    answer_timeout)`; `forwarder()` passes the production 2 s): the tests
    pass 10 minutes, so a wait the forwarder must not do fails them, and
    none races the 2 s. Pinned on a real WebSocket peer:
    `the_calls_reach_cg_obs_in_queue_order`,
    `a_scene_switch_is_answered_before_the_next_call_goes_out`,
    `a_getter_never_holds_the_next_call_back`,
    `a_switch_a_later_mirror_supersedes_is_never_sent`,
    `a_later_manual_forward_supersedes_nothing`,
    `an_abandoned_later_switch_supersedes_nothing`,
    `an_awaited_switch_with_too_little_time_left_is_never_sent`; the facade
    side by `only_the_mirror_of_a_playlist_press_supersedes_an_earlier_switch`;
    the mirror's waiter by
    `a_mirror_answered_after_the_upstream_timeout_is_still_recorded`
    (paused clock).
  - Every raw op=5 event cg OBS sends is broadcast as `ObsEvent::Raw` on the
    existing `obs_event_tx`; the engine bridge ignores it.
- **Never block the engine's OBS queue.** `Upstream::enqueue` uses
  `try_send` (a full queue returns 207 at once; FIFO, so a call goes out
  after every call queued before it) and `Upstream::wait` waits at most the
  upstream timeout (`UPSTREAM_TIMEOUT`, 3 s; a test may set a longer one
  with the `#[doc(hidden)] pub` `with_timeout` — the end-to-end test uses
  20 s, so a stalled coverage runner never runs a manual press out of its
  time). `request` = both. A call whose requester gave up is skipped on the
  OBS side (`reply.is_closed()`), and a switch with too little of its time
  left is never written (above). A stale `SetCurrentProgramScene` queued
  while cg OBS was away must never switch cg OBS seconds after the press was
  answered "not ready".
- **Startup:** `PlaybackEngine::start_program` calls `remote::start_remote`
  with the engine's `obs_cmd_tx` + `obs_event_tx`. `lib.rs` is at 1000/1000:
  its `pub mod remote;` line replaced a redundant comment, and it gained no
  line.

## Studio mode (#221 L2): what the page-13 buttons get

| Request | Answer |
|---|---|
| `GetStudioModeEnabled` | `{studioModeEnabled: true}`; `StudioModeStateChanged` is never emitted |
| `SetCurrentPreviewScene {sceneName}` | stored on THIS session; 100, then `CurrentPreviewSceneChanged {sceneName}` to this session when it subscribed to Scenes (4); no `sceneName` → 300. No validation: a playlist scene is known from the catalog, cg OBS validates a manual one at the transition (a cached cg scene list would go stale). |
| `GetCurrentPreviewScene` | `{sceneName, currentPreviewSceneName}`: the session's preview, else the program scene NOW (`program_scene_name`), else 604 |
| `TriggerStudioModeTransition` | ALWAYS a switch to that preview (below), never short-circuited when it equals the program scene: a same-source cut is a bus no-op whose publication still counts (the re-kick). No preview and nothing on program → 604. The preview is NOT swapped afterwards. |
| `SetCurrentProgramScene {sceneName}` | a switch to that scene |
| `GetCurrentProgramScene` | (L3) `{sceneName, currentProgramSceneName}`: SP-program's scene (`program_scene_name`), never forwarded; nothing on program → 604 |
| `SetCurrentSceneTransitionDuration {transitionDuration}` | validated like obs-websocket (missing / null → 300, not a number → 401, outside 50..=20000 → 402), 100, NOT applied: `remote.last_transition_duration {ms, applied: false}` (a fraction truncates) |

- **The preview is per session**, not OBS-global: Companion's preview →
  transition pair and a second client (the post-deploy E2E driver) never
  trigger each other's preview, and a client never gets another client's
  preview event.
- **The transition duration is a main-session decision** (design record,
  "Technical decisions"): cg OBS runs a Cut, so the "ytfast" button's 2000 ms
  never had a visible effect, and applying it would change what the
  operator sees today. The program transition stays the Settings value.

## A scene press: the ONE switch path (`playback/program_switch.rs`)

`switch_scene(ctx, X, via)` runs under the bus's `switch_order` for the
whole switch:

1. `kind` = the scene catalog (one `get_active_playlists` read). Unreadable
   → keep, `catalog_failed`, 205.
2. **Playlist(pid)**: `persist_and_cut(pid, catalog name)` FIRST — never
   gated on cg OBS (an unreachable cg OBS still cuts and plays). Then the
   legacy MIRROR, until B4 step 6: `SetCurrentProgramScene(catalog name)` is
   `enqueue`d for cg OBS and NOT awaited under the lock; a spawned
   `record_mirror` waits (≤ the upstream timeout + 4 s) and sets the cut's
   `cg_forward` (`pending` → `ok` | `error <code>` | `not_ready`) only while
   that cut is still the last one (`RemoteShared::set_cg_forward` by the id
   `record_cut` returned). The reply is 100 whatever the mirror does. A
   failed persist cuts nothing, mirrors nothing, 205 `persist_failed`.
3. **Manual**: `SetCurrentProgramScene(X)` to cg OBS FIRST, awaited under
   the lock ("OBS manuál" carries cg OBS's program; a later press must not
   overtake it). Refused → cg OBS's own answer (600) passed through, keep
   `not_switched`; not reachable → 207, keep `not_switched`, `cg_forward
   not_ready`. Accepted → cut to -1 (published with the scene X) while
   `InputSettings::active()`, else keep `input_inactive` (100).
4. **"OBS manuál" itself** (`PROGRAM_INPUT_LABEL`: the resolver's name for
   -1 with no scene, so a transition with no preview after the input was
   restored at startup lands here): the NDI input — cut to -1 while it is a
   source, published with NO scene and with NO cg OBS call (cg OBS keeps
   what it shows, like a dashboard cut to -1), `cg_forward` null; else keep
   `input_inactive`. Before review round 1 the label went to cg OBS as a
   scene name and came back 600.
5. The cut uses the bus's current transition spec unchanged. Manual →
   manual keeps -1 (no mix, `health.cuts` unchanged) and publishes the new
   scene name.
6. `remote.last_remote_cut {scene, action (playlist|input|keep), source,
   reason (not_switched|input_inactive|persist_failed|catalog_failed),
   cut_boundary_100ns, at_ms, via (program|transition), cg_forward}`. The
   follow's `last_follow_cut` has the same shape with `via` / `cg_forward`
   null.

A request without `sceneName` (a `sceneUuid` only) is answered `300` and
nothing is switched. Companion always sends the name.

The dashboard cut on this path, the playback authority and deleting the
follow are the later lanes (L4a–L6) of #221.

## Program feedback + transition events (#221 L3, `remote/studio_events.rs`)

- **`CurrentProgramSceneChanged {sceneName}`** (Scenes, 4):
  `run_program_feedback`, ONE task per bound listener (spawned by `serve`
  into its `JoinSet`), watches `ProgramBus::on_air()`, names it with the one
  resolver and emits whenever the NAME changes (`scene_change`). So it
  follows EVERY cut: a press, a dashboard `POST /api/v1/program/cut`, the
  OBS follow, the startup restore. A publication under the same name (a
  same-scene press, the re-kick) is no event, as in OBS. The value on air
  when the listener starts is not announced (a client reads it at connect).
  -1 with no scene is "OBS manuál". The watch coalesces: A → B → A faster
  than the task runs may announce nothing; the NAME a client last got is
  always the current one.
- **`SceneTransitionStarted` / `SceneTransitionEnded {transitionName}`**
  (Transitions, 16): ONLY a facade switch that cut (`Switched::Cut` in
  `session::switch`; a same-source cut too) calls `announce_transition`:
  Started at once (sent before the waiter exists, so every client gets it
  before Ended), then Ended once `transition.active` clears
  (`wait_transition_end`, polled every 20 ms): at once when nothing is
  mixed (a Cut, a same-source cut), else when the window is served, bounded
  by `TRANSITION_END_MAX_WAIT` (15 s; the longest window is ~10.6 s, so only
  a stalled `SP-program` sender reaches it — then Ended is sent anyway, with
  a WARN). `transitionName` = the kind the program cuts with (`Cut` /
  `Fade`). A kept / refused / failed press and a dashboard cut announce no
  transition.
- **Order a client sees for its own press:** the RequestResponse first (the
  session is busy answering while the events queue), then Started, the
  program-scene event and Ended — the program-scene event may come before or
  after Started (another task emits it), Ended always after Started. Another
  client may get them any time, so a test that asks a second client for
  something must skip events (`request_collecting`).
- `GET /api/v1/program` → `remote.program_scene` = the same resolver.

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
    the test timeout). A dropped handshake, a 4007, or a connection that
    ends with no readable close (on Windows a reset discards the close frame
    when the client's Identify was unread) there is correct behaviour under a
    stall, not a failure. Any OTHER handshake error still fails the test.
- `GET /api/v1/program` → `remote {enabled, port, auth, listening, error,
  clients, requests, last_request, last_remote_cut, unsupported_requests,
  last_transition_duration}`.
  `enabled` / `port` / `auth` come from the STORED settings, so a save shows
  at once. #221 L3: `program_scene` (SP-program's scene name, `null` while
  nothing is on program).
- Nastavenia has the fieldset `settings-remote` with `settings-remote-enabled`,
  `settings-remote-port` and `settings-remote-password`. The mock derives
  `remote` from the stored settings and runs no listener.

## tungstenite 0.26 facts this code relies on
- `accept_hdr_async_with_config(stream, callback, Some(config))`:
  `WebSocketConfig` is `#[non_exhaustive]`, so build it with its builders
  (`.max_message_size(Some(n)).max_frame_size(Some(n))`).
- A frame over the limit is `Error::Capacity` on read, with NO close frame.
  The session breaks, and the unread data turns the close into a reset.
- The tungstenite CLIENT verifies the subprotocol:
  - requested but not echoed → `NoSubProtocol`;
  - echoed but not requested → `ServerSentSubProtocolNoneRequested`.

  That is why the tests prove the echo simply by connecting.
- A handshake the server drops surfaces on the client as
  `Protocol(HandshakeIncomplete)` (EOF) or `Io` (a reset).

## Tests

- `protocol_tests.rs`, pure. Includes the spec's auth vector: password
  `supersecretpassword`, salt `lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=`,
  challenge `+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=` → auth
  `1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4=`, computed independently
  with Python hashlib. That spec password is the ONLY password in tests
  (the staging hook needs `# airuleset:secret-ok <reason>`).
- `session_tests.rs` uses REAL sockets (tokio-tungstenite), a real
  `ProgramBus` and pool (with the playlists sp-fast = 7, sp-slow = 3: the
  switch reads them), and fakes cg OBS at the `ObsCommand::Remote` channel.
  It is at ~925/1000 lines: its rig (`rig_on`, `connect`, `request`, `press`,
  `last_cut_json`, …) is `pub(super)`, and new facade tests go to
  `session_tests_studio.rs` (#221: the switch path and the studio buttons).
  - Telemetry fields are read through the API's own JSON
    (`last_cut_json`), so a test never depends on a struct field it only
    wants to see serialized.
  - "A playlist press never waits for cg OBS" is proven with a fake that
    HOLDS every `SetCurrentProgramScene` reply open and a rig whose upstream
    timeout is 10 minutes: an awaited mirror could never be answered inside
    the test's 10 s bound. Never prove it with a wall-time threshold.
  - The switch order: the test holds `bus.switch_order()`, sends a press,
    and requires NO answer for 200 ms (the safe direction), then releases.
    `a_manual_press_holds_the_switch_order_until_cg_obs_answers` pins that
    the lock spans a manual press's awaited forward: a second client's
    playlist press cannot cut while cg OBS holds the manual switch.
- `session_tests_feedback.rs` (#221 L3, same rig): SongPlayer's own
  `CurrentProgramSceneChanged` on a press AND a dashboard cut (no transition
  events for the latter), none for a same-name publication, Started → Ended
  at once for a Cut and only after the window is served for a fade (the test
  serves it with `bus.release_due(now + 1 min)`: with no source live, every
  due boundary is filled and the window is pruned), no transition for a
  press that cut nothing, `GetCurrentProgramScene` native (604, never
  forwarded). Its `pub(super) request_collecting` answers a request and
  returns the events that came before the response — use it whenever
  another client's press can have queued events.
- `studio_events_tests.rs`: `scene_change`, the feedback task over a real
  bus (a same-name publication is checked with `yield_now` rounds on the
  current-thread runtime + `try_recv` Empty), the event shapes, and
  `wait_transition_end` / `announce_transition` on a paused clock (at once
  for a Cut, the served window, the 15 s bound).
- `playback/program_switch_tests.rs`: `cg_forward_label`, the records, and
  `record_mirror` (its answer lands only on its own cut; an answer after the
  upstream timeout is still recorded, and the wait is bounded).
- `mod_tests.rs` covers the settings, the telemetry, `Upstream` on a paused
  clock (the time-out leaves the call marked abandoned and its `deadline` is
  the enqueue time + the timeout; a full queue never waits) and the settings
  task over real ports:
  - bind;
  - a same-port password rebind;
  - disable;
  - a taken port, retried;
  - shutdown.
- `tests/remote_control.rs` runs end to end: client → facade → the REAL
  `ObsClient` → `FakeObsServer`. The harness serves `GetSceneList` /
  `GetCurrentProgramScene` / `SetCurrentProgramScene` from `scene_list` /
  `program_scene`, logs `requests`, and has `push_event`. #221 L3: after the
  page-13 pair Companion gets SongPlayer's own `CurrentProgramSceneChanged`;
  a pushed cg OBS `CurrentProgramSceneChanged` never arrives before the
  pushed `SceneListChanged` witness (the OBS client's reader broadcasts raw
  events in wire order).
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

- **The newest-press-last order has a limit.** It holds for switches cg OBS
  answers within the OBS client's answer timeout (2 s). A switch cg OBS
  answers later is given up by the forwarder (the dispatcher's timeout) and
  the next call is written; obs-websocket runs messages on a thread pool, so
  cg OBS may still carry out the late switch after the newer one. Waiting
  longer would hold every later press behind a stalled cg OBS; the next
  press (or the operator) corrects it, and `cg_forward` shows the late one
  as `not_ready`.
- **A switch cg OBS answers after the OBS client's 2 s is reported as not
  switched although it may have happened.** The deadline rule only decides
  whether a switch is WRITTEN (`too_late` is checked just before the write,
  with no margin for the write itself); a written frame cannot be recalled.
  So a manual press whose switch cg OBS carries out after 2 s is answered
  "not ready" (207) and the program is kept while cg OBS (and so "OBS
  manuál") did switch; a mirror cg OBS answers after 2 s is recorded
  `cg_forward: not_ready` although cg OBS followed. The next press
  corrects it.
- **Companion's feedback at CONNECT is still cg OBS's.** v3.15.3's
  `buildSceneList` sets `scene_active` from the forwarded `GetSceneList`'s
  `currentProgramSceneName` (`index.js` 1102-1115), which is cg OBS's
  program. Until B4 step 6 the mirror keeps cg OBS on SP-program's playlist
  scene, so they differ only after SongPlayer cut to "OBS manuál" with no
  scene (a startup restore, a dashboard cut to -1); the next program-scene
  event corrects it. Found in L3, not in the design record — the main
  session decides whether the forwarded answer is patched.
- When Companion connects while cg OBS is down, its scene list stays empty
  until cg OBS emits a `SceneListChanged` or Companion reconnects. The
  page-13 buttons send their stored names verbatim, so they keep working:
  playlist presses cut, manual presses answer not-ready.
- The E2E driver (`e2e/obs-driver.ts`) raises its transition flag BEFORE
  `TriggerStudioModeTransition`: the facade ends a Cut at once, and Node's
  `ws` may hand the response and the Ended frame over in one tick, so a flag
  raised after the call's promise could undo the Ended and stall the wait.
- Until #221 L4b the PLAYBACK is still driven by cg OBS's scene detection:
  a playlist press cuts `SP-program` at once, but the playlist starts
  playing when the mirror has switched cg OBS. A failed mirror leaves the
  playlist on `SP-program` paused (`cg_forward` says so).
- Hand switches in cg OBS's own UI are invisible to the facade (no cg
  tracking, by the owner's ruling): the next press decides.

## Box acceptance (the supervisor's job)

Use a real obs-websocket 5 client (Companion's OBS module, or `obsws-python`)
against `resolume:4456`, with `remote_ws_enabled=true`. It must:

- report studio mode ON and list cg OBS's scenes;
- on `SetCurrentPreviewScene(baseline sp-*)` + `TriggerStudioModeTransition`
  (the page-13 pair), cut `SP-program` to that playlist (program `filled`
  +0, `last_remote_cut.via = transition`) and then switch cg OBS
  (`cg_forward` → `ok`), with no `GetSceneItemList` from the facade;
- on a manual scene, switch cg OBS first, then cut to "OBS manuál" while the
  input is enabled;
- on `SetCurrentSceneTransitionDuration 2000`, answer 100 and leave
  `transition.duration_ms` unchanged (`last_transition_duration {ms: 2000,
  applied: false}`);
- keep `unsupported_requests` free of the studio requests;
- (L3) after each press send SongPlayer's `CurrentProgramSceneChanged` with
  the button's scene (Companion → Variables `cg_obs:scene_active`), then
  `SceneTransitionStarted` / `SceneTransitionEnded`; answer
  `GetCurrentProgramScene` with `remote.program_scene`; after a dashboard
  `POST /api/v1/program/cut` send only the program-scene event;
- (L3) run the post-deploy E2E with its scene driver on :4456
  (`FACADE_WS_URL`), `remote_ws_enabled=true` seeded by CI.

Box E2E rules apply: never leave the wall on a disruptive scene, and restore
the program scene afterwards.
