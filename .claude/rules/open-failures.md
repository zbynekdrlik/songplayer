---
paths:
  - "crates/sp-server/src/playback/failure_*.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event.rs"
  - "crates/sp-server/src/playback/engine_play.rs"
  - "crates/sp-server/src/playback/title_timers.rs"
  - "crates/sp-server/src/playback/state.rs"
  - "crates/sp-server/src/playlist/selector.rs"
  - "crates/sp-core/src/player_view.rs"
  - "sp-ui/src/components/player.rs"
  - "e2e/player-open-failures.spec.ts"
---

# Videos that cannot be opened: the pause, the retry, the play history (#229)

PP, 6.10.2026: a box with no VP9/AV1 decoder could open no cached video. Each
failed Play selected the next song at once (`state.rs`: `(Playing,
VideoError)` → `SelectAndPlay`): ~6 songs a second, 675 errors in 35 s, a black
program, and a play-history row per attempt. Design record: #229 comment
6027515909 (Approach 1); the lane's readings: `Anchors-confirmed` 6027712205;
the review finding on the selection: 6028419694.

## The pause (`playback/failure_backoff.rs` pure, `failure_retry.rs` engine)

- `next_attempt(n)`: failures 1 and 2 in a row → `None` (the next song at
  once); the 3rd → 5 s, 4th → 30 s, 5th → 120 s, every later one → 300 s.
  `FailureRun` counts them, keeps the last error and WHICH songs failed; a
  `Started` resets it.
- `video_failed` (the `PipelineEvent::Error` arm, after `pause_if_held`) runs
  the state machine's `VideoError` step itself: only where it would
  `SelectAndPlay` (from `Playing`) and the table says wait does it skip the
  selection, set `WaitingForScene`, broadcast, and arm ONE retry
  (`back_off`). Anything else goes through `apply_event` as before. The state
  machine is untouched (no `Backoff` state).
- The retry is `PipelineEvent::RetryDue(id)` on the engine's own channel, a
  `sleep_until(due)` task, `id` from the process-wide `NEXT_RETRY` (never a
  tokio task id). Its attempt is `apply_event(PlayEvent::Start)`, the ▶'s
  selection; it claims no program.
- What ends a pending retry (its `RetryDue` is then stale, `take_due`):
  `begin_play` (EVERY Play), a `SceneOff` in `apply_event` (a cut off program
  AND the dashboard's Pause), `skip_backoff` (a Skip while a retry waits tries
  the next song NOW — the state machine ignores Skip in `WaitingForScene`), and
  `song_started`. The count survives a cut away: the cut back's attempt fails
  into the next, longer pause.
- One WARN per pause (`videos cannot be opened — the next attempt waits`,
  `failures`, `error`, `retry_in_s`).

## The play history: recorded at `Started`, and the selection avoids

- SelectAndPlay and `handle_play_video` set `PlaylistPipeline::record_on_start`
  after `begin_play` (which clears it); `song_started` (first thing in the
  `Started` arm) records that song. Previous and a Loop replay record nothing,
  as before. A test that expects a play_history row sends
  `PipelineEvent::Started` first (`tests_hold.rs::started`).
- **A song that never opened is never recorded, so the selection MUST leave
  it out.** Otherwise, at the end of a rotation, it is the one unplayed song
  and is picked for good (the review's 360 → 49 songs a day model). Every
  youtube selection passes `FailureRun::avoid(current)` (the run's failed
  songs + the song just sent, which is not recorded before it starts) to
  `VideoSelector::select_next(…, avoid)`, decided by the pure
  `failure_backoff::pick_pool`: unplayed minus avoid; else a restart (history
  cleared) from all minus avoid; only when every song is avoided, the old
  pick. Custom playlists (by position) and Loop ignore it.
- Residuals: a late `Started` of an earlier Play takes the newer Play's mark
  and resets the run (older: `Started` names no video); and on a box fault,
  once every unplayed song has failed, the restart branch clears the play
  history one time (the alternative, a pick without a clear, would let one
  bad file fail every other song).

## Operator visibility

- `PipelineHealthSnapshot.open_failures: Option<sp_core::playback::OpenFailures>`
  `{count, last_error, retry_at_ms, retry_in_ms}` (`null` at 0). The engine
  writes it as the run changes (`publish_open_failures` →
  `NdiHealthRegistry::set_open_failures`: `back_off`, `song_started`, the end
  of every `apply_event`, PlayVideo, Previous) and each heartbeat copies it.
  `snapshots()` fills `retry_in_ms` at the read on the SERVER's clock
  (`OpenFailures::read_at`): the dashboard counts down from it, never from the
  browser's clock (another machine). A struct literal of the snapshot or of
  `OpenFailures` needs every field.
- The shared Player: `player-open-failures` under its head
  (`sp_core::player_view::open_failures_line`, "Videá sa nedajú otvoriť (N×):
  … — ďalší pokus o X s", X = `retry_in_ms` rounded up), mounted by a Memo,
  its text following the 1 Hz `store.ndi_health` poll; the state label reads
  "Čaká na ďalší pokus" while a retry waits and the state is known
  (`player_state_label`). The
  on/off-program badge still reads the WS state (`WaitingForScene` → "○ Mimo
  programu", also for a playlist on program that waits black).
- Mock: rows carry `open_failures: null`; the GET fills `retry_in_ms` per
  request; `/__mock/ndi-health-reset` restores the default rows.

## Testing it

- The engine's Play count: `failure_retry_tests.rs::when_every_open_fails_the_plays_follow_the_pause_table`
  (Plays at 0, 0, 0, 5, 35, 155, 455 s). It is a paused-clock test that awaits
  SQLite: see `rust-workspace.md` "A paused-clock test that awaits SQLite".
  Its failures are the TEST pipeline's own answers (the Linux stub's text, a
  decode error on Windows), never a text the test chose.
- Without a paused clock, drive the failures yourself
  (`handle_pipeline_event(Error)`), read the pending retry's `id` from
  `pp.failures.retry`, and tell a Play by a marker title clock (`begin_play`
  clears it), never by the pipeline's replies.
- A random pick is pinned by repetition (ten playlists, thirty skips): a test
  that a random choice could pass by luck is no RED.
- Adding a field to `PlaylistPipeline`: THREE literals build it
  (`runtime_pipeline.rs`, `tests.rs`, `dispatch_lyrics_tests.rs`); grep
  `PlaylistPipeline {` across the crate.
