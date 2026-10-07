---
paths:
  - "crates/sp-server/src/playback/failure_*.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event.rs"
  - "crates/sp-server/src/playback/engine_play.rs"
  - "crates/sp-server/src/playback/title_timers.rs"
  - "crates/sp-server/src/playback/state.rs"
  - "crates/sp-server/src/playback/ndi_health.rs"
  - "crates/sp-server/src/api/routes_tests_clock.rs"
  - "crates/sp-server/src/playlist/selector.rs"
  - "crates/sp-core/src/player_view.rs"
  - "crates/sp-core/src/playback.rs"
  - "sp-ui/src/components/player.rs"
  - "e2e/mock-api.mjs"
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
- Residual: on a box fault, once every unplayed song has failed, the restart
  branch clears the play history one time (the alternative, a pick without a
  clear, would let one bad file fail every other song).

## Which Play an answer belongs to (follow-up, design record 6029071745)

- `PipelineEvent::Started` / `Error` name no Play. After Play A, then Play B
  (a skip or a pick in A's pre-roll), A's answer can come after B went out.
  It used to record B (not open yet), reset B's run, and (an `Error`) count
  a failure and select a third song, replacing B before it opened.
- The pipeline answers every Play exactly once and in order. The stub
  answers each with one `Error`. On Windows a Play's pre-roll reads no
  command, so a later Play waits for the earlier one's `Started` or
  failed-open `Error`, and after `Started` the emit loop has no `Error`
  path. Only a `Shutdown` leaves a Play unanswered, and the pipeline is gone
  then. Keep it so: an answer that can come twice, or out of order, breaks
  the count. A Play id echoed in `Started` / `Error` (the design's rejected
  Approach 3) would be the fix then. The rule is restated where the
  pipeline code lives (`pipeline-testability.md`, comments at the answers
  in `pipeline.rs`, `pipeline_paced.rs`, `pipeline_stub.rs`).
- What a broken guarantee costs: a Play never answered leaves the count one
  too high for the rest of the pipeline's life. Every real answer then
  reads as an earlier Play's, so the playlist never moves on after a
  failure and records nothing. The INFO line of every ignored answer
  carries `still_pending` (`PlayAnswers::pending`): a value that never
  returns to 0 is that.
- Older and rare: the engine's event channel is keyed by playlist id, so a
  removed pipeline's last answer (its thread finishes the open it was in
  before it reads its `Shutdown`) can reach a NEW pipeline of the same
  playlist. With a Play of its own pending it is taken as that Play's, and
  the Play's own answer then saturates at 0 and acts again; the count heals
  itself.
- `failure_backoff::PlayAnswers`, `PlaylistPipeline.pending_plays`:
  - `begin_play` (every Play) calls `sent()`;
  - the `Started` and `Error` arms ask `answers_last_play` FIRST, and an
    answer that leaves Plays pending returns at once (an INFO line): no
    record, no run reset, no failure, no selection, no title clock, no
    lyrics;
  - an answer with nothing pending saturates at 0 and acts as before (a test
    injects one with no Play sent).
- `Ended` / `Position` are not answers: a still-queued `Ended(A)` is still
  taken as B's (`resolume-driver.md`).
- A test that sends more than one Play before the answer it injects must
  inject the earlier answers too
  (`a_song_is_recorded_as_played_when_it_starts`: the skip's late `Started`,
  then Previous's).

## Operator visibility

- `PipelineHealthSnapshot.open_failures: Option<sp_core::playback::OpenFailures>`
  `{count, last_error, retry_at_ms, retry_in_ms, on_program}` (`null` at 0). The engine
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
  "Čaká na ďalší pokus" while a retry waits, the state is known and the
  pipeline is not told it decodes (`player_state_label`).
- The badge follows the SAME rule (`waits_for_retry`, one predicate) for a
  retry that BELONGS TO SP-PROGRAM'S SOURCE (set when armed, refreshed by
  an ON that sent no Play): `player_program_badge` reads "● Na programe —
  čaká na ďalší pokus" (`ProgramBadge::OnProgramRetry`, the `on` style,
  `is_on_program`). The engine's wait is `WaitingForScene`, which alone
  read "○ Mimo programu" for SP-program's source, its program black. A
  pipeline told it decodes keeps the WS state's badge.
- A retry is armed off program too: `video_failed` backs off from
  `Playing` whatever the scene (a ▶ off air, a dub prepared on the Dabing
  page). The WS state cannot tell the two apart, so the engine states it
  (ROZHODNUTÉ 6029773698, answering Design-question 6029484142):
  - `back_off` records `on_program` = the playlist is in the authority's
    on-air set (SP-program's source) when the retry is armed
    (`PendingRetry.on_program`, `failure_backoff::RetryView`,
    `OpenFailures.on_program`, `#[serde(default)]`, additive);
  - a cut off program (or the dashboard's Pause) is a `SceneOff`, which
    always ends the retry; a cut on program ends it with the Play its
    selection sends, and when that sends none (no song to pick, a custom
    playlist in Single mode, a DB error) `retry_came_on_program` (in
    `handle_scene_change`, after the `SceneOn`) sets the flag from the
    on-air set and re-publishes the row (review round 4); with no retry
    pending the row says `false`;
  - `OpenFailures::retry_pending` (from `retry_at_ms`, so the engine's own
    row and a row as read agree; the label's input) and
    `retry_on_program` (the badge's): a ▶ off program that waits keeps "○
    Mimo programu", with the label "Čaká na ďalší pokus".
- The badge follows the 1 Hz health poll at both ends of a wait: it reads
  "○ Mimo programu" for up to ~1 s after the wait starts, and keeps the
  retry badge up to ~1 s after a cut off program or a Pause ends the retry
  (neither changes the WS state then). A cut flips the label and the badge
  in one render only outside a wait.
- Mock: rows carry `open_failures: null`; the GET fills `retry_in_ms` per
  request and passes `on_program` through as a spec set it (absent = false);
  `/__mock/ndi-health-reset` restores the default rows.

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
- A late answer: `a_late_started_of_an_earlier_play_neither_records_nor_ends_the_run`
  and `a_late_error_of_an_earlier_play_neither_counts_nor_replaces_the_newer_play`
  (Play A, a skip = Play B, A's answer, then B's); the count itself in
  `failure_backoff_tests.rs`. The badge table (× a retry armed on / off
  program): `player_view`
  `the_badge_says_on_program_while_a_retry_armed_on_program_waits`; the
  flag on the row: `a_retry_armed_off_program_says_so_on_the_row`,
  `a_retry_armed_on_program_says_so_until_it_ends`,
  `a_retry_still_pending_after_its_playlist_came_on_program_says_so` (the
  ON refresh; on program =
  `put_on_air_for_test` + the scene's ON). The mock E2E sets the WS state
  with `/__mock/set-playing {playlist_id, state, transport}` and the row's
  `on_program` through `/__mock/ndi-health` (`player-open-failures.spec.ts`:
  both badges).
- Adding a field to `PlaylistPipeline`: THREE literals build it
  (`runtime_pipeline.rs`, `tests.rs`, `dispatch_lyrics_tests.rs`); grep
  `PlaylistPipeline {` across the crate.
