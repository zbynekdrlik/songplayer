---
paths:
  - "crates/sp-server/src/obs/**"
  - "crates/sp-server/src/playback/ndi_health.rs"
  - "e2e/post-deploy.spec.ts"
  - "e2e/ndi-health-gate.ts"
  - "e2e/post-deploy-av-sync.spec.ts"
  - "e2e/av-sync-gate.ts"
  - "e2e/obs-driver.ts"
  - "scripts/av_sync_check.py"
  - "scripts/tests/test_av_sync_check.py"
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
sender (#196).** The round-1 plan surfaced each sender's advertised `host:port`
as `sender_url` on `/api/v1/ndi/health` by reading `p_url_address` from
`NDIlib_send_get_source_name`. On the real win-resolume NDI runtime that field is
empty for a LOCAL sender (verified: `sender_url` null for all 10 outputs on a
stable process, even with a ≤2 s post-create retry) — `send_get_source_name`
returns the sender's NAME (`p_ndi_name`), not the URL a receiver connects to. The
port ASSIGNMENT is still deterministic; only its DISPLAY via `sender_url` was
unavailable this way.

## The name→port map is read via `NDIlib_find`, not the sender getter (#196 round 2)

The ruling on the GOTCHA above: read each sender's advertised `host:port` from
the SDK's OWN discovery, not the sender-side getter.
`sp_ndi::NdiBackend::discover_local_sources` opens ONE `NDIlib_find_create_v2`
(`show_local_sources = true`) AFTER the id-ordered startup senders exist, polls
`NDIlib_find_get_current_sources` for ≤ 3 s until every own name appears, records
`p_url_address` per output, then destroys the finder;
`startup_senders::discover_and_record_sender_urls` matches the discovered
`(name, url)` to our outputs with the pure `sp_ndi::find::{source_matches,
match_source_urls}` (`"RESOLUME-SNV (SP-x)"` matches own bare `"SP-x"` by the
`"(<bare>)"` suffix), writes them into `NdiHealthRegistry` (surfaced as
`sender_url` on `/api/v1/ndi/health`), and logs one `ndi: sender ready
name=SP-x url=10.77.9.201:5963` line per output (one WARN if a name never
appears within 3 s). It retries ONCE at +30 s (the finder can take a moment to
see a fresh local sender). `MockNdiBackend::set_discovered_sources` drives the
whole match path on Linux; `send_get_source_name` stays only as the name check.

## Post-restart receiver self-check — the ladder is NOT the tool for it (#196 round 2)

The #173 receiver-side ladder CANNOT clear a restart wedge (only another restart
re-rolls it), so a distinct, LADDER-FREE self-check makes a failed post-restart
reconnect VISIBLE instead:

- **Baseline:** each health poll persists the per-output receiver count in the
  `settings` table (`db/models_ndi.rs`, key `ndi_last_receivers_<id>`, one row per
  output — never `db/models.rs` at the 1000-line cap). At startup that map is read
  back ONCE (`NdiHealthRegistry::seed_pre_restart_counts`) as the PRE-restart
  baseline; the live counts keep being persisted for the NEXT restart.
- **Decision (pure, in `sp_core::health::no_receiver_after_restart`,
  exact-boundary + mutation tested):** 30 s after the senders are ready
  (`mark_senders_ready` → `elapsed_since_ready`), an output that is on program OR
  had `≥ 1` receiver before the restart, has NOT reconnected since (a one-time
  latch — once it reaches `≥ 1` it is never flagged again this process, so a
  later legitimate off-program drop is not a restart failure), and still has
  `< 1` receiver → `degraded_reason = "no receiver after restart"`
  (`sp_core::health::NO_RECEIVER_AFTER_RESTART_REASON`). This is a NON-dark-wall
  reason, so `is_dark` is false and the ladder never runs; ONE WARN per output
  (latched, cleared on recovery); the manual `POST /api/v1/ndi/recover/{id}`
  stays. Precedence: the "no OBS scene for this output" reason (item 5) wins over
  the self-check when there is genuinely no OBS input.
- **HealthBar:** the shared `HealthBar` renders a `health-ndi` segment
  `NDI: N výstup(y/ov) bez prijímača` (Slovak plural via
  `sp_core::health::ndi_label`/`ndi_output_word`), hidden when N=0, clearing the
  moment they reconnect. It counts snapshots whose `degraded_reason` equals the
  shared reason string.
- **Caveat:** a previously-connected output intentionally taken OFF program right
  at the restart (and never re-subscribed) can read as flagged until it reconnects
  once — accepted (the wall is a persistent installation where DistroAV keeps
  off-program `sp-*` sources subscribed, so a previously-connected output that
  stays 0 IS the anomaly worth surfacing).

## One restart per push (#196 round 2)

The Deploy job starts SongPlayer; the post-deploy E2E job then restarted it
AGAIN, doubling the per-push receiver-lottery rolls. `/api/v1/status` now carries
`uptime_s` (`crate::process_start`, marked at the top of `lib::start`), and the
E2E "Restart SongPlayer" step SKIPS the restart (`exit 0`) when the running
process reports the deployed `VERSION` (from the checkout) AND `uptime_s < 600` —
i.e. it IS the fresh Deploy-started process — logging which branch it takes.
The engine's OBS scene-poll reconcile + the OBS-client reconnect backoff cover
the "pick up OBS after OBS start" case the restart used to serve.

## A restart is a receiver lottery until camera-box re-resolves — SongPlayer keeps the map stable

DistroAV's genlock build reconnects a stale source BY URL with the PINNED
previous port; SongPlayer's job is to keep the name→port map IDENTICAL across
restarts (round 1: port-availability wait + serialized id-order creation) so that
by-URL reconnect lands on the right sender. SongPlayer now also MAKES the map
visible (`sender_url` via `NDIlib_find`) and ESCALATES a failed reconnect
(the self-check above) — but the receiver-side re-resolve after a sender restart
is camera-box's (camera-box#1096/#1302). Read `sender_url` per output on
`/api/v1/ndi/health` to confirm the map is stable across the 10-restart box
acceptance.

## Post-deploy A/V gate (#147) — lipsync + audio dropouts on the REAL output

**Rule: no change to decode, pacing, the mixer, NDI or the audio path merges
unless this gate is green.** The owner saw a major lipsync regression while
every other gate was green. This gate is the one that measures what the wall
and the recording actually get.

- **Where it runs:** `e2e/post-deploy-av-sync.spec.ts`, inside the E2E job's
  "Feature-level Playwright (post-deploy spec)" step (`post-deploy.config.ts`
  matches `post-deploy*.spec.ts`). It uses the shared `ObsDriver` (obs-websocket).
  It parks the program on the shared baseline scene (`e2e/obs-baseline-scene.ts`:
  sp-slow, never sp-warmup/sp-fast). It proves the output is PLAYING with
  `/api/v1/ndi/health`: `state=Playing` AND `frames_submitted_last_5s > 0`.
  It sets the SONG faders to unity, then `StartRecord` → 20 s → `StopRecord`,
  which returns `outputPath`.
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
    120 s of the 300 s budget is used, so a full worst-case take still fits.
    The run is classified by `classifyAvSyncRun`: the stdout JSON and the exit
    code must agree. Missing JSON (a numpy import failure, an argparse error)
    is `error`, not a verdict.
  - It deletes every recording plus its auto-remux sibling. When the profile's
    `Video/AutoRemux` is on, an mkv also leaves `<base>.mp4`.
    `removeRecording` waits for the remux and retries while OBS still holds
    the file. An undeletable file fails the test after the verdict, never
    masking it.
  - `afterAll` first sets `tornDown`: Playwright does not cancel a
    timed-out body, so the body refuses to start a recording or a skip after
    it. `ObsDriver.lastRecordingPath` keeps the file of a StopRecord whose
    inactive-poll timed out, so that file is deleted too.
  - `afterAll` is the safety net for a timed-out body. It kills a
    still-running analysis (a Windows `taskkill /T`: python AND its ffmpeg
    children hold the file open), stops our recording (only while
    `isRecording()`), re-deletes every recording made (late remux siblings
    too), restores the faders and restores the scene. Each step runs in its
    own try/catch, and the errors are asserted together at the end.
- **Original sidecars:** `/api/v1/playlists/{id}/videos` has NO `file_path`.
  The pair is resolved from the cache listing by
  `*_{youtube_id}_normalized[_gf]_{video.mp4|audio.flac}`
  (`resolveSidecars`). Exactly one complete pair must match, otherwise the gate
  throws.
- **Analysis:** `scripts/av_sync_check.py` runs under the lyrics venv Python
  (`SP_AVSYNC_PYTHON`) with the bundled ffmpeg (`SP_FFMPEG`). Only numpy is
  needed. The box has no ffprobe, so stream start times and frame sizes come
  from ffmpeg's own `showinfo`/`ashowinfo` pts.
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
       not judged. An EXCEPTION in the picture step (probe, decode,
       no-shift) lands here too, as a `video analysis error` reason with
       `av_ms: null`, so dropouts already found still decide step 2. An
       audio-step exception is `cannot_measure` with sides `["error"]`,
       which is never retaken.
    4. |A/V| > 40 → `fail`, else `pass`.
  - **letterbox crop:** the original is cropped (`source_crop`) to exactly
    the grid cells the recording keeps before scaling. A letterbox edge
    inside a cell would otherwise skew the geometry by up to one cell.
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
  `AV-SYNC status=… av_ms=… audio_corr=… video_match=… video_contrast=…
  dropouts=… glitches=… reasons=[…]`.
  - `av_ms` > 0 means audio AHEAD of picture. `audio.offset_s` and
    `video.offset_s` are `orig_time − rec_time`.
  - `video.plateau_ms` is the video's resolution. It is up to one source-frame
    period when source and recording frame rates are equal, so ~±17 ms of the
    measured A/V is quantization.
  - `dropouts.dropout_events[].start_s` and `glitch_times_s` are recording
    times. Glitches (relative error > 0.8, not silent) are informational only.
  - `audio.second_corr` is the best audio match more than 0.5 s away from
    the peak. A repeated chorus can come close to `corr`. A wrong peak then
    shows up as a low video match (`cannot_measure`), never as a false pass.
  - Baseline on 24.9.2026 (manual): A/V +13 ms, corr 0.997, match 0.999,
    0 dropouts.
- **Tested:** the pure functions are covered by pytest on synthetic click-train
  plus flash-frame fixtures in Eval Checks (numpy only). The ffmpeg I/O layer
  is untested in CI by design, because that job has no ffmpeg. It was checked
  locally against ffmpeg-muxed mkv and mp4 AAC "recordings" with known offsets
  (+120 → 116, 0 → −3/−4, −80 → −83 ms, and an 85 ms zeroed stretch → a
  dropout). It was checked with both ffmpeg 6.1 and the BtbN master build.
- **Blind spot:** the reference is the sidecar pair itself. An offset baked
  into the sidecars at download/normalize time is invisible to this gate.
- **Unverified until the first box run:** the thresholds (0.95 match, 0.002
  contrast) have not been measured with the global method on a real OBS
  recording. Static overlays in the sp-slow scene (title text, logos) lower
  the match. Read the first run's `AV-SYNC` line before trusting a red or a
  green.
- **Do not "fix" a red gate** by raising 40 ms, lowering 0.9/0.95, or skipping
  on exit 2. Find what moved the audio or the picture.
