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
  that never gets SongPlayer's clips costs 1 + 11 + 3 = 15 fetches in 5 min.
- Precedence: forced command > breaker-closed > NotReady > startup / TTL.
  NotReady must sit BEFORE TTL: after a restart `last_full_ok` is the
  pre-restart stamp, and a TTL reason would wait out the retry window.
- The refresh that finds a SongPlayer token clears `not_ready_since` and fires
  a `RecoveryEvent`. The engine's `handle_resolume_recovery` re-pushes the
  title and the current display line.

## RecoveryEvent: exactly one re-push per recovery

`show_title` fades opacity from 5 % to 100 %, so a SECOND `ShowTitle` while the
title is up is a visible blink. Keep it to one event:

- `apply_outcome(true)` fires one after prior failures (`was_failing`). The
  ready transition in the same `refresh_mapping` call fires only when that one
  did not (`recovery_fired = consecutive_failures > 0`, read BEFORE
  `apply_outcome`).
- Opening the breaker clears `not_ready_since`. The `/product` probe that
  closes the breaker fires its own event BEFORE the breaker-closed refresh.
  The engine's commands queue on the driver's mpsc and run after that refresh,
  against the fresh map. A not-ready episode left over from before the outage
  would add a second event.
- The event is broadcast synchronously inside the driver call. Tests count
  events with `try_recv` right after the `.await` (see `drain` in
  `driver_not_ready_tests.rs`).

## Engine side (`playback/recovery.rs`)

`handle_resolume_recovery` sends straight on `resolume_tx`. It does NOT go
through the `last_resolume_subtitles_signature` dedup in
`playback/position_update.rs`, so a re-push is always delivered.

The dedup DOES record a push the driver skipped against an empty clip map
("no Resolume subtitle clips found … skipping push"). So recovery must re-send
the full current state: `ShowSubtitles` for a line, and `HideSubtitles` when
the display plan is blank. Otherwise a stale text Arena restored from its
saved composition stays until the next line change.

## Testing the driver on the no-compile box

- **One clock.** `on_tick_at(now)` → `run_full_refresh(reason, now)` →
  `refresh_mapping(now)` stamps `last_full_attempt_at`, `last_full_refresh_ok_at`
  and `not_ready_since` with the SAME synthetic `now`. Drive every policy test
  with `base + Duration` (never `Instant::now() - x`: that underflows on a
  freshly booted Windows runner). A real `Instant::now()` stamp mixed with
  synthetic ticks shifts every window by the test's runtime.
- **A composition sequence** is two mocks on the same path. Mount the first
  with `.up_to_n_times(1)` first; wiremock tries mocks in mount order, so it
  wins while it has a use left.
- **A "steady state" fixture must carry a SongPlayer clip.** `{"layers":[]}`
  is NOT READY and is fetched again on the next tick
  (`driver_poll_tests.rs::loaded_composition`).
- Pin exact fetch counts from a scratch Python model of `decide` + the tick
  loop. The storm test's 15 kills the `<` → `<=` window mutant; a `≤ 15` bound
  would not.
