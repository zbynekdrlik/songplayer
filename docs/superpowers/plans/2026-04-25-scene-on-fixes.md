# Scene-On Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix issue #45 (Resolume title not refreshed when scene becomes active for an already-playing song) and add defensive Pause/Resume/Play state-transition logs to the pipeline (#46).

**Architecture:** Two small, focused changes inside `crates/sp-server/src/playback/`. No new crates, no migrations, no new dependencies. The functional fix adds a single conditional block to `handle_scene_change` and a private helper. The defensive change promotes three pipeline logs from `debug!` to `info!` with a `prev_paused` field.

**Tech Stack:** Rust 2024, tokio, sqlx, existing `playback` module patterns.

**Spec:** `docs/superpowers/specs/2026-04-25-scene-on-fixes-design.md`

**Branch:** `dev` (no worktree, two-file change). Implementer never pushes — controller batches and pushes once at the end.

---

## Airuleset constraints (every implementer must follow)

- **TDD strict.** Write a failing test first, watch it fail, implement, watch it pass, `cargo fmt --all --check`, commit on green. Never skip the fail step. If a Rust test cannot be run locally (the rule), still write it first and trust by inspection.
- **Never run `cargo clippy/test/build` locally.** Only `cargo fmt --all --check` runs locally. Everything else runs on CI.
- **File size cap 1000 lines.** Current sizes: `mod.rs=954`, `pipeline.rs=692`. Adding ~30 lines to `mod.rs` brings it to ~984 — under the cap but tight; do not add anything beyond what this plan specifies.
- **Commit after each green test.** One commit per task step that says "Commit". Implementer does NOT push.
- **`mutants::skip` requires a one-line justification.** This plan adds no `mutants::skip` (the new code is genuinely test-covered).
- **No emojis.**

---

## Task 1: #45 — failing test for scene-go-on title refresh

**Files:**
- Modify: `crates/sp-server/src/playback/tests_scene_change.rs`

**Model:** sonnet (test setup needs careful channel-mock work)

- [ ] **Step 1: Read existing scaffolding**

Read `crates/sp-server/src/playback/tests_scene_change.rs` end-to-end. Note how the existing tests build a `PlaybackEngine` with mock channels and observe the `resolume_rx` for `ResolumeCommand` traffic. Use the same builder pattern.

- [ ] **Step 2: Write the failing test**

Append to `crates/sp-server/src/playback/tests_scene_change.rs`:

```rust
#[tokio::test]
async fn scene_go_on_refreshes_title_for_already_playing() {
    use crate::resolume::ResolumeCommand;

    let (engine_setup, mut resolume_rx, _obs_rx) = build_test_engine().await;
    let (mut engine, playlist_id, video_id) = engine_setup;

    // Insert a video row so get_video_title_info can resolve song/artist.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, position, normalized) \
         VALUES (?, ?, 'abc123', 'Test Song', 'Test Artist', 0, 1)",
    )
    .bind(video_id)
    .bind(playlist_id)
    .execute(&engine.pool)
    .await
    .unwrap();

    // Force pipeline into Playing state with scene_active=false.
    if let Some(pp) = engine.pipelines.get_mut(&playlist_id) {
        pp.state = PlayState::Playing { video_id };
        pp.scene_active.store(false, Ordering::Release);
    }

    // Drain any residual messages from setup.
    while resolume_rx.try_recv().is_ok() {}

    // Trigger scene becoming program.
    engine.handle_scene_change(playlist_id, true).await;

    // Expect a ShowTitle on resolume_rx within a short window.
    let cmd = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        resolume_rx.recv(),
    )
    .await
    .expect("ShowTitle should arrive within 500ms")
    .expect("channel should not be closed");

    match cmd {
        ResolumeCommand::ShowTitle { song, artist } => {
            assert_eq!(song, "Test Song");
            assert_eq!(artist, "Test Artist");
        }
        other => panic!("expected ShowTitle, got {other:?}"),
    }
}
```

If `build_test_engine` does not exist, refactor an existing helper out of one of the existing tests in the file (lift the inline setup into a helper, then both old and new tests share it). The refactor stays inside `tests_scene_change.rs` — do not touch any other file in this step.

- [ ] **Step 3: Confirm formatting**

Run: `cargo fmt --all --check`
Expected: clean exit.

- [ ] **Step 4: Commit (test only — will fail on CI)**

```bash
git add crates/sp-server/src/playback/tests_scene_change.rs
git commit -m "test(playback): scene-go-on must refresh title for already-Playing pipelines (#45)"
```

This commit is intentionally test-only; the test fails until Task 2 lands. The controller batches both commits before pushing, so CI sees the test passing in the same push.

---

## Task 2: #45 — implementation

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs`

**Model:** sonnet

- [ ] **Step 1: Add the private helper `push_title_for_playing`**

Insert this method into the same `impl PlaybackEngine` block that already contains `handle_scene_change`. Put it directly above `handle_scene_change` so the call site can find it without scrolling. Use the existing `OBS_TITLE_SOURCE` constant and the existing imports (`info`, `crate::obs::ObsCommand`, `crate::resolume::ResolumeCommand`, `get_video_title_info`).

```rust
    /// Re-push the song title to OBS + Resolume for an already-Playing
    /// pipeline. Used when a scene becomes program for a pipeline that
    /// was already playing off-program — the original 1.5 s post-Started
    /// title-show task already aborted with "title suppressed — off program",
    /// so without this re-push the title on Resolume stays stale (last
    /// song's title, or empty). Idempotent — Resolume's A/B lane no-ops a
    /// same-text write.
    async fn push_title_for_playing(&self, playlist_id: i64, video_id: i64) {
        let Ok(Some((song, artist))) = get_video_title_info(&self.pool, video_id).await else {
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

- [ ] **Step 2: Wire the call into `handle_scene_change`**

In `crates/sp-server/src/playback/mod.rs::handle_scene_change`, find the `if on_program {` branch (currently around line 272). After the existing `apply_event(SceneOn)` call, append the title re-push:

```rust
        if on_program {
            self.apply_event(playlist_id, PlayEvent::VideosAvailable)
                .await;
            self.apply_event(playlist_id, PlayEvent::SceneOn).await;

            // #45 — re-push title for an already-Playing pipeline that
            // just gained program. The 1.5 s post-Started title-show task
            // suppressed itself if scene_active was false at that boundary;
            // there is no other path that re-pushes it.
            let video_id = self
                .pipelines
                .get(&playlist_id)
                .and_then(|pp| match pp.state {
                    PlayState::Playing { video_id } => Some(video_id),
                    _ => None,
                });
            if let Some(video_id) = video_id {
                self.push_title_for_playing(playlist_id, video_id).await;
            }
        } else {
            self.apply_event(playlist_id, PlayEvent::SceneOff).await;
        }
```

The intermediate `let video_id = ...` exists to release the immutable borrow on `self.pipelines` before the `&self` call to `push_title_for_playing`.

- [ ] **Step 3: Confirm formatting**

Run: `cargo fmt --all --check`
Expected: clean exit.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/playback/mod.rs
git commit -m "fix(playback): refresh title on scene-go-on for already-Playing pipelines (#45)"
```

Test from Task 1 now passes — verified by inspection (the new branch in `handle_scene_change` matches what the test asserts).

---

## Task 3: #46 — failing test for paused-cleared-on-Play invariant

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs` (test module at the bottom of the file)

**Model:** haiku (mechanical pin test)

- [ ] **Step 1: Locate the existing test module**

`crates/sp-server/src/playback/pipeline.rs` ends with a `#[cfg(test)] mod tests { ... }` block. If no such block exists, add one. If it exists, append to it.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn play_command_clears_paused_state() {
    // Pin: PipelineCommand::Play must reset paused = false BEFORE
    // entering decode, so a stale Pause cannot leak across video
    // changes. This test fails red if anyone ever moves the
    // `paused = false` assignment out of the Play arm in the loop.

    use crossbeam_channel::unbounded;
    let (cmd_tx, cmd_rx) = unbounded::<PipelineCommand>();

    // Send Pause then Play; capture via the channel.
    cmd_tx.send(PipelineCommand::Pause).unwrap();
    cmd_tx
        .send(PipelineCommand::Play {
            video: std::path::PathBuf::from("/tmp/dummy_video.mp4"),
            audio: std::path::PathBuf::from("/tmp/dummy_audio.flac"),
        })
        .unwrap();

    // Drive the command loop synchronously up to (but not including)
    // decode_and_send by reading the source: the Play arm sets
    // paused=false at the line marked with `paused = false;` BEFORE
    // calling decode_and_send. This test asserts that ordering by
    // matching against the source.
    //
    // Static check via include_str! — fails if the line moves.
    let src = include_str!("pipeline.rs");
    let play_arm_start = src
        .find("Ok(PipelineCommand::Play {")
        .expect("Play arm must exist");
    let decode_call = src[play_arm_start..]
        .find("decode_and_send(")
        .expect("Play arm must call decode_and_send");
    let play_block = &src[play_arm_start..play_arm_start + decode_call];
    assert!(
        play_block.contains("paused = false"),
        "PipelineCommand::Play must clear paused = false BEFORE decode_and_send. \
         Current Play arm:\n{play_block}"
    );
}
```

This is a pin test: it asserts the invariant lives in the source via `include_str!` rather than driving the channel loop (which would require a full thread + decoder). The pin test fires red if anyone moves `paused = false;` outside the Play arm or after `decode_and_send`.

- [ ] **Step 3: Confirm formatting**

Run: `cargo fmt --all --check`
Expected: clean exit.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/playback/pipeline.rs
git commit -m "test(pipeline): pin paused=false-on-Play invariant (#46 defensive)"
```

---

## Task 4: #46 — promote pipeline logs to info! with prev_paused

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs`

**Model:** haiku (mechanical log promotion)

- [ ] **Step 1: Promote the active-codec arm logs**

In `crates/sp-server/src/playback/pipeline.rs`, find the active command loop (the one with `let mut paused = false;` at line 271). Edit the three command arms:

**`PipelineCommand::Play` arm** (currently around line 282–343):

Add an `info!` immediately at the top of the arm, BEFORE the inner-loop `let mut current_video = video;`:

```rust
            Ok(PipelineCommand::Play { video, audio }) => {
                info!(
                    playlist_id,
                    prev_paused = paused,
                    ?video,
                    ?audio,
                    "pipeline: Play received (paused -> false)"
                );
                let mut current_video = video;
                let mut current_audio = audio;
                // ... existing code unchanged ...
```

Keep `paused = false;` at line 295 exactly where it is (the pin test from Task 3 enforces this).

**`PipelineCommand::Pause` arm** (currently around line 345–348):

Replace the existing `debug!`:

```rust
            Ok(PipelineCommand::Pause) => {
                info!(playlist_id, prev_paused = paused, "pipeline: Pause (paused -> true)");
                paused = true;
            }
```

**`PipelineCommand::Resume` arm** (currently around line 349–352):

Replace the existing `debug!`:

```rust
            Ok(PipelineCommand::Resume) => {
                info!(playlist_id, prev_paused = paused, "pipeline: Resume (paused -> false)");
                paused = false;
            }
```

- [ ] **Step 2: Promote the stub-codec arm logs**

The stub command loop (currently at lines 176–212, the `#[cfg(...)]` stub used for non-Windows test compilation) has its own `Pause`/`Resume`/`Play` handlers. For symmetry, replace the existing `info!` lines there with the same `prev_paused` field shape. The stub has no `paused` variable — track it with a local `let mut paused_stub = false;` at the top of the stub loop and update it on Pause/Resume, mirroring the active loop. If introducing a tracked variable breaks the stub's existing test coverage, leave the stub logs unchanged — symmetry is nice-to-have, not load-bearing.

- [ ] **Step 3: Confirm formatting**

Run: `cargo fmt --all --check`
Expected: clean exit.

- [ ] **Step 4: Commit**

```bash
git add crates/sp-server/src/playback/pipeline.rs
git commit -m "feat(pipeline): info! state-transition logs on Pause/Resume/Play (#46)"
```

---

## Task 5: Push + monitor CI (controller-only)

**Model:** controller (you, not a subagent — this is a `git push` + `gh run view` flow)

- [ ] **Step 1: Pre-push sanity**

```bash
git fetch origin
git status
git log --oneline origin/dev..HEAD
```

Expected: 4 commits on `dev` ahead of `origin/dev`:
1. `test(playback): scene-go-on must refresh title for already-Playing pipelines (#45)`
2. `fix(playback): refresh title on scene-go-on for already-Playing pipelines (#45)`
3. `test(pipeline): pin paused=false-on-Play invariant (#46 defensive)`
4. `feat(pipeline): info! state-transition logs on Pause/Resume/Play (#46)`

- [ ] **Step 2: Verify formatting clean**

```bash
cargo fmt --all --check
```

Expected: clean exit.

- [ ] **Step 3: Push**

```bash
git push origin dev
```

- [ ] **Step 4: Monitor CI to terminal**

Run a single backgrounded `sleep N && gh run view <run-id>` per `airuleset/ci-monitoring.md`. Do not poll-loop, do not use `gh run watch`. Read `gh run list --branch dev --limit 1 --json databaseId,status,conclusion` to get the run id, then:

```bash
sleep 600 && gh run view <run-id> --json status,conclusion,jobs
```

Steady-state CI is ~17 min on a cache hit. If `runner queued > 2 min` for `Deploy to win-resolume`, ping win-resolume and report (per `feedback_check_runner_when_queued.md`). Do not silently wait through a queued state.

- [ ] **Step 5: Verify all jobs green**

Once CI reaches terminal state, confirm conclusion=success on every job. If any job fails, investigate via `gh run view <run-id> --log-failed` and fix in one batched commit.

- [ ] **Step 6: Manual post-deploy verification of #45**

On win-resolume (via MCP):

```
1. Switch OBS program to a non-sp-* scene (e.g., the Pause/Black scene).
2. POST /api/v1/playlists/<id>/play-video with a known video on a different playlist.
3. Read sp-server logs — confirm "title suppressed — off program" appears.
4. Switch OBS program to that playlist's sp-* scene.
5. EXPECTED: Resolume shows the new song's title within ~500 ms.
6. EXPECTED: log line "title re-pushed on scene-go-on" at info level.
```

If the title appears as expected, #45 is verified. If not, capture the log span and stop — do not declare done.

- [ ] **Step 7: Verify #46 logs are present**

Read sp-server logs from the latest playback session and confirm at least one `pipeline: Play received (paused -> false)` and `pipeline: Pause (paused -> true)` line appears at `info!` level (the previous `debug!` would have been filtered out in production).

- [ ] **Step 8: Close the issues**

Once CI is green and post-deploy verification confirms #45 is fixed:

```bash
gh issue close 45 --comment "Fixed by commit <SHA-of-fix>. handle_scene_change(on_program=true) now re-pushes ShowTitle to Resolume + OBS for already-Playing pipelines. Verified post-deploy on win-resolume."
gh issue close 46 --comment "Defensive logging shipped in commit <SHA-of-logs>. Both root causes (selector past-end, paused state leak) are already handled at code level (v21 auto-wrap + pipeline auto-clears paused on Play). Pause/Resume/Play state transitions now log at info! with prev_paused field, so any recurrence is diagnosable from logs."
```

---

## Verification (controller-only)

After all 5 tasks complete:

| Check | Expected |
|---|---|
| All 4 commits present on dev | `git log origin/dev..HEAD` shows them |
| `cargo fmt --all --check` | clean |
| CI total runtime | ≤17 min (cache hit) |
| All CI jobs green | `gh run view <run-id>` conclusion=success on every job |
| #45 post-deploy verified | manual repro on win-resolume shows title appears on scene-go-on |
| #46 info! logs visible | sp-server logs show `pipeline: Play received` etc. at info level |
| Issues closed | `gh issue list` no longer includes #45 or #46 |

If all checks pass, the work is done. Do **not** open a PR yet — the CI-perf commits from earlier today are also unmerged on dev; both ride together in the next dev→main PR. Wait for explicit user instruction to open the PR.

---

## Out of scope (recorded for future)

- Subtitle re-push on scene-go-on. Line-change hook auto-recovers on next sung word.
- Engine-layer Resume-before-Play. Pipeline already auto-clears.
- A `ScenePromoted` event the title task can listen to (instead of polling state in `handle_scene_change`). Worth doing if more "becomes-program needs to refresh X" surfaces appear.
