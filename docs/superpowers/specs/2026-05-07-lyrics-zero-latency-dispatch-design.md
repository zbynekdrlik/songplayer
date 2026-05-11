# Zero-Latency Lyrics Dispatch — Design Spec

**Date:** 2026-05-07
**Status:** Draft
**Working dir:** `/home/newlevel/devel/songplayer`
**Branch:** `dev` (currently `0.32.0-dev.1`)

---

## Problem

Karaoke lines display LATE on both the LED wall (Resolume) and the Presenter stage display. Wall-verify of `id=132` "Holy Forever" (2026-05-07) confirmed lines arrive visibly after the singer reaches them.

Root cause: `crates/sp-server/src/playback/position_update.rs` gates ALL downstream consumers (`NowPlaying` WebSocket, lyrics karaoke WebSocket, Resolume `ShowSubtitles`, Presenter `push`) behind a single 500 ms throttle (`POSITION_BROADCAST_INTERVAL_MS = 500`, introduced 2026-04-11 commit `e5caaaf6`). The throttle's stated purpose was to keep `NowPlaying` from flooding the dashboard with position-bar updates at video-frame rate (~30 Hz). The wiring conflated two unrelated concerns:

1. **Periodic stream** — the dashboard's progress bar needs `position_ms` updates a few times per second. Throttle is appropriate.
2. **Edge events** — lyrics lines change only when the singer moves to a new line (a few times per song). Each such change should dispatch within one pipeline-event tick (~33 ms). Throttle is wrong.

Because lyrics dispatch was bundled with the periodic stream, every line change waits up to 499 ms before reaching Resolume / Presenter. For karaoke this is unusable.

## Goals

1. Lyrics line changes dispatch to all consumers (karaoke WebSocket, Resolume `ShowSubtitles`, Presenter `push`) within one pipeline Position event (~33 ms typical).
2. Dispatch fires only when the current OR next line changes (idempotent — repeated Position events on the same line do not flood downstream consumers).
3. Dashboard `NowPlaying` / `position_ms` broadcast behaviour is unchanged (500 ms throttle preserved). The progress bar is not part of this fix.
4. No new public API surface. Internal refactor of `playback/position_update.rs` only.
5. Single commit, no `LYRICS_PIPELINE_VERSION` bump (per project rule).

## Non-goals

- Changing the dashboard progress bar cadence.
- Changing `PresenterPayload` or `ResolumeCommand::ShowSubtitles` formats.
- Touching the lyrics-merge pipeline (`text_reference_merge`, `timed_reference_merge`, `claude_merge`).
- Network-level latency optimisation (HTTP keep-alive, TCP no-delay) — out of scope.
- Touching `last_presenter_text` semantics — `presenter::maybe_push_line` is already idempotent on identical `current_en`.

## Architecture

Split `maybe_broadcast_position_update` into two functions, both called by every pipeline `PipelineEvent::Position`:

```
PipelineEvent::Position { playlist_id, position_ms, duration_ms } →
  dispatch_lyrics_if_changed(playlist_id, position_ms)        // no throttle
  maybe_broadcast_position_update(playlist_id, position_ms, duration_ms)  // 500 ms throttle (unchanged)
```

### `dispatch_lyrics_if_changed` (new method)

Responsibilities:

- Fetch the pipeline's `lyrics_state`. If absent → return.
- Compute current `(en, next_en, sk, next_sk)` via existing `lyrics_state.resolume_lines_with_next(position_ms)`.
- Compute current/next presenter line via existing `lyrics_state.presenter_lines(position_ms)`.
- Build the karaoke WebSocket lyrics-update via existing `lyrics_state.update(playlist_id, position_ms)`.
- Compare against the snapshot stored on the per-playlist pipeline state (`PerPlaylistPipeline`) — only dispatch when the snapshot differs:
  - `last_resolume_subtitles_signature: Option<String>` — stable hash of `(en, next_en, sk, next_sk, suppress_en)` or just `en` text + suppress flag (sufficient because karaoke lines are typed by their primary EN text).
  - `last_presenter_text` — already exists, kept as the deduplication key for Presenter.
  - `last_lyrics_ws_signature: Option<String>` — for the karaoke WebSocket lyrics update message; key on `current_line_text` so we don't broadcast position-tick lyrics updates that don't change the line.
- Dispatch only the changed channels. (e.g. if presenter line same, don't push presenter; if Resolume signature same, don't push Resolume.)
- Resolume gating preserved: only dispatch Resolume when `pp.scene_active.load(Acquire)`.
- Failure handling unchanged: `try_send` for Resolume (drop on full channel), `tokio::spawn` fire-and-forget for Presenter.

### `maybe_broadcast_position_update` (modified)

Becomes purely about the dashboard's `NowPlaying` ws message. Removes:

- The `pp.lyrics_state` block (lines 56–88 in the current file).
- The Presenter push block (lines 90–102).

Keeps:

- The 500 ms throttle gate (`should_send_position_update`).
- The `pp.last_now_playing_broadcast` update.
- The `ws_event_tx.send(ServerMsg::NowPlaying { ... })` broadcast.

## State changes

`PerPlaylistPipeline` (in `playback/mod.rs`) gets two new fields:

```rust
/// Snapshot of the last Resolume `ShowSubtitles` payload signature (or
/// `HideSubtitles`-equivalent) we sent. Used by
/// `dispatch_lyrics_if_changed` to skip duplicate dispatches at every
/// Position event tick.
last_resolume_subtitles_signature: Option<String>,

/// Snapshot of the last karaoke WebSocket lyrics-update line text we
/// sent. Used by `dispatch_lyrics_if_changed` to skip duplicate dispatches.
last_lyrics_ws_signature: Option<String>,
```

`last_presenter_text` already exists and stays.

Both new fields are reset to `None` in `clear_lyrics_display` and on song change (same pattern as `last_presenter_text`).

The signature is a deterministic concatenation; format is internal-only and need not be stable across releases.

## Tests

Unit tests in `crates/sp-server/src/playback/dispatch_lyrics_tests.rs` (new sibling file):

1. `dispatch_lyrics_skips_when_no_lyrics_state` — `lyrics_state = None` → no Resolume/Presenter/ws calls.
2. `dispatch_lyrics_fires_on_first_position_event` — fresh playlist, line N → all three channels fire.
3. `dispatch_lyrics_idempotent_on_same_line` — same line at multiple Position events → only the first fires; subsequent return early.
4. `dispatch_lyrics_fires_on_line_change` — line N → line N+1 → second event fires all three channels.
5. `dispatch_lyrics_resolume_gated_on_scene_active` — `scene_active = false` → Resolume skipped, Presenter + ws still fire.
6. `dispatch_lyrics_no_throttle` — two events 100 ms apart on different lines → both fire (no `POSITION_BROADCAST_INTERVAL_MS` gate).

The existing `tests_song_end.rs::presenter_empty_payload_on_song_end` still applies; verify it does not regress (the `ShowSubtitles` clear on song-end via `clear_lyrics_display` is unaffected because `last_presenter_text` reset semantics remain).

## Failure modes

- Pipeline emits a Position event for a paused/stopped pipeline → `pp.lyrics_state` may still be present from the prior song. Mitigation: `clear_lyrics_display` on song change clears all three snapshots so the next song's first line fires correctly.
- Two consecutive Position events on the SAME line → second one is a no-op (signature equality short-circuit).
- Resolume mpsc channel full → `try_send` drops; signature still updated to reflect intended state. Acceptable: next change attempt re-dispatches if the signature actually differs.
- Presenter `tokio::spawn` push fails → existing `tracing::warn!` log; `last_presenter_text` updated to the new value (same as today's behaviour).

## Performance impact

Position events fire at video-frame rate (~30 Hz) per active pipeline. New method runs the line-detection helpers (already cheap — they index into a sorted `Vec<LyricsLine>` by `position_ms`) and a string compare against the snapshot. Both O(log n) in the song's line count (<200 typically). No HTTP / no allocations in the no-change branch. Net cost: a few microseconds per Position event per pipeline. Negligible vs the existing video-decode path.

## Migration

None. No DB schema change, no settings change, no provenance/source-label change. Output `LyricsTrack` JSON files unchanged. Catalog reprocess not needed.

## Approval gates

1. Spec approval (this doc) → user reviews, approves.
2. Plan written via `writing-plans` skill, user reviews.
3. Implementation via subagent-driven-development.
4. CI green.
5. Wall-verify on sp-live: play a song that previously felt late, confirm line dispatch latency is now sub-perceptible (lines appear in sync with the singer).

## References

- `crates/sp-server/src/playback/position_update.rs` — the file being refactored
- `crates/sp-server/src/playback/mod.rs:38-58` — `POSITION_BROADCAST_INTERVAL_MS` and `should_send_position_update`
- `crates/sp-server/src/presenter/mod.rs::maybe_push_line` — already idempotent on identical `current_en`; called by the new dispatch path
- `crates/sp-server/src/resolume/mod.rs::ResolumeCommand::ShowSubtitles / HideSubtitles` — channel commands
- `crates/sp-server/src/playback/recovery.rs` — re-emits `ShowSubtitles` after a Resolume host recovers; unchanged by this refactor (explicit one-shot dispatch, not Position-event-driven)
- 2026-04-11 commit `e5caaaf6` — origin of `POSITION_BROADCAST_INTERVAL_MS = 500`
- `feedback_line_timing_only.md` — output remains line-level only; no word synthesis
