---
paths:
  - "crates/sp-server/src/obs/**"
  - "crates/sp-server/src/obs_bridge.rs"
  - "crates/sp-server/tests/common/mod.rs"
  - "crates/sp-server/tests/obs_reconnect.rs"
  - "crates/sp-server/tests/fake_obs_handshake.rs"
  - "crates/sp-server/src/playback/ndi_health.rs"
  - "crates/sp-server/src/playback/ndi_health_expect*.rs"
  - "crates/sp-server/src/playback/ndi_health_log.rs"
  - "crates/sp-server/src/playback/ndi_health_tests*.rs"
  - "crates/sp-server/src/playback/startup_pipelines.rs"
  - "e2e/post-deploy.spec.ts"
  - "e2e/ndi-health-gate.ts"
  - "e2e/ndi-health-gate.spec.ts"
  - "e2e/post-deploy-dabing.spec.ts"
  - "e2e/post-deploy-av-sync.spec.ts"
  - "e2e/av-sync-gate.ts"
  - "e2e/av-sync-probe*.ts"
  - "e2e/av-sync-evidence.ts"
  - "e2e/obs-audio-wait*.ts"
  - "e2e/obs-driver.ts"
  - "scripts/av_sync_check.py"
  - "scripts/av_sync_drift.py"
  - "scripts/av_sync_warp.py"
  - "scripts/tests/test_av_sync_check.py"
  - "scripts/tests/test_av_sync_warp.py"
  - "scripts/tests/test_av_sync_profile.py"
---

# OBS ↔ NDI health, the `SP-program` receiver, the post-deploy A/V gate

## The one NDI sender is `SP-program` (#221 lane 3)

Every consumer takes SongPlayer's PROGRAM: the LED wall `SP-program-MAX`
(Spout), the Presenter, strih and the stream `SP-program` (NDI), FOH its
VBAN. cg OBS is only the NDI input "OBS manuál" (`ndi-input.md`). Lane 3
(ROZHODNUTÉ 5877969167, plan 5999882988) retired the per-playlist NDI senders
(`SP-slow`, `SP-fast`, …): a playlist pipeline feeds the program bus and
nothing else (`pipeline-testability.md`). Deleted with them — do not bring any
of it back:

- the #127/#173 receiver-recovery ladder (`obs/ndi_recovery*.rs`,
  `obs/ndi_remove.rs`, `playback/ndi_recovery_trigger.rs`,
  `ObsCommand::NudgeNdiReceiver`), the manual `POST /api/v1/ndi/recover/{id}`
  and `recovery_step`;
- the NDI source map (`obs/ndi_discovery.rs`: `NdiSourceMap`, the rebuild
  signal from the playlist CRUD, `canonical_sender_name`), and with it the
  #196 "no OBS scene for this output" reason;
- the dark-wall reason of a playlist output, the #196 post-restart receiver
  self-check, its persisted receiver baseline (`db/models_ndi.rs`; the stale
  `ndi_last_receivers_<id>` settings rows are left in the DB, harmless) and
  the HealthBar `health-ndi` segment;
- the #196 sender-URL discovery (`sp-ndi` `find.rs` / `source_url.rs`,
  `send_get_source_url`, `discover_local_sources`, `sender_url`);
- the #151 burn-id overlay (`burn_on`, `POST /api/v1/ndi/burn`) and the
  `genlock_pacing` switch with the SDK-clocked path (`genlock.md`);
- the lock rule "no receiver" (`sp_core::genlock::lock_state::derive`).

NEVER a per-sender `PipelineCommand::RecreateSender` (CLAUDE.md "Disabled
subsystems", #60) — it never could fix a receiver-side binding.

## `/api/v1/ndi/health` = the playlist pipelines' health

The route keeps its name (the dashboard, the post-deploy E2E and camera-box
read it). One row per pipeline (`ndi_health.rs::PipelineHealthSnapshot`):
`ndi_name` (the playlist's `ndi_output_name`, its scene label), `state`,
`transport`, the frame counters and fps, `consecutive_bad_polls`,
`degraded_reason`, `clock`, `pacing`, `audio`, `lock_state` / `lock_reason`.
No receiver field: a row's `degraded_reason` is an underrun
(`underrunning (x/y fps)`) or a stalled delivery (`no frames in 10s`) at
≥ 2 consecutive bad polls while Playing on program (`compute_degraded_reason`;
a bad poll is classified by `pipeline::classify_bad_poll`). Pinned by
`ndi_health_tests.rs::a_health_row_carries_no_ndi_sender_fields`.

- `handle_health_snapshot` is sync + `mutants::skip` glue; its log lines are
  `ndi_health::health_log::log_health_snapshot` (child module
  `ndi_health_log.rs`, logging only: degraded / recovered and the
  once-per-minute heartbeat + genlock + loop-stats lines). Add new
  per-snapshot logging there, not to the handler.
- Compose new per-pipeline state into `NdiHealthRegistry` (the `Arc` the
  engine already holds) rather than a new `PlaybackEngine` field
  (`playback/mod.rs` is near the cap).

## `SP-program` expects a receiver while a source is on program

`ndi_health_expect.rs::program_degraded_reason(source, the polled count)`,
pure: served as the top-level `degraded_reason` on `GET /api/v1/program` —
`"no NDI receiver on SP-program"` (`PROGRAM_NO_RECEIVER_REASON`) while a
source (a playlist, or -1 "OBS manuál") is on program and
`health.connections < 1`, else `null`. The `SP-program` thread polls the
count about once a second (`ProgramBus::set_connections`); until its first
poll the count reads 0 and is no reading (`health.receivers_polled`,
serde-skipped), so no reason is named then. With no sender at all (no NDI
SDK, the sender or its thread could not be created) nothing will ever poll,
so `spawn_program_thread` sets a polled 0 at once and the reason is named.
`ProgramCore::set_connections` logs it through the pure, mutation-scored
`ndi_health_expect::receiver_log` (a WARN when SP-program turns dark, an INFO
when the first poll finds a receiver or the first one comes back; the logger
is `mutants::skip`, so change the decision there). The first poll runs on
the first served pair, before any receiver can re-attach, so every start
with a source on program logs the WARN, then the INFO once a receiver
attaches: the pair times the receivers' re-attach after a restart.
Tests: `ndi_health_expect_tests.rs`, `api/program_tests.rs`.

## `SP-program`'s port is pinned across a restart (#196)

DistroAV's genlock build reconnects a stale source BY URL with the PINNED
previous port (`reset_ndi_receiver: connect BY-URL '10.77.9.201:5970'`), and
the NDI runtime hands each `send_create` the next free TCP port in creation
order. `SP-program` is the only sender, so on Windows `start_program` first
waits (≤ 10 s) until the previous instance has released the span
(`startup_pipelines::wait_for_program_ports` → `wait_for_ports_free` +
`ndi_ports_free` over `ndi_port_range(NDI_SENDERS)` = 5960..5961, the ports
SongPlayer's own process listens on), then creates it: a restart gets the
same port again. A span still busy after the bound is a WARN naming the
busy ports, never a blocked start. #240: never add a "margin" port past
them — cg OBS's own NDI sender listens on 5962 + 5963, so the old
`base..=base+N+1` span never came free and every start since 8.10.2026
waited the full 10 s before SP-program, VBAN and ASIO started. The pure pieces are Linux-tested in
`startup_pipelines.rs`. The port moves ONCE, at the 0.71.0-dev.16 deploy that
retires the per-playlist senders (they took the ports before it): see "The
cutover" below.

- **GOTCHA — `NDIlib_send_get_source_name().p_url_address` is EMPTY for a
  local sender** (#196): a sender's own `host:port` is not readable from the
  sender side; read it on a receiver (or with `NDIlib_find`, which the NDI
  input's source list uses, `ndi-input.md`).
- **The stored `ndi_source_name` host case MUST equal the advertised name**
  (#173): NDI advertises `"<HOST> (<stream>)"` with `<HOST>` = Windows
  `COMPUTERNAME` (`RESOLUME-SNV`), NOT `hostname` (lowercase). DistroAV's
  re-match after a sender re-announce is case-sensitive, so a cg OBS input
  stored with the lowercase host never re-attaches. The A/V gate's probe
  input is named from `COMPUTERNAME` for exactly this reason.

## The cutover (0.71.0-dev.16) — what a deploy of lane 3 changes outside SongPlayer

- **cg OBS's `sp-*` inputs lose their senders.** `SP-slow`, `SP-fast`, … no
  longer exist, so those inputs (and their scenes) show black. DistroAV's
  `ndi_behavior` 0 is KEEP_ACTIVE, and cg OBS's genlock build forces it on
  every input with `genlock_fifo` on (the default,
  `camera-box/vendor/distroav/src/ndi-source.cpp`): those inputs keep
  receivers whether shown or not, and the genlock build reconnects a stale
  source BY URL to its pinned port. The input pinned to the port `SP-program`
  now takes would land on `SP-program`: it would show the program inside an
  `sp-*` scene (a picture loop if cg OBS shows that scene while "OBS manuál"
  is on program) and count as a receiver of `SP-program` (masking a lost
  Presenter / strih / stream). Required BEFORE the lane-3 commits reach
  `dev` (main session / owner, never the E2E): a push to `dev` deploys AND
  runs the post-deploy E2E at once, and both its dark gate and the A/V
  gate's receiver-rise wait read `SP-program`'s receivers, so a stale input
  that lands on the new port fails or masks them on the very first run. Set
  every stale `sp-*` input's `ndi_source_name` to `""` (an empty source
  stops a DistroAV receiver), then after the deploy check
  `GET /api/v1/program` `health.connections` = the real consumers. NEVER
  remove those scenes (release 0.71.0 review): the facade forwards cg OBS's
  scene list as it is (`with_songplayer_scenes` rewrites only the current
  program / preview fields), so Companion's playlist buttons and the
  post-deploy suite (`scenes.includes("sp-fast")`, `pickBaselineScene`, the
  A/V gate's baseline) find a playlist by its `sp-*` scene NAME in it.
- **`SP-program`'s port moves once** (it used to be created after the
  playlist senders): its consumers (the Presenter, strih, the stream)
  reconnect to the pinned OLD port and must re-resolve; confirm each one
  shows `SP-program` again after the deploy.
- The owner's rule (5.10.2026): name every consumer still on an old source
  and ASK him to re-point it, never assume.

## One restart per push (#196 round 2)

The Deploy job starts SongPlayer; the post-deploy E2E job used to restart it
AGAIN. `/api/v1/status` carries `uptime_s` (`crate::process_start`, marked at
the top of `lib::start`), and the E2E "Restart SongPlayer" step SKIPS the
restart (`exit 0`) when the running process reports the deployed `VERSION`
(from the checkout) AND `uptime_s < 600` — i.e. it IS the fresh
Deploy-started process — logging which branch it takes. The OBS client's
reconnect backoff covers the "pick up OBS after OBS start" case.

## obs-websocket 5.x write-path gotchas (the A/V gate's probe scene, and any scene item)

Learned on the #173 ladder (deleted), still true for any input / scene-item
write — the A/V gate provisions its probe scene this way (`e2e/obs-driver.ts`):

- **There is no "which scenes contain source X" request.** Resolve a scene
  item by `GetSceneItemId {sceneName, sourceName}` (600 = not in that scene),
  or scan `GetSceneList` → `GetSceneItemList` per scene.
- **`GetInputSettings` returns both `inputSettings` AND `inputKind`** (600 =
  no such input). DistroAV's `ndi_behavior` 0 is KEEP_ACTIVE (the receiver
  runs whether the input is shown or not), `ndi_behavior_timeout` 1 is "keep
  content" (not 1 second), and cg OBS's genlock build forces KEEP_ACTIVE on
  every `genlock_fifo` input. Only an EMPTY `ndi_source_name` stops a
  receiver.
- **`CreateInput` adds the scene item at the TOP of the scene** and returns
  its `sceneItemId`. Fit it with `SetSceneItemTransform` (bounds = the
  `GetVideoSettings` base size, `OBS_BOUNDS_SCALE_INNER`). A round-tripped
  `sceneItemTransform` carries read-only fields (`width`, `height`,
  `sourceWidth`, `sourceHeight`) that OBS REJECTS as out-of-range — strip
  them before writing one back.
- **`RemoveInput` of a RECEIVING DistroAV `ndi_source` reports success but
  does nothing** (the libobs destroy blocks on the receiver thread): clear
  `ndi_source_name` to `""` first, then remove, then READ BACK. And
  **`RemoveInput` frees the name ASYNCHRONOUSLY**: reusing it at once races
  the teardown (`601 "a source already exists by that new input name"`).
  The A/V gate therefore never removes its probe input; it idles it
  (`""`) outside the take and points it at `SP-program` only for it.
- Always log the full obs-websocket error on a failed write
  (`d.requestStatus.code` + `comment` + the step).
- **obs-websocket-js 5.0.8 never settles a `call` / `reidentify` whose
  socket closes** (`onClose` emits `ConnectionClosed`, then `cleanup()`
  drops the internal listeners the pending promise waits on; a LATER call
  throws "Not connected"). A wait that must end listens to
  `ConnectionClosed` itself, as `ObsDriver.waitForInputAudio` does
  (#221 dev.18).

## The OBS client serves the #213 remote control (`ObsCommand::Remote`, `ObsEvent::Raw`)

- `ObsCommand::Remote(remote_call::RemoteCall)` runs a forwarded request
  for the Companion facade, on this ONE connection (`obs/remote_call.rs`). A
  call whose requester gave up (`reply.is_closed()`) is skipped. The facade
  decides from SongPlayer's own playlists. The facade's calls go through ONE
  forwarder per connection, `remote_call::run_calls`, in queue order (never
  a task per call), and a scene switch's answer is awaited before the next
  call is written (cg OBS runs messages on a thread pool): see
  `remote-control.md`.
- `ObsCommand` is not `Clone`: it holds a oneshot sender.
- The reader broadcasts EVERY op=5 event as `ObsEvent::Raw { event_type,
  event_data }` on `obs_event_tx` — `Raw` is `ObsEvent`'s ONLY variant. The
  facade passes on only `SceneListChanged`
  (`remote::protocol::passthrough_intent`).
- The obs-websocket SERVER side (the facade) lives in `crate::remote`; see
  `remote-control.md`.

## Mutation gate: `obs/**` is EXCLUDED

`ci.yml` runs `cargo mutants --in-diff` with `obs/` excluded
(`.cargo/mutants.toml`) — pure logic in `obs/` is NOT mutation-scored (still
unit-test it, but survivors there won't fail CI). Code in
`playback/ndi_health*.rs` **is** scored: every new non-`mutants::skip` fn
there needs tests that kill its mutants.

## E2E dark gate (post-deploy suite) — on `SP-program` (#127, #221 B4 step 6)

`post-deploy.spec.ts` "SP-program has a live NDI receiver — the program is not dark" polls `GET /api/v1/program` (≤ 60 s, no scene switch) until `programReceiverVerdict` says `ok`: a source on program, `health.connections > 0` and the server's `degraded_reason` `null`. `connections`: `>0` live, `0` dark (also right after a start, before the sender's first poll, when the server names no reason yet), `<0` no valid reading (the SDK's error value) — keep polling on both; `source: null` = nothing on program, a failure too. `post-deploy-dabing.spec.ts` asks the same of SP-program next to "the Dabing pipeline is up" (the dub takes the program). Pure decision logic lives in `e2e/ndi-health-gate.ts` (unit-tested by `ndi-health-gate.spec.ts` in the ubuntu **mock** suite — a `test()` that never touches `page` runs with no browser/box). Keep the baseline-scene discipline (CLAUDE.md "E2E must not switch to disruptive OBS scenes").

## Gotcha: `e2e/post-deploy-report/index.html` is a TRACKED artifact

Playwright runs regenerate it; it shows up as ` M` in `git status`. `git checkout -- e2e/post-deploy-report/index.html` before committing so it never lands in your diff.

## cg OBS's program is NOT read any more (#221 L6)

Design record 5873773896 §1g "L6": SongPlayer's own program drives playback
(L4b), so the OBS client's scene detection is DELETED, not kept as a
fallback — do not bring any of it back: `obs/scene.rs`, `obs/scene_poll.rs`,
the connect-time `GetCurrentProgramScene` read, `ReaderMessage::SceneChange`,
`obs/snapshot.rs` (`ObsSnapshot`), `obs/transition.rs`, `ObsState.{current_scene,
active_playlist_ids, lookup_failed, transition}`,
`text::get_current_scene_request`, and their tests and `FakeObsServer`
pieces.

What the client still does (`obs/mod.rs` module doc): the facade's calls
(`remote_call.rs`), the title text, cg OBS's raw events, and `ObsState` =
`connected` (`/api/v1/status.obs_connected`) + the #154 stream/record state.
The identify subscribes Scenes (4: `SceneListChanged` for the facade) |
Outputs (64: #154) = `EVENT_SUBSCRIPTIONS` (68, pinned in `mod_tests.rs`).
The dashboard's connect-time `ObsStatus.active_scene` is SP-program's scene
(`program_scene_name`), never cg OBS's.

- **The connection's helper tasks are reaped** (#218 review round 4): every
  helper goes through `spawn_helper(&mut spawned_tasks, …)`, which first
  `try_join_next`s the finished ones (a `JoinSet` keeps a finished task until
  joined, and the connection loop never joins: one helper per title text)
  and WARNs a helper that panicked. Never call `spawned_tasks.spawn`
  directly.

## Reading the health snapshot's `state` — `Playing` already means "on program" (#154)

`handle_health_snapshot` RECONCILES the pipeline-reported state before
storing it: a pipeline that is `Playing` but whose scene is NOT on program
(`scene_active == false`) is stored as `Paused`. So a consumer that reads
`NdiHealthRegistry::snapshots()` and checks `state == PlaybackStateLabel::Playing`
is already getting "a playlist is playing AND it is on program". The #154
lyrics idle gate relies on exactly this (`lyrics/idle_gate.rs::any_playing`).
Read the registry in-process (the engine already holds the `Arc`); never
HTTP-loop `/api/v1/ndi/health` back to your own server.

## Post-deploy A/V gate (#147) — lipsync + audio dropouts on the REAL output

**Rule: no change to decode, pacing, the mixer, NDI or the audio path merges
unless this gate is green.** The owner saw a major lipsync regression while
every other gate was green. This gate is the one that measures a real output
end to end. Until #221 B4 step 6 that was what the wall took (cg OBS's
program); since #221 lane 3 it records SongPlayer's PROGRAM, `SP-program`,
the output every consumer takes, through cg OBS's own probe scene (below).

- **Where it runs:** `e2e/post-deploy-av-sync.spec.ts`, inside the E2E job's
  "Feature-level Playwright (post-deploy spec)" step (`post-deploy.config.ts`
  matches `post-deploy*.spec.ts`). It uses the shared `ObsDriver` (obs-websocket)
  twice (#221 L3): the SCENE driver on SongPlayer's facade (`FACADE_WS_URL`,
  :4456 — Companion's studio-mode path, SongPlayer's own program feedback and
  transition events), and a second one on cg OBS (`OBS_WS_URL`, :4455) for
  `GetProfileParameter` / `StartRecord` / `StopRecord` /
  `GetRecordStatus` (the recording is cg OBS's program) and — #221 lane 3 —
  the gate's own PROBE SCENE (`e2e/av-sync-probe.ts`, design question
  6004634711 option 1):
  - `AV_PROBE_SCENE` "A/V gate (SP-program)" holds ONE DistroAV input,
    `AV_PROBE_INPUT` "A/V gate SP-program". The gate provisions it itself
    (`ensureProbeScene` + `probeSteps`): the scene and the input are
    created when missing (the settings copied from `sp-slow_video`, else an
    `sp-*_video`, else any cg OBS NDI input, `pickTemplateInput`, with
    `genlock_fifo` on (the certified receive path: DistroAV then forces
    source-timecode sync, KEEP_ACTIVE, the highest bandwidth, normal
    latency), `ndi_audio` on, `genlock_monitor` and `genlock_burn` off and
    `ndi_bw_mode` 0 forced, `PROBE_FIXED_SETTINGS`), put into the scene when
    it is not there, and an existing probe RESET on every run (its fixed
    settings again and idle, `probeIdleSettings`: a hand edit of its
    settings never survives a run; a hidden scene item or a muted input is
    not undone, the take then fails as unmeasurable), NEVER removed (a
    receiving DistroAV input does not delete reliably, above).
    It is not an sp-* name, so it is never a playlist scene in SongPlayer's
    catalog, and `pickBaselineScene` never picks it. Cost: one permanent
    technical scene in the owner's cg OBS scene list (a press of it by hand
    during a take while "OBS manuál" is on program would loop the picture).
  - **The probe is IDLE outside the take** (`ndi_source_name` `""`, set at
    provisioning — also after a run that died mid-take — and in
    `afterAll`): DistroAV keeps a receiver whether the input is shown or not
    (KEEP_ACTIVE, above), so a probe left pointed at SP-program would hold
    a receiver on it forever and the dark gate above could no longer see the
    real consumers go.
  - The take runs only while `/api/v1/program.source` is the baseline
    playlist (`programCarriesBaseline`): with "OBS manuál" (-1) on program
    cg OBS would record itself through SP-program.
  - The gate reads SP-program's receivers with the probe idle, once two
    reads ~1.5 s apart agree (`receiversSettled`, ≤ ~15 s), then points the
    probe at `"<COMPUTERNAME> (SP-program)"` (else the host the template
    input names, `programSourceName`), switches cg OBS to the probe scene
    when it is not on it already, and waits (≤ 30 s) until SP-program's
    `health.connections` rose above that count (`probeReceiverAttached`);
    it fails naming it, never as an unmeasurable take.
  - **A freshly attached probe's VIDEO reaches the recording before its
    AUDIO: wait for its audio meter before recording (#221 dev.18) — and
    know what that meter can see (below).** The receiver count rises
    as soon as DistroAV connects, and the picture flows at once. The audio
    reaches cg OBS's MIX (what StartRecord records) only with gaps until
    camera-box's genlock audio pairing (camera-box 1367) has fixed the
    delay: by their design the pairing withholds the packets from the mix
    until its latch locks. The dev.17 cg OBS log (local time):
    - the probe's scene reset 08:49:31.566;
    - DistroAV bound the source 33.114;
    - `genlock-shallow-lock` 36.133;
    - DEGRADED `audio_pairing` 36.217;
    - LOCKED 37.214 (4.1 s after the bind).

    The gate logged its take start at 35.705: 4.1 s after the reset and
    2.6 s after the bind. The take opened with two dropouts (0.100 s / 22 ms,
    0.227 s / 234 ms, run 37423917199). Its audio came at 0.122–0.227 s,
    stopped until 0.461 s, and was clean from there. Before lane 3 the gate
    recorded a long-attached input and never met this. So before EVERY take
    the gate waits for the probe's audio (`ObsDriver.waitForInputAudio`, the
    pure decision in `e2e/obs-audio-wait.ts`), and only then reads the
    playing video and records:
    - **Signal:** obs-websocket's `InputVolumeMeters`. Each input's
      `inputLevelsMul` is one `[magnitude × volume, peak × volume, peak]`
      triple per channel, linear. The gate reads the THIRD value, the input
      peak BEFORE the fader and mute (does the receiver deliver audio?),
      from the loudest channel. A muted probe still fails as unmeasurable.
    - **Condition:** above −60 dBFS for 1 s in a row. Three things restart
      the run:
      - a silent reading;
      - an event without the probe (obs-websocket meters only ACTIVE
        inputs, those on the PROGRAM feed — `obs_source_active`; an input
        only on a preview is not metered. The probe is on cg OBS's program
        from the scene switch);
      - more than 500 ms between meter events (only observed continuity
        counts).
    - **Bound:** 20 s. It rejects with the meter state it saw: the events,
      the ones with the probe, its last peaks, the loudest, and the longest
      run. It also says what that state means (`explainAudioWait`):
      - no event at all: the subscription did not apply;
      - events, but never the probe: the probe is not active (cg OBS is not
        on the probe scene, or its item is hidden);
      - the probe's ONLY run above the floor came after a METERED silence (a
        reading of the probe at or below the floor) and was still open when
        the bound hit (`runsAboveFloor` 1 + `readingsAtOrBelowFloor` > 0 +
        `openRun`, rounds 4-6): its audio started late.
        - The text names only what was observed: when the run began and
          its last reading, both before the bound.
        - An open run is one whose last reading is within the gap bound of
          the end, the same `<=` as the streak.
        - A probe that became active late (absent from the events, never
          read silent) is not a late start; it falls to the next case;
      - the probe above the floor at times, but never for the hold, in any
        other way (several runs, or one that ended; rounds 3-5): DistroAV
        delivers it with gaps, or the meter events stopped (more than
        500 ms apart, or the probe left the program feed). It names the
        number of runs. Gappy audio whose bound lands mid-burst stays here,
        never a "late start". Never "no audio" next to readings that show
        audio;
      - the probe metered, never above the floor: DistroAV delivers it no
        audio (SP-program carries no sound, or the probe's `ndi_audio` is
        off).

      The explanation uses the wait's OWN floor, hold and gap. None of
      these cases is ever the pairing's (see "Blind to the withhold"
      below).

      A connection that closes mid-wait ends it at once, naming the close
      (the driver listens to `ConnectionClosed`). It never sits out the
      bound and then reports "no event".
    - **High-volume event:** obs-websocket sends it every 50 ms only to a
      session that asks for it. The cg OBS recorder asks with a `Reidentify`
      (All | `InputVolumeMeters` = 1 << 16) for the wait alone, and drops it
      after with an EXPLICIT `eventSubscriptions: All`, also when the wait
      fails: a `Reidentify` without the field KEEPS the current ones. A
      connect never asks for it. `e2e/obs-driver-protocol.spec.ts` pins
      both on a msgpack stub.
    - **Blind to the withhold (review round 2) — the meter sits BEFORE
      it.** obs-websocket's meter is an audio CAPTURE CALLBACK
      (`Obs_VolumeMeter.cpp`), and camera-box's libobs calls the callbacks
      for EVERY packet the source outputs: `source_signal_audio_data`, at
      the end of `source_output_audio_data` (camera-box
      `vendor/obs-studio/libobs/obs-source.c`). That includes the packets
      the pairing withholds from the mix (the `GENLOCK_AUDIO_ACT_WITHHOLD`
      branch just before it). camera-box's `genlock-audio-pairing.md`:
      "its packets never enter the mix (they still reach the audio
      callbacks/monitoring)". So the wait proves DistroAV delivers audio
      and adds its 1 s. It does NOT observe the pairing's lock. Its cover
      for the warm-up is the time it takes: the dev.17 take would have
      started ~1 s later, after its audio turned clean (0.461 s into it)
      but before LOCKED (37.214). A slower first lock (camera-box's
      withhold runs up to 10 s after the first packet) would reach the
      take again. Waiting on the pairing's own state is the open design
      question on #221. camera-box's documented channels:
      - the probe's `genlock-fifo audit` line in the OBS log
        (`audio_hold=pending`);
      - the `genlock-lock-json:` facet on `:8899/bundle-state.json`
        (per-input `locked`; on change and a ~30 s heartbeat).

      A vendor request `GetGenlockStats` was mentioned, but it is not in
      camera-box's tree (checked 6.10.2026).
    - **Limit:** obs-websocket also HOLDS a level until no audio has
      arrived for 0.3 s (`Obs_VolumeMeter.cpp` `GetMeterData`), so a gap
      in DistroAV's own delivery shorter than ~300 ms is invisible too.
      Never "fix" a take that still opens with dropouts by loosening the
      dropout check.
  - `afterAll` idles the probe FIRST (an idle probe shows nothing, so
    restoring the program to "OBS manuál" can never loop the picture through
    cg OBS), then restores the program scene through the facade (a manual
    scene is set on cg OBS there too), then cg OBS's own scene, guarded
    against a same-scene switch (cg OBS's 2 s self-fade, the #170
    dropped-event state).
  - cg OBS found ON the probe scene before the gate (a run that died
    mid-take, `cgStuckOnProbe`) fails the body at its start, naming it:
    there is no scene of the owner's to restore cg OBS to. `afterAll` still
    idles the probe in that case.
  - Before lane 3 the gate recorded cg OBS's sp-* input of the baseline
    playlist's own NDI output (lane 1's stopgap, deleted with those
    outputs).
  **`obs-driver.ts` keeps the BARE `obs-websocket-js` import (#221 L2b).**
  In Node it resolves (package `exports` → `import` / `require`) to the
  MSGPACK build, which offers only `obswebsocket.msgpack` — exactly what
  Companion (a Node module) speaks. The facade speaks msgpack since L2b and
  cg OBS always did, so both drivers use it. L3 briefly imported
  `obs-websocket-js/json` because the facade was then JSON-only (it answered
  msgpack with HTTP 400 and killed every post-deploy `beforeAll`, run
  36497336926). That workaround hid the same 400 from the E2E until the
  Companion cutover hit it (#221 comment 5881650057), so never reintroduce
  it: `e2e/obs-driver-protocol.spec.ts` pins the msgpack offer.
  It parks the program on the shared baseline scene (`e2e/obs-baseline-scene.ts`:
  sp-slow, never sp-warmup/sp-fast) through the facade. It proves the
  baseline playlist is PLAYING with `/api/v1/ndi/health`: `state=Playing`
  (= on program) AND `frames_submitted_last_5s > 0`.
  It sets the SONG faders to unity. Then, for every take, it waits for the
  probe's audio (above), reads the playing video, and runs `StartRecord` →
  20 s → `StopRecord`, which returns `outputPath`.
  - `startRecord` refuses to touch a recording the operator already started.
  - **Takes (max 3).** A take is repeated only in two cases:
    - `/api/v1/mix` shows a different `video_id` after the take, so the song
      changed mid-recording;
    - the result was `cannot_measure` with `unmeasurable_sides == ["video"]`
      (the picture alone: a still or overlaid video). Then the playlist is
      `/skip`ped to the next song first. The skip moves the playlist position
      and is not undone.

    A `fail` is never retaken. Neither is an audio-side `cannot_measure`,
    which can be a real audio fault. A retake starts only while less than
    110 s of the 320 s budget is used. The spec derives it:
    `RETAKE_BEFORE_MS` = `TEST_TIMEOUT_MS` − `WORST_TAKE_MS` − 10 s.
    - `WORST_TAKE_MS` is 200 s. It is summed from the take's own bounds,
      including the 20 s audio wait and copying the evidence.
    - The 10 s covers the calls the sum does not count: the audio wait's
      two `Reidentify` round trips, the StartRecord pre-check, the `/mix`
      and `/videos` reads, and spawning the analysis.
    - #221 dev.18 raised `TEST_TIMEOUT_MS` from 300 s to 320 s, by the
      audio wait's bound, so a run keeps the retake room it had before.
    The run is classified by `classifyAvSyncRun`: the stdout JSON and the exit
    code must agree. Missing JSON (a numpy import failure, an argparse error)
    is `error`, not a verdict.
  - It deletes every recording plus its auto-remux sibling from the OBS
    folder. When the profile's `Video/AutoRemux` is on, an mkv also leaves
    `<base>.mp4`.
  - **A take that did not pass keeps its evidence first.** That is every
    analysed take whose result is `fail`, `cannot_measure`, `error`, or an
    analysis that threw (`keepsEvidence`). A take discarded because the song
    changed keeps nothing. The recording, its remux sibling (copied only
    once its size stops growing), the analysis JSON (`av_sync.json`) and its
    stderr (or `analysis-error.txt`) are copied into the Playwright output
    dir as `take<N>-<name>` (`e2e/av-sync-evidence.ts`). It never throws: a
    failed copy is a `console.error`. A pass keeps no copy.
    - **Where it lands in CI:** the E2E job's "Upload post-deploy Playwright
      report on failure" step uploads `e2e/test-results/` as the
      `post-deploy-playwright-report` artifact (7 days). Open the run →
      Artifacts → `post-deploy-playwright-report` →
      `test-results/post-deploy-av-sync-…-chromium/av-sync-evidence/`
      (`take1-<date>.mkv`, `take1-<date>.mp4`, `take1-av_sync.json`,
      `take1-av_sync.stderr.txt`). The size per file is not measured yet
      (the design estimate is ~15 MB per failing run). Playwright wipes
      `test-results/` at the start of the next run, so nothing piles up on
      the box.
    - It is uploaded only when the JOB fails. A take that failed to measure
      and was then followed by a passing retake leaves the job green, and
      its copy is not uploaded.
    - The repo is PUBLIC, so any GitHub user can download the artifact for
      7 days. It holds 20 s of the OBS program picture + program audio,
      which includes any other source live in the program mix at that
      moment.
    `removeRecording` waits for the remux and retries while OBS still holds
    the file. An undeletable file fails the test after the verdict, never
    masking it.
  - `afterAll` first sets `tornDown`. Playwright does not cancel a
    timed-out body, so after that point the body refuses to start the
    probe audio wait, a recording, the recording wait, the analysis, or a
    skip.
  - It awaits an in-flight `StartRecord`, which is tracked as
    `startInFlight`. A start that resolved is ours, even if the body never
    got to set `recordingOurs`. A REJECTED start (an operator recording was
    running) is never stopped.
  - `ObsDriver.lastRecordingPath` keeps the file of a StopRecord whose
    inactive-poll timed out, so that file is deleted too.
  - `afterAll` is the safety net for a timed-out body. It has its own 180 s
    hook budget and works in this order:
    1. It kills a still-running analysis (a Windows `taskkill /T`, because
       python AND its ffmpeg children hold the file open).
    2. It settles an in-flight start, waiting at most 10 s.
    3. It stops our recording, only while `isRecording()`.
    4. It restores the faders, idles the probe, restores the program
       scene, then cg OBS's own scene (#221 B4 step 6, lane 3; the probe
       bullets above). This comes BEFORE the slow file deletion, so a hook
       that runs out of time never leaves the program on the baseline scene.
    5. It deletes recordings:
       - recordings the body never removed get a 15 s wait for their remux
         sibling;
       - recordings the body already removed get a 0 ms re-sweep, which
         catches a sibling that appeared late.

    Each step runs in its own try/catch, and the errors are asserted together
    at the end.
- **Original sidecars:** `/api/v1/playlists/{id}/videos` has NO `file_path`.
  The pair is resolved from the cache listing by
  `*_{youtube_id}_normalized[_gf]_{video.mp4|audio.flac}`
  (`resolveSidecars`). Exactly one complete pair must match, otherwise the gate
  throws.
- **Analysis:** `scripts/av_sync_check.py` runs under the gate's OWN venv
  (`SP_AVSYNC_PYTHON` = `C:\ProgramData\SongPlayer\e2e\avsync_venv\Scripts\python.exe`)
  with the bundled ffmpeg (`SP_FFMPEG`). Only numpy is needed. #221: never
  the lyrics venv — SongPlayer may reinstall its packages at startup, and a
  numpy replaced mid-analysis failed CI run 36475215084 (`No module named
  'numpy.fft'`). The E2E step "Prepare the A/V gate's own Python (#221)"
  creates the venv once and verifies it every run (numpy pinned to the
  Eval Checks version, 2.1.1; `av_sync_check.py --help` must start and
  `import numpy, numpy.fft` must succeed: numpy 2 loads `numpy.fft` lazily
  and the scripts use it only inside functions, so `--help` alone never
  imports it; `C:\Program Files\Python312` is required, no PATH fallback). A new
  import in `av_sync_check.py` / `av_sync_drift.py` / `av_sync_warp.py`
  must be added to that step AND to the Eval Checks pip line. The box has
  no ffprobe, so stream start times and frame sizes come from ffmpeg's own
  `showinfo`/`ashowinfo` pts.
  - **audio:** 8 kHz mono FFT cross-correlation, normalized by local energy,
    searched over the whole song. corr must be ≥ 0.9.
  - **video:** 64-wide gray frames, cropped to the content box computed from
    the source aspect (1920×960 in a 1080 canvas → rows 2:34 of 36). The
    offset is a GLOBAL alignment: the shift (1 ms grid, ±1 s around the audio
    offset, clamped to the decoded video span) that maximizes the mean
    per-frame score, with sample-and-hold frame timing.
    - The median match must be ≥ 0.95, and the contrast (peak minus the
      curve's median) must be ≥ 0.002.
    - Do NOT "simplify" back to a per-frame argmax median. On static or
      lyric-video frames every candidate ties at ~1.0, and those frames score
      highest, so the "above-median" filter keeps them. The median then
      collapses toward the window centre (the audio offset, A/V ≈ 0 →
      false PASS) or toward the window edge. The pytest
      `test_mostly_static_lyric_video_still_measures_the_true_offset` pins
      this.
  - **dropouts:** a **10 ms RMS window sliding at 1 ms**. A window is a
    dropout when rec RMS < 15 % of `level` × the original's RMS while the
    original is loud. Overlapping windows, and runs less than 50 ms apart,
    merge into one event.
    - **Detection floor: 11 ms.** Any gap of at least window + hop contains
      a whole window at any phase.
    - Why not blocks: the manual method used 50 ms blocks, and a
      grid-aligned block misses a lost 10–50 ms NDI buffer. At 50 ms a 40 ms
      silence read `pass`; at a fixed 10 ms grid a 12 ms gap was missed 14
      times in 20. Both are review findings.
    - `level` = `max(|LS gain|, median rec/orig RMS ratio over loud
      windows)`. The RMS ratio is immune to a sub-sample lag, which shrinks
      the phase-coherent LS gain and would blind the detector.
    - "Loud" = above `max(0.1 × median window RMS, −45 dBFS)`: within 20 dB
      of the take's level and above near-silence.
      - The window counts only if the original is that loud in EVERY 2 ms of
        it (a minimum sub-window RMS). A window that clips the edge of a hard
        onset is never judged against a recording one sample late (review:
        66 false events without this).
      - No percentile term: `min(p20, …)` skipped an audible passage 12 dB
        under the take's level (review finding). A lost buffer there must
        still fail, even if an OBS gate caused it.
      - The absolute floor (the sidecars are −14 LUFS) keeps rests, fades
        and what an encoder rounds to zero out.
    - Glitch statistics (relative error > 0.8) stay on 50 ms blocks, and
      blocks holding a dropout are excluded.
    - The first/last 100 ms are not classified. Older ffmpeg (6.1) decodes
      the AAC priming of an mkv (`start_time` −0.021 s) as silence at sample
      0.
    - Output: `dropouts.dropout_count`, `dropout_ms`, and `dropout_events`
      (a list of `{start_s, ms}`).
  - **verdict order:** each result is trusted only as far as its own side was
    measurable.
    1. Audio unmeasurable → `cannot_measure` (sides `["audio", …]`).
    2. Any dropout → `fail`, even when the picture is unmeasurable. A still
       or overlaid picture must never turn a lost buffer into a retake.
    3. Picture unmeasurable → `cannot_measure` (sides `["video"]`); A/V is
       not judged.
       - An EXCEPTION in the picture step (probe, decode, crop, no-shift)
         is side `["video_error"]` with the error as the only reason and
         `av_ms: null`. Dropouts already found still decide step 2.
       - A `video_error` is deterministic (a bug), so it is NEVER retaken.
         Only `["video"]` is retaken.
       - An audio-step exception is `cannot_measure` with sides
         `["error"]`, and is never retaken.
    4. |A/V| > 40 → `fail`, else `pass`.
  - **letterbox crop:** the original is cropped (`source_crop`) to exactly
    the grid cells the recording keeps before scaling. A letterbox edge
    inside a cell would otherwise skew the geometry by up to one cell.
- **ffmpeg window decode: `-copyts` with `-ss`/`-t` as INPUT options
  (before `-i`).**
  - With an OUTPUT `-t` under `-copyts`, the duration counts from 0, not from
    the seek point. In the first local run that cut the window to 109 of ~570
    frames.
  - It also drops frames that `showinfo` had already logged, so the pts count
    no longer matched the frames (114 vs 109).
  - Frame times always come from `showinfo`/`ashowinfo` `pts_time:`, and the
    script checks that the frame count equals the pts count.
  - Keep `-fps_mode passthrough` so rawvideo never duplicates or drops frames.
- **Never hardcode the AAC priming subtraction.** ffmpeg 6.1 OUTPUTS the
  priming samples: sample 0 is at −0.021 s. The BtbN master build the box
  downloads (`tools.rs`, checked 24.9.2026) SKIPS them: sample 0 is at 0.000.
  The script reads sample 0's time from `ashowinfo`, so both measure right.
  A fixed "subtract start_time" would be off by 21 ms on the box.
- **Verdict / exit:**
  - `pass` (0): |A/V| ≤ 40 ms and 0 dropouts.
  - `fail` (1): |A/V| > 40 ms or any dropout.
  - `cannot_measure` (2): low corr, match or contrast, or an analysis error.
    It FAILS the job and is never a skip.
- **Reading the output:** the CI log shows the full JSON and one line:
  `AV-SYNC status=… av_ms=… audio_corr=… rate_ppm=… fit_residual_ms=…
  warped=… video_match=… video_contrast=…
  dropouts=… dropout_ms=… glitches=… drift_ms_per_10s=… max_step_ms=…
  step_at_s=… outlier_steps=… windows=<used>/<total> reasons=[…]`.
  - `av_ms` > 0 means audio AHEAD of picture. `audio.offset_s` and
    `video.offset_s` are `orig_time − rec_time`.
  - `video.plateau_ms` is the video's resolution. With continuous motion it
    is up to one source-frame period when source and recording frame rates
    are equal. With sparse cuts it is up to one period of the coarser clock
    (33 ms at 30 fps). So ~±17 ms of the measured A/V is quantization.
  - `dropouts.dropout_events[].start_s` and `glitch_times_s` are recording
    times. Glitches (relative error > 0.8, not silent) are informational only.
  - `audio.second_corr` is the best audio match more than 0.5 s away from
    the peak. A repeated chorus can come close to `corr`. A wrong peak then
    shows up as a low video match (`cannot_measure`), never as a false pass.
  - Baseline on 24.9.2026 (manual): A/V +13 ms, corr 0.997, match 0.999,
    0 dropouts.
- **Clock-rate drift is COMPENSATED before the audio verdict (#147,
  `av_sync_warp.py`).** SongPlayer's genlocked output and the OBS audio clock
  that records the program run at slightly different rates. Push run
  36081072389 recorded the ORIGINAL with a perfectly smooth +16.8 ppm walk:
  −3.35 → +12.43 samples at 48 kHz over 19.6 s, local corr 0.985–0.999, no
  steps. Correlated at ONE lag, those 0.33 ms decorrelate everything above
  ~1 kHz at SR 8 kHz: corr 0.84 → `cannot_measure`, and the fixed-lag scan
  reads 89 "glitches". The playback was fine. So `analyze_audio` works like
  this:
  - It keeps the global lag.
  - It fits `deviation(i) = a + b·i` over 0.25 s windows. Each window gets a
    normalized xcorr searched ±50 ms, and the peak is refined by sinc
    interpolation to 1/64 sample.
  - When |rate| ≤ 200 ppm AND the max window residual ≤ 0.5 ms, it resamples
    the original onto the recording with a Blackman-windowed sinc. It never
    uses `np.interp`: linear interpolation attenuates the band the gate
    checks.
  - corr, the dropout scan and the glitch scan then run against that WARPED
    original. `audio.offset_s` (and so `av_ms`) is the fitted offset at the
    recording's MIDPOINT, which matches the picture's global alignment.
  - A step/slip (it cannot fit a line), > 200 ppm, or < 8 measurable windows
    stays UNWARPED. The old one-lag analysis then decides, and
    `rate_ppm`/`fit_residual_ms` are reported as the fault sign. A real
    resync still fails.
  - JSON: `audio.rate_ppm`, `fit_residual_ms`, `fit_windows`, `warped` and
    `corr_unwarped`.
  - Offline on the evidence take: corr 0.833 → 0.996, glitches 99 → 0, rate
    +16.68 ppm, residual 0.007 ms.
  - Thresholds are UNCHANGED: 0.9, 0.8, the dropout rules and 40 ms.
  - Tests: `test_av_sync_warp.py`. It uses an ANALYTIC tone-sum original, so
    a drifted recording is exact at every sample and independent of the warp
    under test. Never build a drift fixture with the warp itself.
- **Per-segment profile — drift vs jump (diagnostics only, #147).** One
  whole-take A/V number cannot tell a clock that drifts from a resync that
  jumps. Release run 36068121677 read av_ms 60.5, corr 0.899 and glitches
  only from 12.5 s. The JSON therefore carries two extra fields:
  - `segments` (`segment_profile` in `av_sync_check.py`): one row per 2 s
    window of the recording. Each row has:
    - `t_s` / `t_end_s`: the window, in recording time;
    - `audio_offset_s` + `audio_corr`: that window's audio cross-correlated
      against the original, searched ±300 ms around the global audio offset;
    - `video_offset_s` + `video_match` + `video_contrast` +
      `video_plateau_ms`: the same global video alignment, restricted to the
      window's frames, ±300 ms around the global video offset;
    - `video_ok`: contrast ≥ 0.002 AND a plateau no wider than 1.5 frames
      of the COARSER frame clock (source or recording). A 60 fps source
      recorded at 30 fps resolves only 33 ms, so the limit is 50 ms, not
      25 ms. A still or nearly static shot fails it: with a flat curve, or
      a wide plateau whose edges drop only at the search limits (e.g.
      233 ms), the window's video offset is just the plateau centre. That
      can be tens of ms off, so its `av_ms` is not a measurement;
    - `av_ms` and `errors` (why a side is `null` there: silence, no frames,
      no shift).
  - `drift` (`drift_and_step` in `av_sync_drift.py`): fitted over the GOOD
    windows (`windows_used` of `windows_total`). A good window has
    `audio_corr ≥ 0.8` and `video_ok`.
    - `drift_ms_per_10s` is the least-squares slope of `av_ms`.
    - `max_step_ms` is the largest `av_ms` change between neighbouring good
      windows. `step_gap_s` is `[end of the earlier, start of the later]`
      window, and `step_at_s` its middle.
      - A zero-width gap means the jump is on that window boundary. A jump
        inside a window lands on one side of it.
      - A gap one window wide means the straddling window was excluded.
      - A wider gap means several windows were excluded: the jump is
        somewhere inside the gap, not necessarily at its middle.
    - `outlier_steps` counts the LASTING outlier steps. An outlier step is
      ≥ 20 ms and ≥ 3× the median step. It is lasting unless an adjacent step
      undoes it (their sum is < half of it): the two edges of a one-window
      spike cancel.
    - The fit gets its own step term there (`step_modeled: true`) only when
      that step is the ONE lasting outlier and each side keeps ≥ 2 good
      windows. The fit is `level + slope·t + step·[t ≥ t_k+1]`, with the
      slope fitted jointly. Results:
      - A drift followed by ONE resync reads its own slope plus the step,
        e.g. −5 ms/window with a pull-back reads −25 ms/10 s. This is the
        typical fault shape, a one-cycle sawtooth.
      - A jump next to one bad window is still a jump.
      - A steady drift (equal steps) and a one-window spike, anywhere
        (including next to an end window), are never split.
      - **`outlier_steps ≥ 2`** (two resyncs: out and back, same direction,
        or periodic) means no step is modelled, and the slope is NOT a clock
        drift. Out and back reads ≈ 0, and two same-direction jumps read
        ~+80 ms/10 s. Read the per-window `av_ms`.
    - A jump in the first or last good window cannot be modelled. It shows
      as `max_step_ms`, and it bends the slope.
    - A resync close to the 20 ms floor reads bimodally under per-window
      noise. With ±5–7 ms, a −5 ms/window drift plus a 30 ms pull-back
      (a raw step of 25 ms) is modelled in ~75–85 % of takes. Otherwise it
      reads ≈ −4 instead of −25 ms/10 s. Near the floor, read the rows.
    - `coverage_low: true` means fewer than 70 % of the windows are good.
      The fit then describes only part of the take.
  - How to read it:
    - **Steady drift** (an unsynchronized clock, #148 / #55): a slope, small
      equal steps, `step_modeled: false`. 50 ms over 20 s reads ≈ 25 ms/10 s.
    - **Jump / resync:** `step_modeled: true`, `max_step_ms` ≈ the jump at
      `step_at_s`, slope ≈ 0.
    - **Constant offset** (e.g. a fixed latency): slope ≈ 0, no step, every
      window's `av_ms` ≈ the global one.
    - **A window with low `audio_corr`** is either where the audio itself
      changed (glitches, a gap: compare its `t_s` with `glitch_times_s`) or
      a drift fast enough to smear the window (next point). Its raw
      `av_ms` is still in `segments`: when `coverage_low` is set or the low
      windows sit at one end, read their `av_ms` trend. The fit leaves them
      out, so a drift that STARTS mid-take can read "no drift, no step" over
      the good windows alone.
  - Per-window `av_ms` jitters by a few ms when the picture has few cuts:
    a 2 s window has far fewer frames than the whole take. Read a trend
    over several windows, not one window.
  - **Limit, measured on the pytest fixture (25.9.2026):** a drift smears
    the alignment inside ONE 2 s window by ±(ppm × 1 µs). How much smear
    the per-window correlation survives depends on how high the audio's
    energy goes. Values are median window corr and good windows of 10:
    - white noise up to 4 kHz: 50 ppm → 0.83 (6), 100 ppm → 0.62 (0);
    - noise up to 2 kHz: 100 ppm → 0.86 (10), 200 ppm → 0.68 (0);
    - noise up to 1 kHz: 200 ppm → 0.90 (10), 300 ppm → 0.82 (8),
      500 ppm → 0.61 (0);
    - noise up to 500 Hz: 500 ppm → 0.87 (10).

    Whenever windows are used, the slope is right: e.g. 1 kHz at 300 ppm
    reads 3.0 ms/10 s. The pytest
    `test_a_realistic_clock_drift_on_music_band_audio` pins 1 kHz at
    100 ppm.

    Music at 8 kHz carries most of its energy below 1 kHz. A clock error of
    a few hundred ppm can still push bright material under 0.8. `drift` then
    stays `null` (or `coverage_low`) instead of fitting a slope through
    windows that do not match. Read the raw per-window `av_ms` then.
  - The verdict and its thresholds do not read these fields. A profile
    error is reported as `drift.error` and on stderr, and never moves the
    verdict.
- **Tested:** the pure functions are covered by pytest on synthetic click-train
  plus flash-frame fixtures in Eval Checks (numpy only).
  - The segment profile (`test_av_sync_profile.py`) uses bass-band noise and
    a moving picture. It covers:
    - drift, a jump, in-sync, and a realistic 100 ppm drift on 1 kHz audio;
    - drift + resync, two resyncs (`outlier_steps` 2), spikes, and the
      end-window guards;
    - an excluded low-corr window, and coverage;
    - still and nearly static windows (not `video_ok`);
    - 60 fps (doubled and real) and 24 fps sources recorded at 30 fps
      (`video_ok`).
  - The evidence copy (`av-sync-evidence.spec.ts`) runs on a temp dir in the
    mock suite.
  - `scripts/tests/conftest.py` puts `scripts/` on `sys.path`, as on the
    box, so a script's sibling import (`av_sync_check` → `av_sync_drift`)
    resolves when a test loads the script by file path.
  - The ffmpeg I/O layer is untested in CI by design, because that job has no
    ffmpeg. It was checked locally against ffmpeg-muxed mkv and mp4 AAC
    "recordings" with known offsets (+120 → 116, 0 → −3/−4, −80 → −83 ms,
    and an 85 ms zeroed stretch → a dropout), with both ffmpeg 6.1 and the
    BtbN master build.
  - The real ffmpeg path of the profile was checked locally (ffmpeg 6.1,
    25.9.2026). The original was a testsrc2 1920×960 at 25 fps plus pink
    noise low-passed to 1.5 kHz. The recording was an mkv letterboxed to
    1080 at 30 fps, AAC, whose audio jumps +60 ms at rec 12 s. The result:
    - windows 1–6 read −17.0 ms and 7–10 read +43.0 ms;
    - every window had corr ≥ 0.98 and a 6 ms video plateau;
    - `max_step_ms=60.0 step_at_s=11.979 step_modeled=true`, drift 0.0;
    - the whole-take verdict was `cannot_measure` (corr 0.596), as on the
      release run.
- **Blind spot:** the reference is the sidecar pair itself. An offset baked
  into the sidecars at download/normalize time is invisible to this gate.
- **Unverified until the first box run:** the thresholds (0.95 match, 0.002
  contrast) have not been measured with the global method on a real OBS
  recording. Static overlays in the sp-slow scene (title text, logos) lower
  the match. Read the first run's `AV-SYNC` line before trusting a red or a
  green.
- **Do not "fix" a red gate** by raising 40 ms, lowering 0.9/0.95, or skipping
  on exit 2. Find what moved the audio or the picture.
