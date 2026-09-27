---
paths:
  - "crates/sp-server/src/resolume/**"
  - "crates/sp-server/src/playback/recovery.rs"
---

# Resolume host driver — poll policy, NOT READY mapping, RecoveryEvent (#157, #217)

`resolume/driver.rs::HostDriver` is one task per Arena host. The steady state
runs only the light `GET /api/v1/product` liveness probe, every ~10 s with
jitter. The ~14 MB `GET /api/v1/composition` (the clip map) runs only when the
pure `FullRefreshReason::decide` asks for it (#157).

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
  retry backoff. After that the retry window applies again. A composition
  that never gets SongPlayer's clips costs 1 + 11 + 3 = 15 fetches in 5 min,
  then one fetch (and one INFO `reason="not-ready"` line) every 60 s for as
  long as it stays that way — 5× the TTL rate, the accepted trade-off.
- **The fast path needs an ANSWERED last attempt** (`last_full_attempt_failed`,
  a `decide` input).
  - Not ready means `/composition` answered without SongPlayer's clips.
  - A FAILED fetch is the #157 case: Arena's REST answers `/product` but
    chokes on the 14 MB composition while it loads. It keeps the 60 s retry
    window, even inside the 120 s.
  - "Failed" means a DNS, transport or timeout error, or a body that is not
    JSON. `fetch_mapping_inner` never checks the status, so a non-2xx JSON
    body counts as ANSWERED (not ready).
  - An answered fetch clears the flag, and the fast path resumes on the next
    tick (`an_answered_fetch_after_a_failed_one_restores_the_fast_path`).
  - Without this, after a breaker close a failing `/composition` was fetched
    every tick. Each failure also made the next probe fire a `was_failing`
    RecoveryEvent, one title re-push per tick (review round 3).
- Precedence: forced command > breaker-closed > NotReady > startup / TTL.
  NotReady must sit BEFORE TTL: after a restart `last_full_ok` is the
  pre-restart stamp, and a TTL reason would wait out the retry window.
- The refresh that finds a SongPlayer token clears `not_ready_since` and fires
  a `RecoveryEvent`. The engine's `handle_resolume_recovery` re-pushes the
  title and the current subtitle state.
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

## RecoveryEvent: one per driver step

`show_title` fades opacity from 5 % to 100 %, so a SECOND `ShowTitle` while the
title is up is a visible blink. So one driver step fires at most one event.
A step is one liveness tick (`on_tick_at`), one command, or the startup
refresh.

- **Why per step:** the engine's re-push for an event queues on the driver's
  own mpsc. The driver is busy until the step ends, so that re-push runs
  AFTER the step, against the map the step ends with. An event fired earlier
  in the step, e.g. by the probe's `apply_outcome(true)` after a failed probe,
  already covers a refresh that later in the same step finds the clips.
- **How:** `send_recovery_event` sets `recovery_sent_this_step`, and
  `on_tick_at` / the `RefreshMapping` arm clear it at the step's start. The
  ready transition fires only when it is still false.
- **Rejected guard:** `consecutive_failures > 0` cannot be the guard. The
  probe resets the counter before the refresh runs, so it missed the
  failed-probe-then-ready tick (review round 1, 🔴).
- **The breaker close:** the probe fires its event (`was_failing`), and the
  breaker-closed refresh in the same tick that finds the clips does not fire
  a second one.
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
current subtitle state of every playing, on-program pipeline:

- `ShowSubtitles` when the display plan has a line at the cached position;
- `HideSubtitles` when the plan is blank there, OR the song has no
  `lyrics_state`.

Otherwise a stale text Arena restored from its saved composition stays until
the next line change, or for the whole song.

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
