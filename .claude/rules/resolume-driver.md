---
paths:
  - "crates/sp-server/src/resolume/**"
  - "crates/sp-server/src/playback/recovery.rs"
  - "crates/sp-server/src/playback/title.rs"
  - "crates/sp-server/src/playback/title_timers.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event.rs"
---

# Resolume host driver — poll policy, NOT READY mapping, stale ids, RecoveryEvent, the wall title (#157, #217)

`resolume/driver.rs::HostDriver` is one task per Arena host. The steady state
runs only the light `GET /api/v1/product` liveness probe, every ~10 s with
jitter. The ~14 MB `GET /api/v1/composition` (the clip map) runs only when the
pure `FullRefreshReason::decide` asks for it (#157). Inside a not-ready
episode's fast window the driver ticks every 2 s instead (#217 addendum 2).

## Arena's REST answers before its composition has loaded (#217)

After an Arena restart (the owner's hotkey, a crash relaunch, SP-ArenaLaunch),
`/product` and `/composition` answer while the composition is still loading.
`/composition` then returns no clips (`{"layers":[]}`), and `parse_composition`
gives an empty map.

**An empty mapping is NOT READY, never a success.** A mapping with none of
`resolume::SONGPLAYER_TOKENS` (`#sp-title`, `#sp-subs`, `#sp-subs-next`,
`#sp-subssk`) means "not loaded yet". The operator's own tokens (`#bible-*`,
`#timer`) alone do not count. Before #217 such a refresh stamped
`last_full_refresh_ok_at`, so the next full refresh was the 300 s TTL one, and
the wall had no lyrics or title for ~5 min (box log 2026-09-27).

What the driver does now (`refresh_mapping(now)`):

- A not-ready refresh leaves `last_full_refresh_ok_at` / `last_full_refresh_ts`
  alone. It records the FIRST not-ready instant of the episode in
  `not_ready_since`. Never restamp it on each refresh: that keeps the fast
  window open forever.
- It still calls `apply_outcome(true)`, because the REST answered. The
  breaker and failure counter do not see "not ready".
- `decide` returns `NotReady` (log `reason="not-ready"`) on every tick for
  `NOT_READY_FAST_WINDOW` (120 s) from `not_ready_since`, skipping the 60 s
  retry backoff. After that the retry window applies again.
- **The 2 s cadence (#217 addendum 2).** On that fast path `run` ticks
  every `NOT_READY_TICK` (2 s, `tick_period`) instead of the ~10 s liveness
  cadence, so the clips are mapped within ~2 s of the composition loading
  (the box took 11 s, one liveness tick). One predicate,
  `not_ready_fast_path` (open episode, inside the 120 s, last attempt
  answered), decides both the every-tick refetch and the 2 s tick, so they
  never disagree: after a failed fetch the retry window runs at the liveness
  cadence. A command that put an episode on its fast path (a 404 push) moves
  the next tick up to 2 s (`tick_due_after_command`, a `min`: it never delays
  the probe).
- **The cost.** A composition that never gets SongPlayer's clips costs
  1 + 59 + 3 = 63 fetches in 5 min (0..118 s every 2 s, then 180, 240, 300 s),
  then one fetch (and one INFO `reason="not-ready"` line) every 60 s for as
  long as it stays that way — the accepted trade-off of the design record
  (comment 5858074840). `driver_not_ready_tests.rs`'s storm test drives
  explicit 10 s ticks and still pins 15.
- **A 2 s episode opens the breaker sooner.** Three failed probes in a row
  open it, so during a fast window that is ~6 s of failures (plus the 5 s
  probe timeouts), not ~30 s. The close then starts a fresh episode and fires
  its own RecoveryEvent.
- **The fast path needs an ANSWERED last attempt** (`last_full_attempt_failed`,
  a `decide` input).
  - Not ready means `/composition` answered without SongPlayer's clips.
  - A FAILED fetch is the #157 case: Arena's REST answers `/product` but
    chokes on the 14 MB composition while it loads. It keeps the 60 s retry
    window, even inside the 120 s.
  - "Failed" means a DNS, transport or timeout error, a non-2xx status, or
    a body that is not JSON. `fetch_mapping_inner` calls `error_for_status`,
    so a 500 with a JSON body is a failed fetch (#217 addendum 2; before, it
    was parsed as an answered, not-ready composition).
  - An answered fetch clears the flag, and the fast path resumes on the next
    tick (`an_answered_fetch_after_a_failed_one_restores_the_fast_path`).
  - Without this, after a breaker close a failing `/composition` was fetched
    every tick. Each failure also made the next probe fire a `was_failing`
    RecoveryEvent, one title re-push per tick (review round 3; `was_failing`
    is gone since #217 addendum 2, see below).
- Precedence: forced command > breaker-closed > NotReady > startup / TTL.
  NotReady must sit BEFORE TTL: after a restart `last_full_ok` is the
  pre-restart stamp, and a TTL reason would wait out the retry window.
- The refresh that finds a SongPlayer token clears `not_ready_since` and fires
  a `RecoveryEvent` when the map changed and the step has not fired one yet
  (see below). The engine's `handle_resolume_recovery` re-pushes the title and
  the current subtitle state.
- **An evicted map is not ready too.** Opening the breaker empties
  `clip_mapping` and clears `not_ready_since` (the outage ends any episode).
  The probe that closes the breaker sets `not_ready_since = now`
  (`on_tick_at`, which logs "clip map evicted by the outage — not ready …").
  A breaker-closed refresh that finds no clips does not log its own
  not-ready line: the episode has already started.
  - Why: `decide` applies the 60 s retry backoff to `BreakerClosed` too. A
    breaker-closed refresh held back by it (Arena back less than 60 s after
    the last attempt) would otherwise leave the empty map looking like the
    steady state until the TTL (review round 2).
  - The relaunch gets a FRESH 120 s fast window from the close, not the tail
    of an old episode.

## A push answered 404 = a stale clip map (#217 addendum 2)

**Arena gives every clip and text param a NEW id on each relaunch** (box
2026-09-27: `#sp-subs` 1790510617970 → 1790518489097; still so on 7.28.0). A
relaunch quicker than three failed probes never opens the breaker, so nothing
refreshed the map: every push went to the dead ids (`404 Not Found`, SongPlayer
log 14:12:22) until the 300 s TTL.

- **Detection at the push choke point.** `set_text` / `set_clip_opacity`
  call `note_push_status`, which sets the `stale_id_seen` `AtomicBool` on a
  404. They take `&self` (the handlers run them in parallel through
  `FuturesUnordered`), hence the atomic.
- **Only 404.** A 500 or a timeout is Arena's REST choking; a refresh would
  not help and costs ~14 MB (`a_push_answered_500_does_not_mark_the_map_stale`).
- **The push step** (`driver_push.rs::run_push`, every title/subtitle
  command is its own driver step and clears `recovery_sent_this_step`):
  1. run the handler (`push` returns whether a 404 was seen and clears the
     note, so it is false outside a push);
  2. on a 404, `not_ready_since.get_or_insert(now)` (an open episode keeps
     its start, so its fast window never extends);
  3. refresh through `decide` (the NotReady path; a failed last attempt still
     waits out the 60 s retry window, the ticks fetch it then);
  4. still not ready, or the fetch failed: stop, the episode's ticks carry on;
  5. the refresh mapped NEW clips (a relaunch): retry the push once
     (`retry_push`). The ready transition has already fired the step's
     RecoveryEvent.
- **How each push is retried (`retry_push`):**
  - a title to show (a ShowTitle, a Resync naming one) is NOT retried. The
    RecoveryEvent's engine re-sync names the title only inside its window;
  - everything else runs again through the same `push`. A HideTitle / a
    Resync naming none then hides AT ONCE (`hide_title_now`: opacity 0, then
    the text cleared) by the title state alone: the refresh's new ids made it
    `Unknown`, or the failed hide left it `FadingOut` (see "The driver owns
    the wall title"). The relaunched clip holds whatever Arena's saved
    composition restored, possibly at opacity 0, and `hide_title`'s fade
    starts at FULL opacity: a 1 s flash of stale text (review round 3). An
    explicit HideTitle arm here would be an equivalent mutant (deleting it
    changes nothing), so there is none;
  - a subtitle push runs again: an instant, harmless double.
- **`finish_push` is the one end of a push:** it logs a failure and returns
  and clears the 404 note, so the note is false outside a push (the instant
  hide goes through it too).
- **The SAME clips came back = a refused id, not a stale map
  (`refused_ids_at`).** Arena still lists the ids it answers 404 for, so
  no clip got a new id: no retry (it would 404 again), no RecoveryEvent
  (`refresh_mapping` fires the ready event only when the map changed), and
  no new refresh for `FULL_REFRESH_RETRY` (a 404 on it logs at debug).
  Otherwise every push costs a ~14 MB fetch, and with an event the loop
  ready refresh → RecoveryEvent → re-push → 404 → refresh had no timer in it.
  Only this holds a refresh back: a relaunch maps new ids, so a second
  relaunch a few seconds later refreshes at once
  (`a_second_relaunch_within_a_minute_is_refreshed_at_once`; an earlier
  "one mark per 60 s" guard held it back, review round 1). The hold is not
  per id: a relaunch inside the 60 s after a refused-id refresh waits for
  the hold to end (no episode opens during the hold, so no 2 s ticks help;
  only a breaker close would refresh sooner; the refused-id refresh itself
  restamped the TTL clock) — only when an id is
  refused, which the steady state never has.
- **Logs.** The stale-map WARN is logged only when the refresh runs; a 404
  that the retry window holds, or on a refused id, logs at debug (a lyric
  line is pushed every few seconds).
- **A HideTitle that 404s near a song's end** is the one push whose
  recovery used to undo it: `handle_resolume_recovery` re-pushed `ShowTitle`
  for every pipeline Playing on program, so the retried hide was faded back
  in and the title stayed into the next song (review rounds 1–2). Now the
  recovery's Resync names no title after the song's hide point ("Engine
  side" below).

## A relaunch with no push: the one-param probe (#217 addendum 3)

A relaunch quicker than three failed probes never opens the breaker. If
SongPlayer pushes nothing meanwhile, no request answers 404, and the map
keeps the dead ids until the 300 s TTL refresh, which fires no event.

- **When:** `on_tick_at` notes `was_failing` (`consecutive_failures > 0`)
  BEFORE the liveness probe. On an ok probe with no open not-ready episode
  (it refreshes anyway, and a probe would restamp its start),
  `driver_probe.rs::probe_stale_map` runs before the tick's `decide`. A
  breaker close is such an episode: it evicted the map and opened one, so
  it is never probed, and no `!breaker_just_closed` guard is needed (review
  round 1; a test of that half could not fail).
- **What:** `GET /api/v1/parameter/by-id/{id}` for the first mapped
  SongPlayer text param (`SONGPLAYER_TOKENS` order), ~100 bytes. A full
  `/composition` per hiccup (14 MB) is the design's rejected alternative.
- **A 404** sets `not_ready_since = now`, so the same tick's `decide` takes
  the NotReady path: a refresh now (the fast path needs an answered last
  attempt), 2 s ticks while Arena still loads, and the ready map fires the one
  RecoveryEvent. The next ok tick is no flip, so it is not probed again.
- **Anything else** (200, a 5xx, a timeout) leaves the map alone. The probe
  never counts toward the breaker.
- **A composition fetch failure counts as a failure too** (the #157 case):
  the next ok tick probes one param. That costs at most one tiny GET per
  failed fetch, and the retry window still spaces the refetches.
- **A 404 on an id Arena still lists** (a refused id) costs one refresh per
  failing→ok flip: the refresh maps the same clips and fires no event, and
  there is no `refused_ids_at` hold on this path. That is bounded by the
  failure rate; the steady state never has a refused id.

## The driver owns the wall title (#217 addendum 3)

The engine cannot see what is queued at the driver. With the title decided
by the engine alone, a ShowTitle queued behind a slow step plus the
recovery's ShowTitle ran two fades (a blink), and a hide on a relaunched
clip faded restored text from full opacity (a flash). Now
`resolume/title_state.rs` holds `TitleState` + the `#sp-title` clips it was
reached on (`WallTitle`), and every title command goes through
`driver_push.rs::push_title` → `TitleState::plan` → `run_title_action`.

- **States:** `Unknown` (startup; new title clip ids), `Hidden`,
  `Shown(text)`, and `FadingIn(text)` / `FadingOut`. The last two are what a
  fade that did not finish leaves (a request failed): partly up. The driver
  runs one command at a time and a fade blocks it, so a fade is never seen
  "in flight" by another command.
- **`plan`, act only on a difference** (it returns `Option<TitleAction>`,
  `None` = nothing to do):
  - a title (a `ShowTitle`, or a `Resync` naming one): nothing while that
    exact text is `Shown`. Onto a `Hidden` clip it fades in (`FadeIn`:
    `show_title`, text then 5 % → 100 %). Onto any other state it is
    `Replace` (`replace_title`: opacity 0 at once, then the same fade-in).
    `show_title` writes the text first, so over a title that is up (or a
    relaunched clip) the new text showed at full opacity before the fade
    restarted: a blink (review round 1). An empty ShowTitle shows nothing;
    an empty Resync title is no title;
  - `HideTitle`: nothing while `Hidden`. It fades out only from `Shown` (a
    title known to be up); any other state hides at once (`hide_title_now`);
  - a `Resync` naming no title: nothing while `Hidden`, else hide at once.
- **The state holds only for its clips.** `refresh_mapping` calls
  `note_clips` when the map changed. Other non-empty `#sp-title` ids mean a
  relaunch (Arena re-ids every clip), so the state becomes `Unknown`: the
  recovery's Resync must act on the restored clip, not trust the old
  `Shown(t)`. The same ids after an outage (breaker evicted, then re-mapped)
  keep the state, so a long REST hang does not re-run the fade. No title
  clips (a composition still loading) says nothing.
- **No title clips mapped:** nothing runs and the state is kept. A Show
  against an empty map must not claim `Shown`.
- **A `Resync` supersedes the title commands queued before it** (`take_queued`,
  called by `run`): when the driver takes a command, it also takes every
  command already queued behind it and drops each ShowTitle / HideTitle /
  Resync before the LAST Resync. The Resync is the engine's later statement
  of the wall, so a queued ShowTitle can no longer flash the title on the way
  to a `Resync(None)`. Subtitle commands and later title commands keep their
  order. A superseded scene-off HideTitle leaves the old title up for the
  next scene's `Resync(Some(new))`, which is why that is a `Replace`, not a
  text swap at full opacity.
- **A batch holds the tick.** `run` handles the whole drained batch (at most
  the 64-command channel, each fade ~1 s) before `select!` looks at the tick
  or the shutdown again. The tick's deadline is kept, only reached later.
- **`handlers::show_title` takes the formatted text** (`format_title_text`),
  the text the state compares. `playback::title::format_title_text` is a
  re-export of it, the one formatter for the Resync, the ShowTitle and the
  OBS text.

## The RecoveryEvent forwarder (#217 addendum 3)

`playback/recovery.rs::forward_recovery_events` (spawned by `lib.rs`) matches
the WHOLE `recv()` result. The old `Ok(event) = recv()` select branch was
disabled by the first error: after one `Lagged`, that `select!` waited on
shutdown only, and no RecoveryEvent reached the engine again.

- `Lagged(n)`: a WARN, then ONE `ResolumeRecovered { host: "(lagged)" }` for
  the missed events. A recovery may be pending, and the re-push is idempotent
  (the driver acts only on a difference). The event the channel kept is
  forwarded next as usual.
- `Closed` or shutdown: the task ends.
- Test with a capacity-1 channel and three sends BEFORE the task starts:
  `(lagged)`, then the kept third, then a later one; drop the sender →
  it ends (`recovery_tests.rs`).

## Subtitle clips: blank, never skip (#217 addendum 2)

- `clear_subtitles` clears `#sp-subs`, `#sp-subs-next` AND `#sp-subssk`. The
  next-line clip used to keep its text while the wall was blank (box:
  `#sp-subs-next` = "If he dresses lilies" with `#sp-subs` empty).
- A `suppress_en` push (English lyrics inside the video) writes the EN clips
  EMPTY. Skipping them left the previous song's English on the wall. The
  engine pushes only on a signature change, so that is one blank write each.

## RecoveryEvent: only on a real recovery, one per driver step

**Only a real recovery fires one (#217 addendum 2):** the breaker close
(`apply_outcome`) or a not-ready map becoming ready with a CHANGED map
(`refresh_mapping`, including an episode a 404 started). A bare failing→ok
flip (one failed probe or `/composition`) evicted nothing, so it fires none.
It used to (`was_failing`): every transient failure re-pushed all hosts and
restarted the title fade, and in the #157 retry case every failed fetch did.

- **The accepted cost (design item 2).** A push that failed with a timeout or
  a 5xx (not a 404) during such a hiccup is no longer caught up by a
  re-push. A subtitle line comes back at the next line change (the engine's
  dedup key changes); a lost `ShowTitle` stays lost until the next song.
  Before, the flip's event re-pushed it only when a probe or `/composition`
  had also failed, never for a push that failed alone.

`show_title` fades opacity from 5 % to 100 %, so a SECOND `ShowTitle` while the
title was up was a visible blink. So one driver step fires at most one event.
Since #217 addendum 3 the re-push's title is a `Resync` that does nothing for
a title already up, so a second event is only redundant work, and the rule
stays. A step is one liveness tick (`on_tick_at`), one command, or the
startup refresh.

- **Why per step:** the engine's re-push for an event queues on the driver's
  own mpsc. The driver is busy until the step ends, so that re-push runs
  AFTER the step, against the map the step ends with. An event fired earlier
  in the step, e.g. by the probe's `apply_outcome(true)` closing the breaker,
  already covers a refresh that later in the same step finds the clips.
- **How:** `send_recovery_event` sets `recovery_sent_this_step`, and
  `on_tick_at` / the `RefreshMapping` arm / `run_push` clear it at the step's
  start. The ready transition fires only when it is still false.
- **Rejected guard:** `consecutive_failures > 0` cannot be the guard. The
  probe resets the counter before the refresh runs, so it missed the
  failed-probe-then-ready tick (review round 1, 🔴).
- **The breaker close:** the probe's `apply_outcome` fires its event on the
  close, and the breaker-closed refresh in the same tick that finds the clips
  does not fire a second one.
- **Counting events in tests:** the event is broadcast synchronously inside
  the driver call. Count with `try_recv` right after the `.await` (see
  `drain` in `driver_not_ready_tests.rs`).
- **Every host re-pushes.** The engine's `handle_resolume_recovery` ignores
  `host`, and the Resolume command forwarder sends to every host. So every
  RecoveryEvent re-pushes all hosts, including healthy ones (an older
  behaviour).

## Engine side (`playback/recovery.rs`)

`handle_resolume_recovery` sends straight on `resolume_tx`. It does NOT go
through the `last_resolume_subtitles_signature` dedup in
`playback/position_update.rs`, so a re-push is always delivered.

The dedup DOES record a push the driver skipped against an empty clip map
("no Resolume subtitle clips found … skipping push"), and the song-start
`clear_lyrics_display` hide is never re-sent. So recovery re-sends the full
current subtitle state of the playing, on-program pipelines:

- `ShowSubtitles` for each one whose display plan has a line at the cached
  position;
- ONE `HideSubtitles` only when none of them has a line (a blank plan
  position, or a song with no `lyrics_state`). The subtitle clips are shared
  by every on-program playlist, so a blank one must never clear another's
  line (review round 5; `obs/scene.rs` can keep several active playlists);
- that same one `HideSubtitles` when NO SongPlayer playlist plays on program.
  The scene-off hide goes through the same `clear_subtitles` path, so an
  outage can have swallowed it too (review round 6).

Otherwise a stale text Arena restored from its saved composition stays until
the next line change, for the whole song, or over the next camera shot.

**The title is ONE `Resync` (#217 addendum 3), sent first by
`resync_wall_title(now)`**, which the OBS scene-on (`push_title_for_playing`)
uses too. It names the title that SHOULD be up, and the driver compares it
with what it did (above).

- **One clock: `TitleClock { video_id, show_at, hide_at }`**
  (`playback/title.rs`). The `Started` handler fixes it
  (`show_at = Started + 1.5 s`, `hide_at = end − 3.5 s`, `None` for a song of
  5 s or less or an unknown 0 duration) and arms the timers from it
  (`title_timers.rs::arm_title_timers`: `sleep_until` those instants).
- **A title is due** (`PlaylistPipeline::title_due`) when its pipeline plays
  on program and its clock is this video's and `open_at(now)`:
  `[show_at, hide_at)`.
- **Why not the position (review round 1, 🔴).** The first version read
  `cached_position_ms`, the decoder position the pipeline reports every
  500 ms, while the timers slept on the clock of `Started`. For up to
  ~500 ms at each boundary they disagreed. A `Resync(Some)` queued with the
  song-end HideTitle superseded it, and the title stayed into the next song;
  a `Resync(None)` just after the show timer hid the title for the whole song.
  Every OBS program change re-sends `on_program: true` for every on-program
  playlist (`obs_bridge.rs`), so a Resync near a boundary is common. The
  timers and the Resync now read the same instants; only a race of
  microseconds between the engine computing a Resync and a timer sending
  remains.
- **Between songs.** The clock is the previous video's until the new one's
  `Started`, and a skip's late Position events do not matter any more. So the
  window is closed from a song change to the new `Started`, and the scene-on
  that selects a new song names no title.
- **A scene-on re-arms the timers** (`rearm_title_timers`). A scene-off
  cancels the song's timers (a timer of a playlist off program must not write
  the shared clip), while the #215 transition hold keeps the song playing. A
  scene-on of the playing video cancels and arms them again from its clock,
  for what is still ahead. Before, a bounce in the first 1.5 s left the song
  with no title, and a later one with no hide 3.5 s before the end. A clock of
  another video arms nothing; its `Started` will. Every scene change
  re-arms (cheap tasks, the same deadlines).
- **Several due** (a program scene with more than one SongPlayer playlist;
  they share the one `#sp-title` clip): the highest playlist id, so the
  answer never depends on HashMap order.
- **The text** comes from `format_title_text` (one formatter, see above).
  The OBS text source follows the Resync: the title, or cleared, as the hide
  timer clears it. A failed DB read sends nothing: a transient error must not
  hide a title mid-song.
- **`cached_position_ms`** is the subtitle and pause position only. The
  `Started` handler no longer zeroes it: a Pause before the first Position
  report recorded 0 and resumed the song from its start (review round 1).
- Pinned in `tests_scene_change.rs` (`Window::{Due, BeforeShow, AfterHide,
  OtherSong, NotStarted}`), `title_tests.rs` (the clock's instants and both
  sides of each boundary) and `a_recovery_follows_the_title_clock_not_a_lagging_position`.
- **Residual:** a resume (`start_position_ms`) runs its clock from its
  `Started`, like the timers, not from the resume position. The hide timer
  was already this way.

## Testing the driver on the no-compile box

- **One clock.** `on_tick_at(now)` → `run_full_refresh(reason, now)` →
  `refresh_mapping(now)` stamps `last_full_attempt_at`, `last_full_refresh_ok_at`
  and `not_ready_since` with the SAME synthetic `now`. Drive every policy test
  with `base + Duration` (never `Instant::now() - x`: that underflows on a
  freshly booted Windows runner). A real `Instant::now()` stamp mixed with
  synthetic ticks shifts every window by the test's runtime.
- **A composition sequence** is two mocks on the same path. Mount the first
  with `.up_to_n_times(1)` first. wiremock 0.6 sorts mocks by priority, and
  the sort is stable, so at equal (default) priority mount order decides. The
  first mock wins while it has a use left.
- **A "steady state" fixture must carry a SongPlayer clip.** `{"layers":[]}`
  is NOT READY and is fetched again on the next tick
  (`driver_poll_tests.rs::loaded_composition`).
- Pin exact fetch counts from a scratch Python model of `decide` + the tick
  loop. The storm test's 15 kills the `<` → `<=` window mutant; a `≤ 15` bound
  would not.
- **The 2 s cadence without `run`** (`run` is `mutants::skip`, real time):
  simulate its loop, `now += driver.tick_period(now, secs(10))` with the
  jitter fixed, and pin the fetch instants
  (`a_not_ready_episode_refetches_every_2_s_then_falls_back_after_120_s`).
  Bound the loop (`for _ in 0..200`): a mutant whose period is zero would
  otherwise hang the test instead of failing it.
- **A push step on the synthetic clock:** call `run_push(&cmd, base + secs(n))`;
  `handle_command` stamps `Instant::now()`.
- **Mount every route a push test PUTs to.** wiremock answers an unmatched
  request with 404, which the driver now reads as a stale id and refreshes.
- **Mount the probed param in every failing→ok tick test** that keeps a
  ready map (`GET /api/v1/parameter/by-id/{id}` → 200). An unmounted route
  answers 404, which the probe reads as a relaunch: an extra refresh, and
  a not-ready episode the test did not mean
  (`a_single_failed_composition_then_an_ok_probe_fires_no_recovery_event`).
- **A title test starts from a state:** `WallTitle::at(state, clips)`
  (`#[cfg(test)]`), then reads the wall's request sequence
  (`driver_title_tests.rs::put_sequence`: `opacity 100 = 0.0`,
  `text 900 = ""`). The "no flash" test feeds `take_queued` from a real mpsc,
  like `run`.
- **An engine window test sets the song's clock** (`tests_scene_change.rs`,
  `play` + `clock_for`). Its instants are the test's `Instant::now()` or an
  hour ahead, and past the hide point is `hide_at = now`. Never subtract from
  `now`: a freshly booted Windows runner's monotonic clock underflows. A
  window test of the real-clock engine is then deterministic, even under a
  ptrace stall. Pin the boundaries on `TitleClock` itself
  (`title_tests.rs`: `base + 1499 ms` / `base + 1500 ms`), and a timer-arming
  boundary with an explicit `now` (`arm_title_timers(7, now)` with
  `show_at == now`).
- Give `None` a type (`[None::<String>]`) in an `assert_eq!` against a
  `Vec<Option<String>>`.
