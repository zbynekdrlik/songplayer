---
paths:
  - "crates/sp-server/src/obs/**"
  - "crates/sp-server/src/playback/ndi_health.rs"
  - "e2e/post-deploy.spec.ts"
  - "e2e/ndi-health-gate.ts"
---

# OBS ↔ NDI health & receiver recovery (#127)

## The dark-wall failure shape (the worst this project has)
A SongPlayer restart tears down the NDI sender endpoint OBS's DistroAV receiver was bound to; DistroAV does **not** re-run discovery on its own, so the video plate is black while `ndi_health.rs` still reports `state=Playing`, `connections=0`. Every dashboard/health-check/CI gate can pass green while the wall is dark. `compute_degraded_reason` already computes `degraded_reason = "no NDI receiver — wall is dark"` (const `DARK_WALL_REASON`).

## Recovering a stranded receiver — clear + restore, NEVER RecreateSender
- The nudge is **receiver-side over the OBS WebSocket**: clear the NDI input's `ndi_source_name` to `""`, then restore the original — this forces DistroAV to re-run discovery. Re-applying the **identical** value is a **no-op** (DistroAV won't restart the receiver when nothing changed — proven on #127, `consecutive_bad_polls` kept climbing). `obs/ndi_recovery.rs` (pure trigger policy + `NdiRecoveryTracker`) → `ObsCommand::NudgeNdiReceiver` → `obs/ndi_discovery.rs::reapply_ndi_input`.
- **NEVER** a per-sender `PipelineCommand::RecreateSender` — structurally cannot fix a receiver-side binding (CLAUDE.md "Disabled subsystems", #60).
- To go from a health snapshot's bare `ndi_name` (e.g. `"SP-slow"`) to the OBS input (`sp-slow_video`), enumerate NDI inputs (`fetch_ndi_input_names`) and match `extract_ndi_stream_name(ndi_source_name) == ndi_name` — the machine-prefix split (`"MACHINE (stream)"`).

## The stored `ndi_source_name` host case MUST equal the advertised name (#173)
NDI advertises each SongPlayer sender as `"<HOST> (<stream>)"` where `<HOST>` is
the box's computer name **as the NDI runtime announces it** = Windows
`COMPUTERNAME` (`RESOLUME-SNV` on win-resolume), NOT `gethostname()`/`hostname`
(lowercase `resolume-snv`). DistroAV's receiver re-match after a sender
re-announce (every SongPlayer restart) is **case-sensitive**, so an OBS input
whose stored `ndi_source_name` is the lowercase form never re-attaches → 0
receivers while on program = dark wall, even though `extract_ndi_stream_name`
matches by stream and the map looks fine.

- **Code guard:** `obs/ndi_discovery.rs::canonical_sender_name(stored, advertised)`
  returns `Some(advertised)` iff the stored value differs from
  `"<COMPUTERNAME> (<stream>)"` in ASCII case ALONE. `rebuild_ndi_source_map`
  (connect + rebuild signal) and the #127 nudge `reapply_ndi_input` both call it
  and rewrite via `SetInputSettings`, logging INFO
  `ndi: normalized input '<name>' sender host case → 'RESOLUME-SNV (SP-x)'`. The
  nudge now restores the ADVERTISED name, never the stored lowercase one.
  `advertised_ndi_host()` reads `COMPUTERNAME`; unset (Linux/CI) → normalization
  is skipped (never a wrong-case rewrite). The rewrite can only change host
  CASE toward the verified-correct advertised host, never rename to a different
  sender.
- **Manual remedy (over obs-websocket / MCP):** set the input's `ndi_source_name`
  to the uppercase advertised host —
  `mcp__obs-resolume__obs-set-input-settings <input>_video {"ndi_source_name":"RESOLUME-SNV (SP-x)"}`.
  Confirm the advertised host with `$env:COMPUTERNAME` on the box (NOT `hostname`).
- **Flap escalation (#173):** `obs/ndi_recovery.rs` `NdiRecoveryTracker` counts
  recover→re-dark-within-`FLAP_WINDOW_100NS` (30 s) flaps; after
  `FLAP_ESCALATE_COUNT` (2) it forces a `ClearRestore` once (bypassing the
  below-threshold skip) so the normalizing re-apply lands promptly, and logs
  `ndi-recovery: receiver flapping ... escalating to a clear+restore`.

## Escalation ladder — recover a WEDGED DistroAV receiver (#173 round 2)
Clear+restore fixes a mis-named / unmatched source, but a receiver **wedged**
inside DistroAV after the sender was recreated several times (repeated SongPlayer
restarts) **ignores it** — box 17.9.2026: on-program `SP-fast` (uppercase, healthy
name) stayed `connections=0` for ~20 min through three `outcome=Applied`
clear+restore nudges, while `SP-warmup` on the SAME sender process had 6 receivers
(per-input wedge, camera-box#1096 class; NOT #60 sender/mDNS). So a sustained dark
wall now ESCALATES a ladder (`obs/ndi_recovery.rs::next_step`, pure + unit-tested;
executor `obs/ndi_recovery_io.rs`, I/O over the healthy OBS WebSocket):

- **Rung 0 `ClearRestore`** — fires at the dark threshold (`NUDGE_THRESHOLD_BAD_POLLS`
  = 6 polls ≈ 30 s): clear + restore `ndi_source_name` to the ADVERTISED name.
- **Rung 1 `ToggleSceneItem`** — `LADDER_STEP_SPACING_POLLS` (2 polls ≈ 10 s) later:
  `SetSceneItemEnabled` OFF→ON so DistroAV tears down + recreates the receiver.
  **Round 3: read `GetSceneItemEnabled` back after the OFF→ON.** The two acks do
  NOT prove the item is visible — an operator who hid the source on program
  leaves it disabled, and the toggle "applied" while the wall stayed dark. If the
  read-back is `false`, set it ON again and log
  `ndi-recovery: rung 1 — item was disabled, re-enabled`. The ladder must never
  leave an on-program item hidden.
- **Rung 2 `RecreateInput`** — 2 more polls later, **RENAME-FIRST (round 3),
  NEVER remove-then-reuse-a-name.** `SetInputName` the OLD input to a unique temp
  (`<input>__recover_<uuid8>`) — a SYNCHRONOUS rename that frees the original name
  — then `CreateInput` the replacement DIRECTLY under the original name (same
  scene, identical `inputKind` + `inputSettings`, advertised name), PROVE it
  exists (`CreateInput` returned a `sceneItemId` AND `GetSceneItemList` lists it),
  restore the saved transform + z-order, THEN `RemoveInput` the renamed-away old.
  On any pre-remove failure the old content is renamed back to the original name —
  the scene is never emptied. At the START of the attempt any leftover
  `<input>__recover_*` temps from an earlier interrupted recreate are swept
  best-effort (`fetch_ndi_input_names` → `is_stale_recover_input` → `RemoveInput`,
  counted in a WARN) — their unique-per-attempt names are never reused, so their
  async teardown is harmless. The gate `LADDER_RECREATE_ENABLED`
  (`obs/ndi_recovery.rs`) is a real switch: `false` makes the ladder cool down at
  rung 1 instead. The pure step list `recreate_plan()` (`obs/ndi_recovery_io.rs`)
  is unit-tested for BOTH invariants: never remove before verify, and never reuse
  a name freed by a remove.
- **Cool-down** `LADDER_COOLDOWN_POLLS` (6 polls ≈ 30 s) after the recreate, then
  the ladder restarts at rung 0. One action per rung per poll; the ladder resets
  the moment the receiver re-attaches.

The fired rung is surfaced on `/api/v1/ndi/health` as `recovery_step`
(`ClearRestore` / `ToggleSceneItem` / `RecreateInput` / `null`) so the E2E / log
can see which rung recovered a wall. NEVER a per-sender `RecreateSender` (#60).

- **Manual equivalents (over obs-websocket / MCP), same order:**
  1. clear+restore — `mcp__obs-resolume__obs-set-input-settings <input>_video {"ndi_source_name":""}` then the advertised `RESOLUME-SNV (SP-x)`.
  2. toggle — `mcp__obs-resolume__obs-set-scene-item-enabled` (sceneName + sceneItemId, `false` then `true`; find the id with `obs-get-scene-items`), then `obs-get-scene-items` again to confirm `sceneItemEnabled: true`.
  3. recreate (rename-first) — `obs-get-input-settings` (capture `inputKind` + `inputSettings`) → `obs-set-input-name` the OLD input → `<input>__recover_x` (frees the original name) → `obs-create-input` under the ORIGINAL name (same scene, inputKind `ndi_source`, advertised `ndi_source_name`) → confirm it lists → `obs-set-scene-item-transform` + index → `obs-remove-input` the renamed-away old. NEVER create/rename INTO a name you just removed (601 async-teardown race). Or just fire the app path: `POST /api/v1/ndi/recover/{playlist_id}?step=recreate`.
  Always end with OBS on `sp-fast`, engine `[7]`, `SP-fast Playing` with receivers.

- **Inactive-output caveat:** `connections=0` on an INACTIVE output is NORMAL
  (`ndi_behavior 0`, 1 s timeout — DistroAV drops an off-program source); only an
  ON-PROGRAM output with `connections=0` is a dark wall worth a ladder rung (see
  the dedicated section below — `handle_health_snapshot` maps Playing+inactive →
  Paused so `is_dark` never fires off-program).

- **Ladder limits — when receiver-side recovery CANNOT clear it (box 17.9.2026,
  #173 round 5).** The whole ladder is receiver-side; a receiver that stays
  `connections=0` through many `outcome=Applied` clear+restore nudges AND
  `recreate` rungs AND a manual clear+restore with a long (~12 s) clear-hold is
  **wedged deeper than any receiver-side action can reach** — only a SongPlayer
  process restart (fresh NDI runtime) clears it (round 4 saw a restart bring dark
  `SP-fast` back to `connections=2`; SongPlayer must NOT be force-restarted outside
  a deploy). **Diagnostic: count the OTHER outputs.** If 8-of-9 senders from the
  SAME SongPlayer process have receivers (`SP-presence`/`SP-worship`/… `connections
  ≥ 2`) and only ONE on-program output is hard-`0`, it is a **per-input receiver
  wedge**, NOT a process-global mDNS failure (#60) — do not chase it receiver-side;
  a redeploy/restart is the fix. A per-restart-intermittent dark `SP-fast` right
  after a deploy is this class (the E2E suite hammering `sp-fast`'s ladder just
  after the restart can deepen the wedge); re-verify `SP-fast connections ≥ 1`
  after the NEXT deploy rather than burning the lane on receiver-side attempts.

## obs-websocket 5.x write-path gotchas (recreate/toggle a scene item, #173)
When recreating or re-transforming an NDI input over obs-websocket 5.x
(`obs/ndi_recovery_io.rs`):
- **There is no "which scenes contain source X" request.** Resolve an input's
  scene + `sceneItemId` + `sceneItemIndex` by scanning `GetSceneList` →
  `GetSceneItemList` per scene and matching `sourceName`. `GetSceneItemList`
  already embeds `sceneItemId`, `sceneItemIndex` AND `sceneItemTransform`.
- **`GetInputSettings` returns both `inputSettings` AND `inputKind`** — capture
  both so a `CreateInput` recreate is byte-identical (kind `ndi_source`).
- **`CreateInput` adds the scene item at the TOP of the scene** (highest index)
  and returns a NEW `sceneItemId`. Restore the original z-order with
  `SetSceneItemIndex` and the transform with `SetSceneItemTransform`.
- **A round-tripped `sceneItemTransform` carries read-only/derived fields**
  (`width`, `height`, `sourceWidth`, `sourceHeight`) that OBS computes from the
  scale/source and REJECTS as out-of-range on `SetSceneItemTransform` — STRIP
  them before writing (keep position/scale/rotation/crop/bounds/alignment).
- **`RemoveInput` deletes the input and EVERY scene item referencing it** across
  all scenes; our `sp-*_video` inputs each live in exactly one scene, so the
  single-scene recreate is safe. Recreate briefly blacks that scene (~sub-second)
  — acceptable only because the ladder fires when the wall is ALREADY dark.
- **Round 5 — a bare `RemoveInput` of a RECEIVING DistroAV `ndi_source` reports
  success but does NOTHING.** The libobs source destroy blocks on the receiver
  thread, which never joins while the sender is up, so the input + its scene item
  LINGER as an operator-visible duplicate (box 17.9.2026 round 4:
  `sp-youth_video__recover_f0831b7d` stayed listed 2.75+ min after a "successful"
  `RemoveInput`; a second manual `RemoveInput` also "succeeded" without effect).
  **The fix (`obs/ndi_remove.rs::remove_ndi_input_hard`):** `SetInputSettings`
  clear `ndi_source_name` to `""` (overlay=true — DistroAV stops the receiver on
  an empty source) → THEN `RemoveInput` → **READ BACK** `GetInputList` +
  `GetSceneItemList` → still listed and a scene-item id is known →
  `RemoveSceneItem(scene, id)` (a rename keeps the item id) → read back once more
  → still present → loud WARN with both listings. **Never trust the `RemoveInput`
  response code — read the removal back.** Both rung-2 removal call-sites (the
  recreate's `RemoveRenamedOld` and the start-of-attempt stale-`__recover_*`
  sweep) route through this hard remove, so rung 2 ends with exactly ONE input.
  **Manual equivalent:** `obs-set-input-settings <temp> {"ndi_source_name":""}`
  THEN `obs-remove-input <temp>` (+ `obs-remove-scene-item` if the item lingers).
- **Round 3 — RENAME-FIRST, because `RemoveInput` frees the name ASYNCHRONOUSLY.**
  DistroAV tears an `ndi_source` down on its own thread, so the OBS input NAME is
  NOT free the instant `RemoveInput` returns — reusing that name immediately
  (create OR `SetInputName`) races the teardown and returns obs-websocket
  **`601 "a source already exists by that new input name"`**. Two box incidents:
  (1) round-2 remove-then-create left the scene EMPTY when the create lost the
  race; (2) the round-3 create-temp-then-**rename-back-to-original** left every
  recreate named `<input>_recover` when the RENAME lost the SAME race
  (17.9.2026). The fix is to never reuse a name freed by a remove: **rename the
  old input away (a synchronous rename frees the original name at once), create
  the replacement DIRECTLY under the original name, then remove the renamed-away
  old** (its temp name is never reused, so its async teardown is harmless). And
  ALWAYS log the full obs-websocket error on a failed write —
  `d.requestStatus.code` + `d.requestStatus.comment` + the step name
  (`log_obs_failure` / `send_ok_logged`); the round-2 executor swallowed the
  CreateInput error, so the cause was unknown for a whole cycle.

## `connections=0` on an INACTIVE output is NORMAL (not a dark wall)
The `sp-*` NDI inputs run `ndi_behavior 0` with a 1 s `ndi_behavior_timeout`, so
DistroAV **disconnects an inactive source** — an output that is NOT on OBS
program legitimately reports `connections=0`. Only an **on-program** output with
`connections=0` is the dark-wall failure. This is exactly why
`handle_health_snapshot` maps `Playing + scene_inactive → Paused` (so
`compute_degraded_reason` returns `None`) and why E2E test 12 cross-references
`active_playlist_ids`: do NOT read a bare `connections=0` on an off-program
output as a fault.

## Adding recovery/health state without touching `playback/mod.rs`
`handle_health_snapshot` runs on the engine but the engine struct lives in `playback/mod.rs` (often owned by a parallel lane). Compose new per-pipeline state into `NdiHealthRegistry` (the `Arc` the engine already holds) instead of adding a `PlaybackEngine` field — the engine reaches it via `self.ndi_health_registry.<method>()`. `handle_health_snapshot` is sync + `mutants::skip`; send `ObsCommand` with `try_send` (channel cap 64).

## Mutation gate: `obs/**` is EXCLUDED
`ci.yml` runs `cargo mutants --in-diff` with `--exclude-re 'sp-server/src/obs/'` — pure logic in `obs/` is NOT mutation-scored (still unit-test it, but survivors there won't fail CI). Code in `playback/ndi_health.rs` **is** scored: every new non-`mutants::skip` fn there needs tests that kill its true/false mutants (e.g. `evaluate_recovery` is killed by a nudge-fires + a nudge-does-not-fire engine test).

## E2E dark-wall gate (post-deploy suite)
Select the on-program output from `GET /api/v1/status` → `active_playlist_ids`, cross-reference `GET /api/v1/ndi/health` by `playlist_id`. `connections`: `>0` live, `0` dark (#127), `-1` never-polled-yet (keep polling). Pure decision logic lives in `e2e/ndi-health-gate.ts` (unit-tested by `ndi-health-gate.spec.ts` in the ubuntu **mock** suite — a `test()` that never touches `page` runs with no browser/box). Keep the baseline-scene discipline (CLAUDE.md "E2E must not switch to disruptive OBS scenes").

## Gotcha: `e2e/post-deploy-report/index.html` is a TRACKED artifact
Playwright runs regenerate it; it shows up as ` M` in `git status`. `git checkout -- e2e/post-deploy-report/index.html` before committing so it never lands in your diff.

## Studio Mode can DROP `CurrentProgramSceneChanged` — the ~2 s poll reconciles it (#170)

OBS on win-resolume runs Studio Mode with a 2 s Fade. From a `preview == program`
state (e.g. right after a same-scene transition) OBS **drops** the next program
switch's `CurrentProgramSceneChanged` — `GetCurrentProgramScene` reports the new
scene but no event fires, so the event-only path never learns of it and the wall
sits on a paused source (a dark wall in daily operation, reproduced live 3×). The
connection loop therefore ALSO polls `GetCurrentProgramScene` every
`SCENE_POLL_INTERVAL` (~2 s) in `connect_and_run`'s `tokio::select!`
(`obs/scene_poll.rs::reconcile_program_scene`); on a mismatch with the last
event-derived `ObsState::current_scene`
(`scene_poll::scene_poll_detects_change(last, polled) -> Option<scene>`) it feeds
the SAME `scene::apply_scene_change` path the event does — so the reader arm and
the poll arm share one scene-apply body (keeps `obs/mod.rs` ≤1000). A duplicate
same-scene emit is harmless: `(Playing, SceneOn)` is a no-op in `state.rs`. The
pure `scene_poll_detects_change` is Linux-unit-tested; `reconcile_program_scene`
(I/O) is not (`obs/**` is excluded from the mutation gate). INFO log on a
poll-caught switch: `obs: program scene changed without an event — reconciled by
poll`.

## Reading the health snapshot's `state` — `Playing` already means "on program" (#154)
`handle_health_snapshot` RECONCILES the pipeline-reported state before storing it: a pipeline that is `Playing` but whose scene is NOT on OBS program (`scene_active == false`) is stored as `Paused`, not `Playing`. So a consumer that reads `NdiHealthRegistry::snapshots()` and checks `state == PlaybackStateLabel::Playing` is already getting "an output is playing AND OBS is showing it" — you do NOT need to also cross-reference `active_playlist_ids`. The #154 lyrics idle gate relies on exactly this (`lyrics/idle_gate.rs::any_playing`): "any snapshot Playing" = "the wall is showing an output" = defer heavy GPU work. Read the registry in-process (the engine already holds the `Arc`); never HTTP-loop `/api/v1/ndi/health` back to your own server.

## A restart is a receiver lottery until camera-box re-resolves — pin the name→port map (#196)
DistroAV's genlock build reconnects a stale source **BY URL with the PINNED
previous port** (`reset_ndi_receiver: connect BY-URL '10.77.9.201:5970'`), and
the NDI runtime hands each `send_create` the next free TCP port from ~5961 up in
**creation order**. So if SongPlayer creates its senders in a non-deterministic
order across a restart (the old lazy / thread-raced path), a stream name can
move to a different port and the receiver's by-URL reconnect lands on the wrong
or a dead sender → `connections=0` on the on-program output = dark wall, and the
#173 receiver-side ladder CANNOT clear it (only another restart re-rolls it). Fix
(round 1): **deterministic, restart-safe creation** in `playback/startup_senders.rs`
+ `runtime_pipeline.rs::ensure_pipeline_inner`:
- **Port-availability wait first** (`wait_for_ports_free` + `ndi_ports_free`, ≤10 s
  poll on 5960..=5960+N+1) so an immediate restart waits for the previous
  instance's listeners to release before creating — same span, same assignment.
- **Serialized id-order creation:** `create_startup_senders` creates each active
  playlist's sender one at a time in `playlist.id` order, waiting for a per-pipeline
  ready one-shot (fired by the pipeline thread right after `send_create`) before the
  next — so `send_create` runs in a fixed order every restart, not OS-scheduler order.
  Runs before the engine command loop drains scene events, so no lazy scene-triggered
  creation preempts it.
- Box-verified 2026-09-20: after a deploy restart, on-program SP-slow
  `connections=2`, every output 2–4, no dark wall.
- **No dark-wall ladder for an output with no OBS input** (`effective_dark_reason`
  + `PlaybackEngine::output_has_obs_input`, tokio `try_read` on the shared
  `NdiSourceMap`): a Playing-on-program output at `connections=0` whose stream is
  advertised by NO OBS NDI input gets `degraded_reason = "no OBS scene for this
  output"` (not the dark-wall reason), so `is_dark` is false and the every-10 s
  degraded/recovered flap stops (the SP-dabing-before-its-scene case).

**GOTCHA — `NDIlib_send_get_source_name().p_url_address` is EMPTY for a local
sender (#196).** The plan was to surface each sender's advertised `host:port` as
`sender_url` on `/api/v1/ndi/health` by reading `p_url_address` from
`NDIlib_send_get_source_name`. On the real win-resolume NDI runtime that field is
empty for a LOCAL sender (verified: `sender_url` null for all 10 outputs on a
stable process, even with a ≤2 s post-create retry) — `send_get_source_name`
returns the sender's NAME (`p_ndi_name`), not the URL a receiver connects to. The
port ASSIGNMENT is still deterministic; only its DISPLAY via `sender_url` is
unavailable this way. To surface the name→port map, use `NDIlib_find`
receiver-side discovery or read SongPlayer's own listening ports (5960–5970) and
correlate by creation order — NOT the sender-side `get_source_name`.
