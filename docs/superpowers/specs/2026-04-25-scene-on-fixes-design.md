# Scene-On Fixes: Title Refresh + Pipeline State Logs — Design

**Date:** 2026-04-25
**Issues:** #45 (title not refreshed on scene-active), #46 (NDI dark wall after restart — defensive)

## Goal

Two related fixes around the OBS scene-becomes-program transition:

1. **#45 — Functional fix.** When a song is already playing in `Playing` state on a playlist whose OBS scene was off-program, switching the scene on-program must re-push the title to Resolume + OBS. Today it only re-fires `VideosAvailable + SceneOn` events, which the state machine ignores in `Playing`, so the title text on Resolume stays stale (last song's title, or empty).

2. **#46 — Defensive logging.** Both root causes called out in the issue are at the code level already handled (custom-playlist past-end auto-wrap shipped in v21 commit `f1701ed`; pipeline's `Play` handler clears `paused = false` at line 295 before decode starts). What is missing is structured visibility into the pipeline's `Pause`/`Resume`/`Play` state transitions, so the next "wall went dark" occurrence is diagnosable from logs alone.

## Non-goals

- Subtitle re-push on scene-go-on. The line-change hook in `playback/mod.rs` re-emits `ShowSubtitles` on every word/line, so subtitles auto-recover on the next sung word. Title is one-shot (1.5 s after `Started`), so it is the only surface that stays stale.
- Engine-layer `Resume`-before-`Play` wiring. The pipeline already clears `paused` inside its `Play` handler before decode begins, so an explicit Resume from the engine would be redundant. Adding it would obscure the actual code path.
- Reproducing #46 with a deterministic test. The original symptom (dark wall on 2026-04-21) was almost certainly the past-end-position selector bug now fixed by v21. With logs in place, any recurrence is traceable; no synthetic repro is needed.
- Fixing #43 (op=7 out-of-band routing). Tracked separately, lower urgency — current 2 s timeout bound is good enough.

## Background

### #45 root cause

`crates/sp-server/src/playback/mod.rs::handle_scene_change` (line 248) handles the `on_program=true` branch by firing two state-machine events:

```rust
self.apply_event(playlist_id, PlayEvent::VideosAvailable).await;
self.apply_event(playlist_id, PlayEvent::SceneOn).await;
```

The `PlayState` machine (`playback/state.rs`) handles `SceneOn` only from `WaitingForScene` (line 69 — emits `SelectAndPlay`). For an already-`Playing` pipeline, `SceneOn` has **no transition** — the state machine returns the input state unchanged with no action.

The title-show task that was originally spawned 1.5 s after `Started` (line 421) reads `scene_active.load()` and aborts with `"title suppressed — off program"` if the scene was off-program at that 1.5 s boundary. By the time the operator switches scene on-program, that task is already gone — there is no machinery that re-pushes the title.

### #46 status

- **Root cause 1 (selector past-end):** `f1701ed feat: v21 resilience pack` includes auto-wrap for custom-playlist `current_position`. Merged via PR #48, currently on `main` and deployed to win-resolume since the 2026-04-25 CI-perf release.
- **Root cause 2 (paused state leak):** `pipeline.rs:295` already executes `paused = false;` immediately on receipt of `PipelineCommand::Play`, *before* the inner decode loop starts. There is no code path that runs `decode_and_send` with `paused=true` carried over from a prior command.

Both are handled. The remaining defect is observability: the existing `debug!` logs at lines 198, 201, 346–347, 349–351 are at debug level (filtered out in production), and they don't include the prior state — so when a future "wall went dark" event happens, log grep cannot reconstruct the sequence.

## Change 1 — #45 functional fix

### Where

`crates/sp-server/src/playback/mod.rs::handle_scene_change` — append a new branch at the end of the `on_program == true` block.

### What

After the existing `apply_event(VideosAvailable)` + `apply_event(SceneOn)` calls, inspect the pipeline's `state` field:

```rust
if on_program {
    self.apply_event(playlist_id, PlayEvent::VideosAvailable).await;
    self.apply_event(playlist_id, PlayEvent::SceneOn).await;

    // #45 — re-push title for already-Playing pipelines that just
    // gained program. Title is one-shot (1.5 s after Started) and
    // gets suppressed if scene_active was false at that boundary;
    // there is no other path that re-pushes it.
    if let Some(pp) = self.pipelines.get(&playlist_id) {
        if let PlayState::Playing { video_id } = pp.state {
            self.push_title_for_playing(playlist_id, video_id).await;
        }
    }
}
```

`push_title_for_playing` is a new private method that mirrors the title-push block at lines 427–455 of the existing title-show task, minus the 1.5 s sleep and the `scene_active.load()` guard (we already know it just became true):

```rust
async fn push_title_for_playing(&self, playlist_id: i64, video_id: i64) {
    let Ok(Some((song, artist))) =
        get_video_title_info(&self.pool, video_id).await
    else {
        return;
    };
    let text = if artist.is_empty() {
        song.clone()
    } else if song.is_empty() {
        artist.clone()
    } else {
        format!("{song} - {artist}")
    };
    if let Some(cmd_tx) = &self.obs_cmd_tx {
        let _ = cmd_tx
            .send(crate::obs::ObsCommand::SetTextSource {
                source_name: OBS_TITLE_SOURCE.to_string(),
                text,
            })
            .await;
    }
    let _ = self
        .resolume_tx
        .send(crate::resolume::ResolumeCommand::ShowTitle { song, artist })
        .await;
    info!(playlist_id, video_id, "title re-pushed on scene-go-on");
}
```

Idempotent — safe to fire even if a title was already showing for that song. Resolume's A/B crossfade lane handles repeated `ShowTitle` for the same text gracefully (no-op when text matches the active lane's current text).

### Test

`crates/sp-server/src/playback/tests_scene_change.rs` — new test:

```rust
#[tokio::test]
async fn scene_go_on_refreshes_title_for_already_playing() {
    // Build engine with mock channels, insert a video, force pipeline
    // state to Playing { video_id }, set scene_active=false, drain any
    // residual ShowTitle from setup. Then call handle_scene_change(true).
    // Assert exactly one ShowTitle was pushed to resolume_rx with the
    // expected (song, artist).
}
```

Existing test scaffolding in `tests_scene_change.rs` shows how to build the engine + observe channel traffic.

## Change 2 — #46 defensive logging

### Where

`crates/sp-server/src/playback/pipeline.rs::run_pipeline_thread` (the active codec impl, currently lines 271–360 area).

### What

Promote the existing `debug!` to `info!` at three call sites and include the prior `paused` value:

```rust
Ok(PipelineCommand::Play { video, audio }) => {
    info!(
        playlist_id,
        prev_paused = paused,
        ?video, ?audio,
        "pipeline: Play received (paused → false)"
    );
    // ... existing code, paused = false stays at line 295 ...
}

Ok(PipelineCommand::Pause) => {
    info!(playlist_id, prev_paused = paused, "pipeline: Pause (paused → true)");
    paused = true;
}

Ok(PipelineCommand::Resume) => {
    info!(playlist_id, prev_paused = paused, "pipeline: Resume (paused → false)");
    paused = false;
}
```

Same pattern for the stub-mode handler at lines 198–201 (kept for symmetry).

### Test

`crates/sp-server/src/playback/pipeline.rs` (or `playback/tests.rs`) — pure unit test for the state-clearing invariant:

```rust
#[test]
fn play_clears_paused_state() {
    // Send Pause then Play to a stub pipeline; assert decode entry
    // observes paused=false. Already covered indirectly today; the
    // explicit test pins the invariant so future refactors can't break it.
}
```

This test exists primarily as a tripwire — if anyone ever moves the `paused = false` line out of the `Play` handler, this test fires red.

### Out of scope

Adding `cargo-mutants::skip` annotations on the new log lines. They are pure observability, no behavioural mutants worth catching. The pin-test on `paused = false` is the meaningful guarantee.

## Files touched

| File | Change |
|---|---|
| `crates/sp-server/src/playback/mod.rs` | +1 conditional block in `handle_scene_change`, +1 new private fn `push_title_for_playing` (~30 lines) |
| `crates/sp-server/src/playback/pipeline.rs` | 3 → 6 lines: promote debug! to info! with `prev_paused` field |
| `crates/sp-server/src/playback/tests_scene_change.rs` | +1 new test (~40 lines) |
| `crates/sp-server/src/playback/tests.rs` (or new pipeline test mod) | +1 pin test for `paused=false` after Play |

No new modules, no new dependencies, no migration. Estimated total diff: ~80 lines added, ~3 changed.

## Verification

### Local

- `cargo fmt --all --check` (only check that runs locally per airuleset).

### CI

- All existing tests stay green.
- New tests pass:
  - `scene_go_on_refreshes_title_for_already_playing`
  - `play_clears_paused_state`

### Post-deploy on win-resolume

Manual repro of #45:

1. Switch OBS program to a non-`sp-*` scene.
2. POST `/api/v1/playlists/{id}/play-video` with a different playlist's video.
3. Confirm log line `"title suppressed — off program"` appears.
4. Switch OBS program to that playlist's `sp-*` scene.
5. **Expected:** Resolume shows the new song's title within ~500 ms.
6. **Logs:** new `"title re-pushed on scene-go-on"` line at `info!`.

For #46, no manual repro — the defensive logging is verified by the new info-level lines appearing in win-resolume's `tracing` output on any subsequent Pause/Resume/Play cycle.

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Title re-push fires twice (initial 1.5 s task + new scene-go-on) | medium | Resolume gets two `ShowTitle` for the same text | Resolume driver no-ops same-text writes; logged at info but cosmetic only |
| `pipelines.get(&playlist_id)` is `None` due to race during shutdown | low | scene-go-on runs without title push | Wrapped in `if let Some(pp)`; harmless |
| Info-level pipeline logs add noise on busy machines | low | log volume goes up by ~2 lines per song | Three lines per Play/Pause/Resume; on a 6-playlist setup that is ~30 lines/min — fine |

## Out of scope (recorded for future)

- An ensemble approach to `handle_scene_change` (e.g., emit a `ScenePromoted` event the title task can listen to instead of polling state). Worthwhile if more "becomes-program needs to refresh X" surfaces appear; not yet justified.
- Promoting all pipeline `debug!` calls to `info!` for full cycle visibility. The three Pause/Resume/Play sites are the diagnostic-load-bearing ones.
