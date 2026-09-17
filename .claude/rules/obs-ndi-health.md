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
