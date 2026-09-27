---
paths:
  - "crates/sp-server/src/resolume/**"
  - "crates/sp-server/src/playback/recovery.rs"
---

# Resolume host driver — poll policy, NOT READY mapping, stale ids, RecoveryEvent (#157, #217)

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
  - a ShowTitle is NOT retried. The RecoveryEvent's engine re-push shows the
    title inside its window (a pending show timer shows it itself); a second
    ShowTitle would run another fade from 5 %, the blink the
    one-event-per-step rule exists for;
  - a HideTitle is retried AT ONCE (`handlers::hide_title_now`: opacity 0,
    then the text cleared). The relaunched clip holds whatever Arena's saved
    composition restored, possibly at opacity 0, and `hide_title`'s fade
    starts at FULL opacity: a 1 s flash of stale text (review round 3). The
    re-push sends no hide, and no longer re-shows a title whose end-of-song
    hide ran ("Engine side" below);
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
  in and the title stayed into the next song (review rounds 1–2). Fixed on
  the engine side, below.

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
title is up is a visible blink. So one driver step fires at most one event.
A step is one liveness tick (`on_tick_at`), one command, or the startup
refresh.

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
  outage can have swallowed it too (review round 6);
- NEVER a `HideTitle`. `hide_title` fades from FULL opacity, so hiding a
  title that is already hidden would flash the stale text. The subtitle clear
  is instant, so it is always safe.
- `ShowTitle` ONLY inside the title window (#217 addendum 2).
  `cancel_title_timers` `take()`s both handles, so an aborted one never
  lingers.
  - A pending show timer (`title_show_abort` is `Some` and not finished,
    Started + 1.5 s): no ShowTitle. The timer shows the title itself, and a
    second ShowTitle restarts the fade (a recovery in a song's first 1.5 s,
    e.g. the song-start subtitle clear 404'd after a relaunch in the gap).
  - A finished end-of-song hide (`title_hide_abort` `Some` and
    `is_finished()`): no ShowTitle. Re-showing undid the hide for the song's
    last seconds and into the next song; a HideTitle that 404'd ends in
    exactly this recovery.
  - Otherwise (`None`, or the show ran and the hide is pending: mid-song) the
    title is re-shown.
  - Pinned by `handle_resolume_recovery_does_not_re_show_a_title_the_song_end_hid`,
    `…re_shows_the_title_mid_song` and `…leaves_a_pending_title_to_its_show_timer`.
  - Residual: a relaunch that spans the end-of-song hide with no 404 (the hide
    was skipped against an empty or evicted map) gets no re-show and no hide.
    Arena's restored title stays until the next song's ShowTitle
    (Started + 1.5 s).
  - Residual (review round 4, a follow-up for the main session): the window
    is read from the engine's timer handles, and cannot see what is already
    queued at the driver. Between Ended / a skip and the next Started a
    recovery re-shows the NEXT song's title early (then its show timer fades
    again); a ShowTitle or scene-off HideTitle queued behind the 404 refresh
    still runs as sent (a double fade, or a hide from full opacity on the
    relaunched clip). The root fix is a driver that owns the title's on-air
    state. The OBS scene-on re-push (`push_title_for_playing`) applies no
    window at all (older, outside this path).

Otherwise a stale text Arena restored from its saved composition stays until
the next line change, for the whole song, or over the next camera shot.

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
