# Resolume Push Hardening — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the Resolume push chain from "works in the lab, silently dies in production" to "fails loud, self-heals, has visible state." Three independent tiers — visibility (info logs + dashboard health card), auto-recovery (re-emit current line on Resolume reconnect), fail-loud (clip-presence asserts + circuit breaker).

**Architecture:** Modifications to `resolume/driver.rs` + `resolume/mod.rs` for state tracking and RecoveryEvent. New `playback/mod.rs` subscriber that re-emits ShowTitle + ShowSubtitles on recovery. New `GET /api/v1/resolume/health` endpoint. New `<ResolumeHealthCard>` Leptos component on the dashboard. No new crates, no DB migrations.

**Tech Stack:** Rust 2024, tokio (broadcast channel for RecoveryEvent), Axum 0.8, Leptos 0.7, sqlx, tracing.

**Spec:** `docs/superpowers/specs/2026-04-26-resolume-push-hardening-design.md`

**Branch:** `dev`. Implementer never pushes — controller batches and pushes once at the end.

---

## Airuleset constraints (every implementer must follow)

- **TDD strict.** Write the failing test, watch it fail, implement, watch it pass, `cargo fmt --all --check`, commit on green. Never skip the fail step. For Rust tests that can't run locally, write the test first and trust by inspection.
- **Never run `cargo clippy/test/build` locally.** Only `cargo fmt --all --check`. Everything else on CI.
- **File size cap 1000 lines.** Current sizes: `resolume/driver.rs=804`, `resolume/handlers.rs=660`, `resolume/mod.rs=236`, `playback/mod.rs=942` (already tight!), `playback/lyrics_loader.rs=79`. **`playback/mod.rs` is at 942/1000 — Task 4 must add ≤55 lines or extract first.** Each task verifies line count before commit.
- **Commit on green.** One commit per task step that says "Commit". Implementer does NOT push.
- **`mutants::skip` requires a one-line justification.**
- **No emojis.**
- **No comments beyond what the plan provides.**

---

## Version bump (controller, before Task 1)

Today's date is 2026-04-26. PR #53 just merged 0.23.0 to main. Per airuleset version-bumping, dev must be strictly greater than main before any feature commits.

```bash
# On dev:
echo "0.24.0-dev.1" > VERSION
./scripts/sync-version.sh
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: bump dev to 0.24.0-dev.1"
```

This is controller-only (one-line workflow rule, no test needed).

---

## Task 1: Tier 1 — Promote lyrics-load logging

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs` (lines 388–404)

**Model:** haiku (mechanical log promotion at known sites)

The `Started` event handler already has three branches:
- `Ok(Some(...))` at line 388 — debug log on success
- `Ok(None)` at line 394 — silent (no lyrics file)
- `Err(e)` at line 397 — already warn

Change all three to be visible at info+/warn level so production debugging works.

- [ ] **Step 1: Read current state**

Read `crates/sp-server/src/playback/mod.rs` lines 365–405 to confirm the three branches. Do NOT touch line numbers outside this range in this task.

- [ ] **Step 2: Promote success branch to `info!` with line count + source + pipeline_version**

The `track` value (sp_core::lyrics::LyricsTrack) has `lines: Vec<...>`, `source: String`, `pipeline_version: u32` fields (verify via the type definition in `sp-core` if needed). Edit line 383–391 to:

```rust
                            Ok(Some((track, offset_ms))) => {
                                let lead_ms = lyrics_loader::load_lyrics_lead_ms(&pool).await;
                                let line_count = track.lines.len();
                                let source = track.source.clone();
                                let pipeline_version = track.pipeline_version;
                                pp.lyrics_state = Some(
                                    crate::lyrics::renderer::LyricsState::with_lead_and_offset(
                                        track, lead_ms, offset_ms,
                                    ),
                                );
                                info!(
                                    playlist_id,
                                    video_id,
                                    lines = line_count,
                                    source = %source,
                                    pipeline_version,
                                    lead_ms,
                                    offset_ms,
                                    "lyrics: loaded"
                                );
                            }
```

If the field names on `LyricsTrack` differ (e.g. `pipeline_version` is `lyrics_pipeline_version`), match the actual struct — verify by reading `crates/sp-core/src/lyrics.rs` (or wherever `LyricsTrack` is defined).

- [ ] **Step 3: Promote no-lyrics branch to `info!`**

The `Ok(None)` branch silently sets `lyrics_state = None`. Add an info log so the operator can see "this song has no lyrics" in production:

```rust
                            Ok(None) => {
                                pp.lyrics_state = None;
                                info!(playlist_id, video_id, "lyrics: no track available — wall will show no subtitles");
                                self.clear_lyrics_display(playlist_id);
                            }
```

- [ ] **Step 4: Keep error branch as warn (already correct)**

The `Err(e)` branch at line 397 already uses `warn!`. Verify, do not change.

- [ ] **Step 5: `cargo fmt --all --check`**

If rejected, run `cargo fmt --all` and re-check.

- [ ] **Step 6: Verify line count**

`wc -l crates/sp-server/src/playback/mod.rs` — must remain < 1000. The change adds ~10 lines.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/playback/mod.rs
git commit -m "feat(logging): promote lyrics-load to info!/warn! for production visibility (T1)"
```

---

## Task 2: Tier 1 — Promote ShowSubtitles dispatch + clips_for_subs miss to info!/warn!

**Files:**
- Modify: `crates/sp-server/src/resolume/handlers.rs`
- Modify: `crates/sp-server/src/playback/mod.rs` (find the line-tick dispatcher)

**Model:** haiku

- [ ] **Step 1: Find the ShowSubtitles dispatch site in `playback/mod.rs`**

`grep -nE "ShowSubtitles|broadcast.*line|ResolumeCommand::Show" crates/sp-server/src/playback/mod.rs` to locate. Note the line number; expect a tokio::sync::mpsc send site inside the line-tick path. Read 5 lines of context around it.

- [ ] **Step 2: Add an `info!` immediately before each `ResolumeCommand::ShowSubtitles` dispatch**

For each site that sends `ResolumeCommand::ShowSubtitles { ... }`, add immediately before:

```rust
info!(
    playlist_id,
    line_idx,
    text = %line_text_preview,  // use the same text being sent — a snippet (max 60 chars) is fine
    "ShowSubtitles dispatched"
);
```

If multiple ShowSubtitles dispatch sites exist, add the log at each. Keep field naming consistent.

- [ ] **Step 3: Promote `clips_for_subs` cache miss in `handlers.rs` to `warn!`**

Find every site in `crates/sp-server/src/resolume/handlers.rs` that logs `no Resolume subtitle clips found, skipping`. Today these are `debug!` (we saw 13 of them in the production log silenced). Promote to `warn!` — when this fires, the wall is dark for subtitles.

```rust
// before:
debug!(
    subs_token = "#sp-subs",
    subs_sk_token = "#sp-subssk",
    "no Resolume subtitle clips found, skipping clear_subtitles"
);
// after:
warn!(
    subs_token = "#sp-subs",
    subs_sk_token = "#sp-subssk",
    "no Resolume subtitle clips found — wall is dark for subtitles, skipping push"
);
```

Repeat for each clip-skip site (`clear_subtitles`, `show_subtitles`, etc. — find all via grep).

- [ ] **Step 4: `cargo fmt --all --check` and verify line counts**

`wc -l crates/sp-server/src/resolume/handlers.rs crates/sp-server/src/playback/mod.rs` — both under 1000.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/resolume/handlers.rs crates/sp-server/src/playback/mod.rs
git commit -m "feat(logging): promote subtitle dispatch + cache-miss to info!/warn! (T1)"
```

---

## Task 3: Tier 1+3 — Health snapshot + clip-presence assertion in driver.rs

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`

**Model:** sonnet (state tracking + assertion logic, more nuanced)

- [ ] **Step 1: Read existing driver state**

Read `crates/sp-server/src/resolume/driver.rs` end-to-end (804 lines). Note:
- `HostDriver` struct fields (line 75)
- `refresh_mapping` async method (line 191)
- The `if new_mapping != self.clip_mapping { ... self.clip_mapping = new_mapping }` block at line 199–207

- [ ] **Step 2: Add health snapshot fields to HostDriver**

Add to `HostDriver` struct:

```rust
    /// Set true after a successful refresh, false after a failure.
    pub(crate) last_refresh_ok: bool,
    /// Wall-clock timestamp of the last completed refresh attempt
    /// (success or failure). `None` until first attempt.
    pub(crate) last_refresh_ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Number of consecutive refresh failures. Reset to 0 on success.
    pub(crate) consecutive_failures: u32,
    /// Whether the circuit breaker has tripped (≥30s of failures).
    pub(crate) circuit_breaker_open: bool,
```

If `chrono` isn't already a workspace dep, use `std::time::SystemTime` instead and serialize as Unix epoch ms. Check `Cargo.toml` for chrono presence first.

Initialize in `HostDriver::new`:
```rust
            last_refresh_ok: false,
            last_refresh_ts: None,
            consecutive_failures: 0,
            circuit_breaker_open: false,
```

- [ ] **Step 3: Update `refresh_mapping` to set the snapshot fields**

In `refresh_mapping`, on the OK path (after successful HTTP fetch + parse):

```rust
        self.last_refresh_ok = true;
        self.last_refresh_ts = Some(chrono::Utc::now());
        let was_failing = self.consecutive_failures > 0;
        self.consecutive_failures = 0;
        if self.circuit_breaker_open {
            self.circuit_breaker_open = false;
            info!(host = %self.host, "circuit breaker closed — Resolume recovered");
        }
        // (existing `if new_mapping != self.clip_mapping { ... }` block stays)
```

On the Err path (where the function returns Err, or where `clip mapping refresh failed` is logged):

```rust
        self.last_refresh_ok = false;
        self.last_refresh_ts = Some(chrono::Utc::now());
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= 2 {
            warn!(
                host = %self.host,
                consecutive_failures = self.consecutive_failures,
                "Resolume refresh failing repeatedly"
            );
        }
        // 30s threshold (3 failures at 10s interval) → circuit open
        if self.consecutive_failures >= 3 && !self.circuit_breaker_open {
            self.circuit_breaker_open = true;
            self.clip_mapping = HashMap::new();
            warn!(host = %self.host, "circuit breaker opened — clip cache evicted");
        }
```

(Adjust threshold constants to defined consts for clarity: `const FAIL_WARN_THRESHOLD: u32 = 2; const CIRCUIT_OPEN_THRESHOLD: u32 = 3;`)

- [ ] **Step 4: Write failing test for circuit-breaker eviction**

Add to driver.rs's existing `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn circuit_breaker_evicts_clip_map_after_threshold_failures() {
        // wiremock server that returns 503 every time
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let port = server.address().port();
        let mut driver = HostDriver::new("127.0.0.1".into(), port);
        // Pretend the cache has clips from a prior successful refresh
        driver.clip_mapping.insert("#sp-title".into(), vec![]);

        // Three consecutive failures should trip the breaker and evict
        for _ in 0..3 {
            let _ = driver.refresh_mapping().await;
        }

        assert!(driver.circuit_breaker_open, "circuit should be open");
        assert!(driver.clip_mapping.is_empty(), "cache should be evicted");
    }

    #[tokio::test]
    async fn single_failure_does_not_trip_circuit() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Subsequent requests should 404 by default; we only test the first failure
        let port = server.address().port();
        let mut driver = HostDriver::new("127.0.0.1".into(), port);

        let _ = driver.refresh_mapping().await;

        assert_eq!(driver.consecutive_failures, 1);
        assert!(!driver.circuit_breaker_open);
    }
```

If `wiremock` isn't already a dev-dependency on `sp-server`, check Cargo.toml. If not, this test plan needs a different mocking approach (e.g. injectable HTTP client trait) — flag that as a blocker and stop.

- [ ] **Step 5: `cargo fmt --all --check` + verify line counts**

`wc -l crates/sp-server/src/resolume/driver.rs` — under 1000.

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs
git commit -m "feat(resolume): circuit breaker evicts clip cache after 30s of failures (T3)"
```

---

## Task 4: Tier 2 — RecoveryEvent broadcast channel

**Files:**
- Modify: `crates/sp-server/src/resolume/driver.rs`
- Modify: `crates/sp-server/src/resolume/mod.rs`

**Model:** sonnet

- [ ] **Step 1: Define RecoveryEvent in `resolume/mod.rs`**

```rust
/// Fired by [`HostDriver`] when a refresh succeeds after at least one
/// prior consecutive failure. Subscribers (e.g. the playback engine)
/// react by re-emitting their current state to the recovered host.
#[derive(Debug, Clone)]
pub struct RecoveryEvent {
    pub host: String,
}
```

Export it: `pub use RecoveryEvent;` if needed via `mod.rs` re-export pattern.

- [ ] **Step 2: Add a broadcast sender field to HostDriver**

Add to `HostDriver` struct:

```rust
    pub(crate) recovery_tx: Option<tokio::sync::broadcast::Sender<RecoveryEvent>>,
```

Initialize as `None` in `HostDriver::new`. Add a setter:

```rust
    pub fn with_recovery_channel(mut self, tx: tokio::sync::broadcast::Sender<RecoveryEvent>) -> Self {
        self.recovery_tx = Some(tx);
        self
    }
```

- [ ] **Step 3: Fire RecoveryEvent on success-after-failure**

In `refresh_mapping`'s OK path, after the `was_failing` flag is computed:

```rust
        if was_failing {
            if let Some(tx) = &self.recovery_tx {
                let _ = tx.send(RecoveryEvent { host: self.host.clone() });
            }
            info!(host = %self.host, "Resolume recovery — RecoveryEvent fired");
        }
```

- [ ] **Step 4: Wire the broadcast channel at registry construction in `resolume/mod.rs`**

Find where `HostDriver::new` is called inside the Resolume registry/worker setup. Create a single `broadcast::channel::<RecoveryEvent>(16)` shared across all hosts (the engine subscribes once and reacts to recovery on any host). Pass the `tx` into each driver via `.with_recovery_channel(tx.clone())`.

Expose the `Receiver` (or a `Sender` reference for late subscribe) so the engine can subscribe at startup. Pattern:

```rust
pub struct ResolumeRegistry {
    // ...existing fields...
    recovery_tx: tokio::sync::broadcast::Sender<RecoveryEvent>,
}

impl ResolumeRegistry {
    pub fn subscribe_recovery(&self) -> tokio::sync::broadcast::Receiver<RecoveryEvent> {
        self.recovery_tx.subscribe()
    }
}
```

- [ ] **Step 5: Write failing test for RecoveryEvent emission**

```rust
    #[tokio::test]
    async fn recovery_event_fires_on_success_after_failure() {
        let server = wiremock::MockServer::start().await;
        // First request fails, subsequent succeed
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
            .mount(&server)
            .await;
        let port = server.address().port();

        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let mut driver = HostDriver::new("127.0.0.1".into(), port).with_recovery_channel(tx);

        let _ = driver.refresh_mapping().await; // fails
        let _ = driver.refresh_mapping().await; // succeeds → RecoveryEvent

        let event = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            rx.recv(),
        ).await.expect("RecoveryEvent should arrive").expect("channel open");
        assert_eq!(event.host, "127.0.0.1");
    }

    #[tokio::test]
    async fn no_recovery_event_on_clean_first_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"layers": []})))
            .mount(&server)
            .await;
        let port = server.address().port();
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let mut driver = HostDriver::new("127.0.0.1".into(), port).with_recovery_channel(tx);

        let _ = driver.refresh_mapping().await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(result.is_err(), "no event should fire on clean first success");
    }
```

- [ ] **Step 6: `cargo fmt --all --check` + line count check**

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/resolume/driver.rs crates/sp-server/src/resolume/mod.rs
git commit -m "feat(resolume): RecoveryEvent broadcast on success-after-failure (T2)"
```

---

## Task 5: Tier 2 — Engine re-emit on RecoveryEvent

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs`

**Model:** sonnet

**File-size constraint:** `mod.rs` is at 942 lines after Task 1+2's promotions. This task adds ~30 lines. Verify after edit.

- [ ] **Step 1: Audit `lyrics_state` reload on Started**

Read `crates/sp-server/src/playback/mod.rs` lines 367–405 (the Started handler). Confirm that `pp.lyrics_state` is **always** set (either to `Some(...)` or `None`) in every match arm. Today inspection shows yes — Ok(Some), Ok(None), Err all assign — so the spec's "lyrics-state reload guarantee" is already satisfied.

- [ ] **Step 2: Add a regression test pinning the reload**

Add to `crates/sp-server/src/playback/tests.rs`:

```rust
#[tokio::test]
async fn started_event_unconditionally_resets_lyrics_state() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query("INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) VALUES (7, 'p', 'u', 'SP-fast', 1)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, song, artist, has_lyrics, normalized) VALUES (1, 7, 'no_lyrics_id', 'Song', 'Artist', 0, 1)")
        .execute(&pool).await.unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool, std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx, None, resolume_tx, ws_tx, None,
    );
    engine.ensure_pipeline(7, "SP-fast");
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.current_video_id = Some(1);
        // Pre-set a stale lyrics_state to verify Started clears it
        // (use a default-constructed dummy if LyricsState has a public ctor;
        //  otherwise leave None — point of test is the post-Started value)
    }

    engine.handle_pipeline_event(7, PipelineEvent::Started { duration_ms: 60_000 }).await;

    let pp = engine.pipelines.get(&7).unwrap();
    // has_lyrics=0 → Ok(None) branch → lyrics_state must be None
    assert!(pp.lyrics_state.is_none(),
        "Started event with has_lyrics=0 must set lyrics_state=None, not preserve stale state");
}
```

- [ ] **Step 3: Wire RecoveryEvent subscription at engine startup**

The engine struct (`PlaybackEngine`) needs to spawn a task at construction-time that subscribes to the Resolume registry's RecoveryEvent broadcast and triggers re-emit. The simplest pattern: in the engine's main run loop (wherever `handle_*` events are dispatched in a `select!`), add a new arm that handles incoming RecoveryEvent.

Find the engine's main event loop (grep for `select!` in `playback/mod.rs` or its run/spawn site). Add:

```rust
                Some(recovery_event) = recovery_rx.recv() => {
                    self.handle_resolume_recovery(&recovery_event.host).await;
                }
```

The plumbing: pass a `broadcast::Receiver<RecoveryEvent>` into the engine's `new()` (or its run-loop entry). Wire it up at startup in `crates/sp-server/src/lib.rs` where the engine is constructed alongside the Resolume registry.

If the engine doesn't currently have a `select!`-based main loop and instead uses a Drop-style command pattern, the simplest alternative is to spawn a background task in `lib.rs::start()` that subscribes to the recovery channel and posts an `EngineCommand::ResolumeRecovered { host }` to the engine. Use whichever pattern matches existing code — do not invent a new orchestration pattern.

- [ ] **Step 4: Implement `handle_resolume_recovery`**

Add to `impl PlaybackEngine`:

```rust
    /// Re-emit current state to a recovered Resolume host: ShowTitle for
    /// every active playlist + ShowSubtitles for the current line.
    async fn handle_resolume_recovery(&self, host: &str) {
        info!(host, "Resolume recovery — re-emitting current state for active pipelines");
        for (&playlist_id, pp) in &self.pipelines {
            let PlayState::Playing { video_id } = pp.state else { continue };
            if !pp.scene_active.load(Ordering::Acquire) { continue; }
            // Re-push title (idempotent)
            if title::push_title(
                &self.pool, self.obs_cmd_tx.as_ref(), &self.resolume_tx, video_id,
            ).await {
                info!(playlist_id, video_id, "title re-pushed on Resolume recovery");
            }
            // Re-push current subtitle line if lyrics_state has one
            if let Some(state) = &pp.lyrics_state {
                if let Some(line) = state.current_line() {
                    let _ = self.resolume_tx.send(
                        crate::resolume::ResolumeCommand::ShowSubtitles {
                            text_en: line.text_en.clone(),
                            text_sk: line.text_sk.clone().unwrap_or_default(),
                            text_next_en: state.next_line().map(|n| n.text_en.clone()).unwrap_or_default(),
                            suppress_en: pp.cached_suppress_en,
                        }
                    ).await;
                    info!(playlist_id, video_id, line_idx = state.current_line_idx(), "subtitle re-pushed on Resolume recovery");
                }
            }
        }
    }
```

(Field names of `LyricsState` may differ — verify against `crate::lyrics::renderer::LyricsState`. Adjust accessor calls to match.)

- [ ] **Step 5: Write failing test**

In `crates/sp-server/src/playback/tests_scene_change.rs` (or a new sibling), add:

```rust
#[tokio::test]
async fn handle_resolume_recovery_reemits_title_for_active_pipeline() {
    use std::sync::atomic::Ordering;

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) VALUES (7, 'p', 'u', 'SP-fast', 1)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized) VALUES (42, 7, 'abc', 'Song', 'Artist', 1)").execute(&pool).await.unwrap();

    let (obs_tx, _obs_rx) = broadcast::channel(16);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(pool, std::path::PathBuf::from("/tmp/test-cache"), obs_tx, None, resolume_tx, ws_tx, None);
    engine.ensure_pipeline(7, "SP-fast");
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.state = PlayState::Playing { video_id: 42 };
        pp.scene_active.store(true, Ordering::Release);
    }
    while resolume_rx.try_recv().is_ok() {}

    engine.handle_resolume_recovery("127.0.0.1").await;

    // Expect at least one ShowTitle on the resolume_rx within 200ms
    let mut got_title = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(cmd)) = tokio::time::timeout(std::time::Duration::from_millis(20), resolume_rx.recv()).await {
            if matches!(cmd, crate::resolume::ResolumeCommand::ShowTitle { .. }) {
                got_title = true; break;
            }
        }
    }
    assert!(got_title, "ShowTitle must be re-emitted on Resolume recovery");
}
```

- [ ] **Step 6: `cargo fmt --all --check` + line counts**

`wc -l crates/sp-server/src/playback/mod.rs` — must be < 1000.

- [ ] **Step 7: Commit**

```bash
git add crates/sp-server/src/playback/mod.rs crates/sp-server/src/playback/tests.rs crates/sp-server/src/playback/tests_scene_change.rs
git commit -m "feat(playback): re-emit ShowTitle + ShowSubtitles on Resolume RecoveryEvent (T2)"
```

---

## Task 6: Tier 1 — `GET /api/v1/resolume/health` endpoint

**Files:**
- Modify: `crates/sp-server/src/api/routes.rs` (or a new `crates/sp-server/src/api/resolume.rs`)
- Modify: `crates/sp-server/src/api/mod.rs` (route registration)
- Modify: `crates/sp-server/src/resolume/mod.rs` (expose snapshot)

**Model:** sonnet

- [ ] **Step 1: Add a `health_snapshot()` method on the registry**

In `crates/sp-server/src/resolume/mod.rs`, add a method that walks all hosts and returns a Vec of per-host snapshots:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostHealthSnapshot {
    pub host: String,
    pub last_refresh_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub last_refresh_ok: bool,
    pub consecutive_failures: u32,
    pub circuit_breaker_open: bool,
    pub clips_by_token: std::collections::BTreeMap<String, usize>,
}

impl ResolumeRegistry {
    pub async fn health_snapshot(&self) -> Vec<HostHealthSnapshot> {
        // Walk hosts, lock each driver, copy fields
    }
}
```

The lock pattern depends on how drivers are stored today (likely `Arc<Mutex<HostDriver>>` in a HashMap). Match existing pattern.

- [ ] **Step 2: Add the route handler**

```rust
pub async fn get_resolume_health(
    State(state): State<AppState>,
) -> Json<Vec<HostHealthSnapshot>> {
    Json(state.resolume_registry.health_snapshot().await)
}
```

- [ ] **Step 3: Register the route in `api/mod.rs`**

Locate the existing `.route("/api/v1/resolume/hosts", ...)` registration around line 79–84. Add a sibling:

```rust
        .route("/api/v1/resolume/health", axum::routing::get(routes::get_resolume_health))
```

- [ ] **Step 4: Write a smoke test**

Add to `crates/sp-server/src/api/routes_tests.rs` (or wherever route tests live):

```rust
#[tokio::test]
async fn resolume_health_endpoint_returns_array() {
    let (state, _shutdown) = build_test_app_state().await; // existing helper if present
    let app = build_router(state);
    let req = Request::builder().uri("/api/v1/resolume/health").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_to_json(resp.into_body()).await;
    assert!(body.is_array(), "response must be a JSON array");
}
```

If the route-test scaffolding doesn't exist or the test helper is named differently, match the existing patterns in `routes_tests.rs`.

- [ ] **Step 5: `cargo fmt --all --check`**

- [ ] **Step 6: Commit**

```bash
git add crates/sp-server/src/api/routes.rs crates/sp-server/src/api/mod.rs crates/sp-server/src/resolume/mod.rs crates/sp-server/src/api/routes_tests.rs
git commit -m "feat(api): GET /api/v1/resolume/health returns per-host snapshot (T1)"
```

---

## Task 7: Tier 1 — Dashboard ResolumeHealthCard component

**Files:**
- New: `sp-ui/src/components/resolume_health.rs`
- Modify: `sp-ui/src/components/mod.rs` (export)
- Modify: `sp-ui/src/pages/dashboard.rs` (mount the card)

**Model:** sonnet

- [ ] **Step 1: Create the component**

```rust
//! Dashboard card showing Resolume push-chain health per host.
//! Polls /api/v1/resolume/health every 5s.

use leptos::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct HostHealth {
    pub host: String,
    pub last_refresh_ts: Option<String>,
    pub last_refresh_ok: bool,
    pub consecutive_failures: u32,
    pub circuit_breaker_open: bool,
    pub clips_by_token: std::collections::BTreeMap<String, usize>,
}

#[component]
pub fn ResolumeHealthCard() -> impl IntoView {
    let snapshot = RwSignal::new(Vec::<HostHealth>::new());

    // Poll every 5s
    set_interval(
        move || {
            spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/v1/resolume/health").send().await {
                    if let Ok(data) = resp.json::<Vec<HostHealth>>().await {
                        snapshot.set(data);
                    }
                }
            });
        },
        std::time::Duration::from_secs(5),
    );

    view! {
        <div class="resolume-health-card">
            <h3>"Resolume hosts"</h3>
            <For each=move || snapshot.get() key=|h| h.host.clone() let:host>
                <div class={if host.circuit_breaker_open {"host red"} else if host.consecutive_failures > 0 || host.clips_by_token.values().any(|&v| v == 0) {"host yellow"} else {"host green"}}>
                    <strong>{host.host.clone()}</strong>
                    <span class="ts">{host.last_refresh_ts.clone().unwrap_or_else(|| "never".into())}</span>
                    <For each=move || host.clips_by_token.clone().into_iter().collect::<Vec<_>>() key=|(k,_)| k.clone() let:entry>
                        <span class={if entry.1 == 0 {"token zero"} else {"token ok"}}>
                            {format!("{}={}", entry.0, entry.1)}
                        </span>
                    </For>
                </div>
            </For>
        </div>
    }
}
```

(Verify `gloo_net`, `set_interval`, `RwSignal` etc. are in the project's existing imports in sibling components like `obs_status.rs`. Match existing patterns.)

- [ ] **Step 2: Export from components/mod.rs**

```rust
pub mod resolume_health;
```

- [ ] **Step 3: Mount in dashboard.rs**

After the `<obs_status::ObsStatus />` line (line 27 today), add:

```rust
                <resolume_health::ResolumeHealthCard />
```

- [ ] **Step 4: Verify trunk build still works**

Per airuleset, only `cargo fmt --all --check` runs locally. Trust by inspection that the component compiles. CI's `Build WASM (trunk)` job will catch syntax issues.

- [ ] **Step 5: Commit**

```bash
git add sp-ui/src/components/resolume_health.rs sp-ui/src/components/mod.rs sp-ui/src/pages/dashboard.rs
git commit -m "feat(sp-ui): ResolumeHealthCard polls /api/v1/resolume/health every 5s (T1)"
```

---

## Task 8: Push + monitor CI (controller-only)

**Model:** controller (you, not a subagent — git push + gh run view)

- [ ] **Step 1: Pre-push sanity**

```bash
git fetch origin
git status
git log --oneline origin/dev..HEAD
```

Expected: 8 commits on `dev` ahead of `origin/dev` (one bump + 7 task commits).

- [ ] **Step 2: `cargo fmt --all --check`**

Expected: clean exit.

- [ ] **Step 3: Push**

```bash
git push origin dev
```

- [ ] **Step 4: Monitor CI to terminal state**

Per airuleset/ci-monitoring: single `Bash(command: "sleep N && gh run view <run-id> --json status,conclusion,jobs", run_in_background: true)` poll. Steady-state CI is ~17 min. If `Deploy to win-resolume` queued >2 min, ping win-resolume and report.

- [ ] **Step 5: Verify all jobs green**

If any job fails, investigate `gh run view <run-id> --log-failed` and fix in one batched commit.

- [ ] **Step 6: Manual post-deploy verification on win-resolume**

The actual repro of the live-event failure mode:

1. Verify dashboard ResolumeHealthCard renders, shows green for 127.0.0.1, all four tokens (`#sp-title`, `#sp-subs`, `#sp-subs-next`, `#sp-subssk`) populated.
2. Tail the SongPlayer log. Verify `info! lyrics: loaded video_id=X lines=N source=Y pipeline_version=Z` appears for the currently playing song.
3. Kill Arena.exe via win-resolume MCP. Watch logs:
   - T+10s: `debug! clip mapping refresh failed`
   - T+20s: `warn! Resolume refresh failing repeatedly consecutive_failures=2`
   - T+30s: `warn! circuit breaker opened — clip cache evicted`
4. Dashboard ResolumeHealthCard goes red. `circuit_breaker_open=true`.
5. Restart Arena.
6. Within 15s of Arena boot, verify:
   - `info! Resolume recovery — RecoveryEvent fired`
   - `info! Resolume recovery — re-emitting current state for active pipelines`
   - `info! title re-pushed on Resolume recovery`
   - `info! subtitle re-pushed on Resolume recovery line_idx=...`
   - `info! sent SetTextSource to OBS`
   - `info! set title text on all clips count=N`
   - `info! set subtitle text on all clips count=M`
7. Wall shows title + lyrics again WITHOUT restarting SongPlayer.
8. Dashboard ResolumeHealthCard goes green.

If any verification step fails, fix and re-push. Do NOT declare done.

- [ ] **Step 7: Open the PR**

After CI green and post-deploy verified:

```bash
# Bump VERSION 0.24.0-dev.1 → 0.24.0 for release
echo "0.24.0" > VERSION
./scripts/sync-version.sh
git add VERSION Cargo.toml sp-ui/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: release 0.24.0"
git push origin dev
# Wait for CI green again, then:
gh pr create --base main --head dev --title "0.24.0: Resolume push hardening (visibility + auto-recovery + fail-loud)" --body "..."
```

PR description summarizes: T1 visibility (info logs, /api/v1/resolume/health, ResolumeHealthCard), T2 auto-recovery (RecoveryEvent + engine re-emit), T3 fail-loud (circuit breaker + clip-presence asserts). References the live-event diagnosis from 2026-04-26.

Wait for explicit "merge it" before merging.

---

## Verification (controller-only, after PR merge)

| Check | Expected |
|---|---|
| Main CI green | All 14 jobs success |
| Deploy to win-resolume | success |
| ResolumeHealthCard renders on dashboard | green for 127.0.0.1, all tokens >0 |
| Manual Arena-kill drill works | recovery within 15s, no SongPlayer restart needed |
| Lyrics-load info log appears for currently playing song | `info! lyrics: loaded ...` visible in production |

---

## Out of scope (future specs)

- T4 CI drill (automated Arena-restart simulation) — needs CI runner with Arena installable. Separate spec.
- A/V codec hardening (AV1 avoidance) — separate sub-project per the brainstorming decomposition.
- Auto-trigger #sp-subs clips — changes contract with operator, deferred.
- Presenter / OBS health endpoints — same architecture pattern, deferred to keep scope tight.
