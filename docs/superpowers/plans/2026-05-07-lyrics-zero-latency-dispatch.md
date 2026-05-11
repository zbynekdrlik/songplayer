# Zero-Latency Lyrics Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decouple lyrics line dispatch (Resolume `ShowSubtitles` + Presenter push + karaoke WebSocket) from the 500 ms `NowPlaying` position-broadcast throttle so line changes reach the LED wall and the Presenter stage display within one pipeline Position event tick (~33 ms).

**Architecture:** Split `playback/position_update.rs` into two functions called by every `PipelineEvent::Position`. `dispatch_lyrics_if_changed(playlist_id, position_ms)` runs first (no throttle, fires on every event when current/next line changed vs per-pipeline snapshot). `maybe_broadcast_position_update(playlist_id, position_ms, duration_ms)` runs second (existing 500 ms throttle, NowPlaying ws only). Add two snapshot fields to `PerPlaylistPipeline` so the dispatch is idempotent at the same line.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), tokio 1, axum 0.8, broadcast / mpsc channels, sp_core::ws::ServerMsg.

**Spec:** [`docs/superpowers/specs/2026-05-07-lyrics-zero-latency-dispatch-design.md`](../specs/2026-05-07-lyrics-zero-latency-dispatch-design.md) — commit `8bebbfa`.

---

## Per-implementer airuleset rules (verbatim)

- TDD strict: failing test first → trust by inspection → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- NEVER run `cargo clippy / test / build / check` locally; rely on CI.
- File-size cap 1000 lines per file.
- One commit per "Commit" step in this plan body.
- `mutants::skip` requires inline justification (one-line `// mutants::skip: <reason>` immediately above the attribute).
- Do NOT push — controller batches and pushes once at the end of the phase.
- Per `feedback_no_legacy_code.md`: when replacing a code path, delete the old one entirely. The lyrics dispatch logic is REMOVED from `maybe_broadcast_position_update`, not duplicated.
- Per `feedback_pipeline_version_approval.md`: do NOT bump `LYRICS_PIPELINE_VERSION`. Constant stays at `20`.
- Per `feedback_line_timing_only.md`: every output line ships `words: None`. (Already true upstream — this plan does not touch the renderer.)
- Per `feedback_take_ownership.md`: root-cause fix only. The fix lives in `position_update.rs`, not in a downstream consumer.

## File structure

```
crates/sp-server/src/playback/
├── mod.rs                               # MODIFY — add 2 fields to PerPlaylistPipeline; add 2 field-init sites; add 2 reset sites in clear_lyrics_display call sites; add dispatch_lyrics_if_changed call before maybe_broadcast_position_update in PipelineEvent::Position arm
├── position_update.rs                   # MODIFY — split into two methods; remove lyrics block from maybe_broadcast_position_update; add dispatch_lyrics_if_changed
├── dispatch_lyrics_tests.rs             # NEW — 6 unit tests for dispatch_lyrics_if_changed
└── tests.rs                             # MODIFY — update PlaylistPipeline struct-init sites to include the 2 new fields
```

`presenter/mod.rs` and `presenter/client.rs` are unchanged. `resolume/` is unchanged. `lyrics/renderer.rs` is unchanged.

---

## Task A.1 — Decouple lyrics dispatch from NowPlaying throttle

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs` (add fields + init + reset + caller wiring)
- Modify: `crates/sp-server/src/playback/position_update.rs` (split into two methods)
- Modify: `crates/sp-server/src/playback/tests.rs` (update existing struct-init test)
- Create: `crates/sp-server/src/playback/dispatch_lyrics_tests.rs` (6 unit tests)

### Step 1: Add the failing tests (TDD red)

Create `crates/sp-server/src/playback/dispatch_lyrics_tests.rs`:

```rust
//! Tests for `PlaybackEngine::dispatch_lyrics_if_changed`.
//! Sibling-included from playback/mod.rs (see `#[cfg(test)] mod` declaration).

#![allow(unused_imports)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::lyrics::renderer::LyricsState;
use crate::playback::pipeline::PlaybackPipeline;
use crate::playback::ndi_health::NdiHealthRegistry;

fn make_track() -> LyricsTrack {
    LyricsTrack {
        version: 20,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![
            LyricsLine {
                start_ms: 1000,
                end_ms: 3000,
                en: "alpha".into(),
                sk: Some("alfa".into()),
                words: None,
            },
            LyricsLine {
                start_ms: 4000,
                end_ms: 6000,
                en: "beta".into(),
                sk: Some("beta".into()),
                words: None,
            },
            LyricsLine {
                start_ms: 7000,
                end_ms: 9000,
                en: "gamma".into(),
                sk: Some("gama".into()),
                words: None,
            },
        ],
    }
}

fn build_engine() -> (
    PlaybackEngine,
    mpsc::Receiver<crate::resolume::ResolumeCommand>,
    broadcast::Receiver<ServerMsg>,
) {
    let pool = futures::executor::block_on(async {
        let p = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&p).await.unwrap();
        p
    });
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, resolume_rx) = mpsc::channel(16);
    let (ws_tx, ws_rx) = broadcast::channel::<ServerMsg>(16);
    let engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None, // presenter_client = None: tests don't assert presenter HTTP push.
        Arc::new(NdiHealthRegistry::new()),
    );
    (engine, resolume_rx, ws_rx)
}

fn install_pipeline(engine: &mut PlaybackEngine, playlist_id: i64, scene_active: bool, lyrics: Option<LyricsState>) {
    let pipeline = PlaybackPipeline::spawn(
        format!("test-{playlist_id}"),
        None,
        mpsc::unbounded_channel().0,
        playlist_id,
    );
    let pp = PlaylistPipeline {
        pipeline,
        state: PlayState::Idle,
        mode: PlaybackMode::default(),
        current_video_id: Some(42),
        scene_active: Arc::new(AtomicBool::new(scene_active)),
        title_show_abort: None,
        title_hide_abort: None,
        cached_song: "Song".into(),
        cached_artist: "Artist".into(),
        cached_duration_ms: 10_000,
        cached_suppress_en: false,
        last_now_playing_broadcast: None,
        history: VecDeque::new(),
        lyrics_state: lyrics,
        last_presenter_text: None,
        last_resolume_subtitles_signature: None,
        last_lyrics_ws_signature: None,
        cached_position_ms: 0,
    };
    engine.pipelines.insert(playlist_id, pp);
}

#[tokio::test]
async fn dispatch_lyrics_skips_when_no_lyrics_state() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, None);

    engine.dispatch_lyrics_if_changed(99, 1500);

    assert!(
        resolume_rx.try_recv().is_err(),
        "no lyrics_state → no Resolume command"
    );
    assert!(
        ws_rx.try_recv().is_err(),
        "no lyrics_state → no ws message"
    );
}

#[tokio::test]
async fn dispatch_lyrics_fires_on_first_position_event() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    engine.dispatch_lyrics_if_changed(99, 1500); // inside line "alpha" 1000..3000

    let cmd = resolume_rx
        .try_recv()
        .expect("Resolume ShowSubtitles must fire on first event");
    match cmd {
        crate::resolume::ResolumeCommand::ShowSubtitles { en, .. } => {
            assert!(en.contains("alpha"), "got: {en}");
        }
        other => panic!("expected ShowSubtitles, got {other:?}"),
    }

    let msg = ws_rx
        .try_recv()
        .expect("ws LyricsUpdate must fire on first event");
    match msg {
        ServerMsg::LyricsUpdate { line_en, playlist_id, .. } => {
            assert_eq!(playlist_id, 99);
            assert_eq!(line_en.as_deref(), Some("alpha"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }

    let pp = engine.pipelines.get(&99).unwrap();
    assert!(pp.last_resolume_subtitles_signature.is_some());
    assert_eq!(pp.last_lyrics_ws_signature.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn dispatch_lyrics_idempotent_on_same_line() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // First call inside "alpha" line: fires.
    engine.dispatch_lyrics_if_changed(99, 1500);
    let _first_resolume = resolume_rx.try_recv().expect("first call fires Resolume");
    let _first_ws = ws_rx.try_recv().expect("first call fires ws");

    // Second call still inside "alpha": NO fires.
    engine.dispatch_lyrics_if_changed(99, 2200);
    assert!(
        resolume_rx.try_recv().is_err(),
        "same line → no second Resolume command"
    );
    assert!(
        ws_rx.try_recv().is_err(),
        "same line → no second ws message"
    );
}

#[tokio::test]
async fn dispatch_lyrics_fires_on_line_change() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // First inside "alpha" 1000..3000.
    engine.dispatch_lyrics_if_changed(99, 1500);
    let _ = resolume_rx.try_recv().unwrap();
    let _ = ws_rx.try_recv().unwrap();

    // Second inside "beta" 4000..6000.
    engine.dispatch_lyrics_if_changed(99, 4500);

    let cmd = resolume_rx
        .try_recv()
        .expect("line change → second Resolume command");
    match cmd {
        crate::resolume::ResolumeCommand::ShowSubtitles { en, .. } => {
            assert!(en.contains("beta"), "got: {en}");
        }
        other => panic!("expected ShowSubtitles, got {other:?}"),
    }
    let msg = ws_rx.try_recv().expect("line change → second ws message");
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => {
            assert_eq!(line_en.as_deref(), Some("beta"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_lyrics_resolume_gated_on_scene_active() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(
        &mut engine,
        99,
        false, // scene_active = false: Resolume must NOT fire
        Some(LyricsState::new(make_track())),
    );

    engine.dispatch_lyrics_if_changed(99, 1500);

    assert!(
        resolume_rx.try_recv().is_err(),
        "scene_active=false → no Resolume command"
    );
    let msg = ws_rx
        .try_recv()
        .expect("ws LyricsUpdate must still fire when scene_active=false");
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => {
            assert_eq!(line_en.as_deref(), Some("alpha"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_lyrics_no_throttle() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // Two events 100 ms apart on DIFFERENT lines must both fire — proves the
    // 500 ms position-update throttle does NOT gate this dispatch path.
    engine.dispatch_lyrics_if_changed(99, 1500); // alpha
    let _ = resolume_rx.try_recv().unwrap();
    let _ = ws_rx.try_recv().unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    engine.dispatch_lyrics_if_changed(99, 4500); // beta

    let cmd = resolume_rx
        .try_recv()
        .expect("second event must fire 100 ms later — no throttle");
    matches!(cmd, crate::resolume::ResolumeCommand::ShowSubtitles { .. });
    let _ = ws_rx
        .try_recv()
        .expect("second ws message must fire 100 ms later — no throttle");
}
```

### Step 2: Verify the tests fail to compile (TDD red)

Trust by inspection:

- `PlaylistPipeline { ..., last_resolume_subtitles_signature: None, last_lyrics_ws_signature: None, ... }` — these two fields don't exist on `PlaylistPipeline` yet (Step 4 adds them).
- `engine.dispatch_lyrics_if_changed(99, 1500)` — method doesn't exist yet (Step 6 adds it).
- `pp.last_resolume_subtitles_signature` / `pp.last_lyrics_ws_signature` accessor reads in Step-1 test bodies — fail to resolve.

CI will report compile errors here. That's the expected red state.

### Step 3: Add the test-mod declaration

Edit `crates/sp-server/src/playback/mod.rs`. Find the existing test-mod declarations near the bottom of the file (`grep -n '#\[cfg(test)\]' crates/sp-server/src/playback/mod.rs`). Append:

```rust
#[cfg(test)]
#[path = "dispatch_lyrics_tests.rs"]
mod dispatch_lyrics_tests;
```

### Step 4: Add the two new fields to `PlaylistPipeline`

Edit `crates/sp-server/src/playback/mod.rs`. In the `PlaylistPipeline` struct (around lines 71–109), insert the new fields immediately after `last_presenter_text: Option<String>,` (around line 103):

```rust
    /// Snapshot of the last Resolume `ShowSubtitles` payload signature (or
    /// `HideSubtitles`-equivalent) we sent. Used by
    /// `dispatch_lyrics_if_changed` to skip duplicate dispatches at every
    /// Position event tick. Cleared on song change so the next song's
    /// first line fires correctly.
    last_resolume_subtitles_signature: Option<String>,
    /// Snapshot of the last karaoke WebSocket lyrics-update line text
    /// (`line_en`) we sent. Used by `dispatch_lyrics_if_changed` to skip
    /// duplicate dispatches. Cleared on song change.
    last_lyrics_ws_signature: Option<String>,
```

### Step 5: Update field-init sites

There are two struct-init sites to update.

**Site 1 — production init** in `crates/sp-server/src/playback/mod.rs` (around line 222–245, search for `last_presenter_text: None,` to locate). Add the two new fields immediately after it:

```rust
                last_presenter_text: None,
                last_resolume_subtitles_signature: None,
                last_lyrics_ws_signature: None,
```

**Site 2 — test init** in `crates/sp-server/src/playback/tests.rs` (around line 142, search for `last_presenter_text: None,`). Add the two new fields immediately after it:

```rust
            last_presenter_text: None,
            last_resolume_subtitles_signature: None,
            last_lyrics_ws_signature: None,
```

(The Step-1 test file already populates the two fields in its own `install_pipeline` helper; don't double-add.)

### Step 6: Reset the snapshots on song change

Edit `crates/sp-server/src/playback/mod.rs`. Find the existing `pp.last_presenter_text = None;` site (around line 700, in the `SelectAndPlay` / new-video handler — locate via `grep -n 'last_presenter_text = None' crates/sp-server/src/playback/mod.rs`). Add the two new resets immediately after it:

```rust
            pp.last_presenter_text = None;
            pp.last_resolume_subtitles_signature = None;
            pp.last_lyrics_ws_signature = None;
```

### Step 7: Refactor `position_update.rs` — remove lyrics block from `maybe_broadcast_position_update`, add `dispatch_lyrics_if_changed`

Replace the entire contents of `crates/sp-server/src/playback/position_update.rs` with:

```rust
//! Per-pipeline-event broadcast helpers — extracted from `mod.rs` to keep
//! that file under the 1000-line cap. Two pure delegates:
//!
//! - [`PlaybackEngine::dispatch_lyrics_if_changed`] — event-driven lyrics
//!   dispatch. Fires Resolume `ShowSubtitles` + Presenter push + karaoke
//!   WebSocket on every Position event when the current/next line changed
//!   vs the per-pipeline snapshot. NO throttle — line latency on the LED
//!   wall is bounded by one Position event tick (~33 ms).
//! - [`PlaybackEngine::maybe_broadcast_position_update`] — periodic
//!   `NowPlaying` rebroadcast for the dashboard progress bar. Throttled
//!   to `POSITION_BROADCAST_INTERVAL_MS = 500`.
//!
//! Both are called by the `PipelineEvent::Position` handler in `mod.rs`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use sp_core::ws::ServerMsg;
use tracing::info;

use super::should_send_position_update;

impl super::PlaybackEngine {
    /// Lyrics fast path — no throttle. Computes current/next line, compares
    /// against the per-pipeline snapshots, and dispatches Resolume +
    /// Presenter + karaoke WebSocket only when the snapshot differs.
    ///
    /// Resolume dispatch is gated on `pp.scene_active` (off-program
    /// playlists must not clobber `#sp-subs`). Presenter + ws fire
    /// regardless of `scene_active`.
    pub(super) fn dispatch_lyrics_if_changed(&mut self, playlist_id: i64, position_ms: u64) {
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };
        let video_id = match pp.current_video_id {
            Some(id) => id,
            None => return,
        };

        // Karaoke WebSocket dedup keyed on the line's English text.
        let ws_msg = lyrics.update(playlist_id, position_ms);
        let ws_signature = ws_signature_from(&ws_msg);
        if pp.last_lyrics_ws_signature != ws_signature {
            let _ = self.ws_event_tx.send(ws_msg);
            // re-borrow pp because send() doesn't take &mut self but the
            // compiler may have invalidated; assign through the original
            // handle:
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.last_lyrics_ws_signature = ws_signature;
            }
        }

        // Re-fetch — the previous mutable borrow was dropped above.
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };

        // Resolume — gated on scene_active.
        if pp.scene_active.load(Ordering::Acquire) {
            let resolume_signature = match lyrics.resolume_lines_with_next(position_ms) {
                Some((en, next_en, sk, next_sk)) => {
                    let sig = format!(
                        "show|{}|{}|{}|{}|{}",
                        en,
                        next_en,
                        sk.as_deref().unwrap_or(""),
                        next_sk.as_deref().unwrap_or(""),
                        pp.cached_suppress_en,
                    );
                    if pp.last_resolume_subtitles_signature.as_deref() != Some(sig.as_str()) {
                        info!(
                            playlist_id,
                            video_id,
                            text = %en.lines().next().unwrap_or("").chars().take(60).collect::<String>(),
                            "ShowSubtitles dispatched"
                        );
                        let _ = self.resolume_tx.try_send(
                            crate::resolume::ResolumeCommand::ShowSubtitles {
                                en,
                                next_en,
                                sk,
                                next_sk,
                                suppress_en: pp.cached_suppress_en,
                            },
                        );
                        Some(sig)
                    } else {
                        // Same as last sent — no-op.
                        Some(sig)
                    }
                }
                None => {
                    let sig = "hide".to_string();
                    if pp.last_resolume_subtitles_signature.as_deref() != Some(sig.as_str()) {
                        let _ = self
                            .resolume_tx
                            .try_send(crate::resolume::ResolumeCommand::HideSubtitles);
                        Some(sig)
                    } else {
                        Some(sig)
                    }
                }
            };
            if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                pp.last_resolume_subtitles_signature = resolume_signature;
            }
        }

        // Presenter — fire-and-forget. `maybe_push_line` is idempotent on
        // identical `current_en` (compares against the `last_seen` arg we
        // pass, which is the pre-call snapshot).
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };
        let lyrics = match &pp.lyrics_state {
            Some(l) => l,
            None => return,
        };
        if let Some((cur, nxt)) = lyrics.presenter_lines(position_ms) {
            pp.last_presenter_text = crate::presenter::maybe_push_line(
                self.presenter_client.as_ref(),
                pp.last_presenter_text.take(),
                cur,
                nxt,
                &pp.cached_song,
                &pp.cached_artist,
            );
        }
    }

    /// Periodic dashboard `NowPlaying` rebroadcast. Throttled to
    /// `POSITION_BROADCAST_INTERVAL_MS = 500` so the progress bar doesn't
    /// flood the WebSocket on high-frequency Position events.
    pub(super) fn maybe_broadcast_position_update(
        &mut self,
        playlist_id: i64,
        position_ms: u64,
        duration_ms: u64,
    ) {
        let pp = match self.pipelines.get_mut(&playlist_id) {
            Some(pp) => pp,
            None => return,
        };

        let now = Instant::now();
        let should_send = match pp.last_now_playing_broadcast {
            Some(t) => should_send_position_update(now.duration_since(t).as_millis() as u64),
            None => true,
        };
        if !should_send {
            return;
        }
        pp.last_now_playing_broadcast = Some(now);

        let video_id = match pp.current_video_id {
            Some(id) => id,
            None => return,
        };
        let song = pp.cached_song.clone();
        let artist = pp.cached_artist.clone();
        let dur = if duration_ms > 0 {
            duration_ms
        } else {
            pp.cached_duration_ms
        };

        let _ = self.ws_event_tx.send(ServerMsg::NowPlaying {
            playlist_id,
            video_id,
            song,
            artist,
            position_ms,
            duration_ms: dur,
        });
    }
}

/// Build a stable signature for a [`ServerMsg::LyricsUpdate`] keyed on the
/// line's `line_en`. Returns `None` when the message has no line (between-
/// lines clear). The signature ignores `position_ms`-derived
/// `active_word_index` so we don't fire a fresh ws message every Position
/// tick within the same line.
fn ws_signature_from(msg: &ServerMsg) -> Option<String> {
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => line_en.clone(),
        _ => None,
    }
}
```

### Step 8: Run formatter

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

If diff: `cargo fmt --all` then re-run `--check`. Expected exit code 0.

### Step 9: Wire `dispatch_lyrics_if_changed` into the Position event handler

Edit `crates/sp-server/src/playback/mod.rs`. Find the Position event handler (around line 528–531; locate via `grep -n 'self.maybe_broadcast_position_update' crates/sp-server/src/playback/mod.rs`). The current code:

```rust
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cached_position_ms = *position_ms;
                }
                self.maybe_broadcast_position_update(playlist_id, *position_ms, *duration_ms);
```

Replace with:

```rust
                if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                    pp.cached_position_ms = *position_ms;
                }
                self.dispatch_lyrics_if_changed(playlist_id, *position_ms);
                self.maybe_broadcast_position_update(playlist_id, *position_ms, *duration_ms);
```

### Step 10: Run formatter again

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

Expected exit code 0. If diff: `cargo fmt --all` then re-run `--check`.

### Step 11: Verify the implementation is complete by inspection

Trust by inspection. Walk through each test:

- `dispatch_lyrics_skips_when_no_lyrics_state` — `pp.lyrics_state = None` → first match arm returns early; nothing sent.
- `dispatch_lyrics_fires_on_first_position_event` — fresh pipeline, `last_lyrics_ws_signature: None` ≠ `Some("alpha")` → ws sent; `last_resolume_subtitles_signature: None` ≠ `Some("show|alpha|beta|alfa|beta|false")` → Resolume sent; signatures updated.
- `dispatch_lyrics_idempotent_on_same_line` — second call with same position lands on same line → signatures match → no resends.
- `dispatch_lyrics_fires_on_line_change` — second call inside line "beta" → signatures differ → both fire.
- `dispatch_lyrics_resolume_gated_on_scene_active` — `scene_active = false` → Resolume branch skipped, ws still fires.
- `dispatch_lyrics_no_throttle` — two calls 100 ms apart on different lines → both fire because the dispatch path has no `should_send_position_update` gate.

### Step 12: Commit

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(playback): zero-latency lyrics dispatch — decouple from NowPlaying throttle

Splits playback/position_update.rs into two helpers:

  dispatch_lyrics_if_changed (NEW, no throttle)
      Fires on every PipelineEvent::Position. Compares current/next line
      against per-pipeline snapshots (last_resolume_subtitles_signature,
      last_lyrics_ws_signature, last_presenter_text). Dispatches Resolume
      ShowSubtitles + karaoke WebSocket + Presenter push only when the
      line changed.

  maybe_broadcast_position_update (existing, 500 ms throttle preserved)
      Now contains only the dashboard NowPlaying rebroadcast — the lyrics
      block was lifted into the new helper. Progress bar cadence
      unchanged.

Wall-verify on id=132 "Holy Forever" 2026-05-07 confirmed lines visibly
trail the singer despite correct lyrics.json timing. Root cause: ALL
downstream consumers (Resolume + Presenter + karaoke ws) were gated by
the 500 ms NowPlaying throttle introduced in 2026-04-11 commit e5caaaf6
for unrelated dashboard-flood prevention. Lyrics line changes are EDGE
events, not periodic stream events, and must not share that gate.

Adds two snapshot fields to PerPlaylistPipeline. Both are reset on song
change alongside the existing last_presenter_text reset. No DB schema
change. No LYRICS_PIPELINE_VERSION bump. No JSON-output format change.

Six new unit tests in dispatch_lyrics_tests.rs cover: skip-on-no-lyrics,
fire-on-first, idempotent-on-same-line, fire-on-line-change,
resolume-gated-on-scene-active, no-throttle (two events 100 ms apart on
different lines both fire).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review (run after writing the plan)

1. **Spec coverage:**
   - ✅ Goal 1 (dispatch within ~33 ms): Step 7 removes throttle from lyrics path.
   - ✅ Goal 2 (idempotent on same line): Step 4 + 7 add snapshots; tests 3 + 6 cover.
   - ✅ Goal 3 (NowPlaying throttle preserved): Step 7 retains `should_send_position_update` gate on `maybe_broadcast_position_update`.
   - ✅ Goal 4 (no new public API): all new fns are `pub(super)`.
   - ✅ Goal 5 (single commit, no version bump): Step 12 single commit; no version touched.
   - ✅ State changes: Step 4 + 5 + 6 add the two new fields and reset sites.
   - ✅ Tests: Step 1 has all 6 listed in the spec.
   - ✅ Failure modes: paused-pipeline reset (Step 6 song-change reset); same-line no-op (Step 7 signature compare); Resolume mpsc full handled by `try_send` (existing behaviour kept); Presenter spawn failure handled by existing `tracing::warn!` in `presenter::maybe_push_line` (unchanged).

2. **Placeholder scan:** No "TBD"/"TODO"/"add appropriate" found. Every step has full code.

3. **Type consistency:**
   - `last_resolume_subtitles_signature: Option<String>` — used identically in struct decl (Step 4), inits (Step 5), reset (Step 6), refactor (Step 7), and test (Step 1).
   - `last_lyrics_ws_signature: Option<String>` — same.
   - `dispatch_lyrics_if_changed(&mut self, playlist_id: i64, position_ms: u64)` — signature consistent in test usage (Step 1) and impl (Step 7) and call site (Step 9).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-07-lyrics-zero-latency-dispatch.md`. Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, two-stage review (spec compliance, then code quality), fast iteration in this session.
2. **Inline Execution** — execute tasks in this session using executing-plans, batch execution with checkpoints.

Per project default, dispatch subagents now without further consent. Begin Phase A.1 immediately after the plan is committed.
