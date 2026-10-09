---
paths:
  - "crates/sp-server/src/lib.rs"
  - "crates/sp-server/src/tools_ready.rs"
  - "crates/sp-server/src/api/websocket.rs"
  - "crates/sp-server/src/api/routes.rs"
---

# Status locks — never held across a slow await (#144)

`lib.rs::start` spawns tasks that publish state the API reads: `tools_status`,
`tool_paths`, `lan_status`, `obs_state`. A tokio `RwLock` guard bound with
`let mut ts = lock.write().await;` lives to the END OF ITS BLOCK, so every
`.await` after it in that block runs with the lock held. A tokio `RwLock` is
fair: a waiting writer also queues every later reader, so even a READ guard
held across a slow await stalls the writer and every status read behind it.

- **The incident.** The startup tools task kept `tools_status`'s write guard
  to the end of its `Ok(paths)` arm: through the yt-dlp self-update, the whole
  catalogue's sample-rate sweep, the startup sync and every worker spawn.
  `GET /api/v1/status` and a new dashboard socket (its first snapshot) waited
  for all of it after every start. The g35t spec's status poll escaped it only
  because it runs after the av-sync / dabing / flac specs.
- **The startup shape now.** `tools_ready::publish_then(sinks, paths, found,
  follow_ups)`. Its `publish` owns the guard and drops it on return; only then
  is the `follow_ups` future that `lib.rs` hands in awaited. A new slow startup
  step goes INSIDE that future. A new tools field is published in
  `tools_ready::publish`, never with a guard in `lib.rs`.
- **The socket shape now.** `api/websocket.rs::send_status` builds each first
  frame under its lock and sends it after the lock is released (a slow client
  with a full buffer must not hold `obs_state` / `tools_status`).
  `ToolsStatus::message` is the one `ToolsStatus` → `ServerMsg` mapping, and
  `send_json` the one sender of the snapshot, replay and error frames (the
  forwarding loop keeps its own send: it breaks on a failed one).
- **The status route shape now.** `api/routes.rs::status` copies the OBS
  flag and clones the tools and LAN status at its top, each guard a statement
  temporary, before its database waits (the playlist count, two settings, the
  metadata block). A new status field is read the same way, never with a
  guard bound across a query.
- **Any status lock.** Set or read the fields in a block or a fn that ends
  before the next unrelated `.await`. A temporary
  (`*lock.write().await = v;`, `lock.read().await.message()` in a `let`) is
  fine: the guard drops at the `;`. So is a guard dropped by hand before the
  await (the sync handler's `drop(paths)`).
- **Testing it.** Hold the slow step open (a oneshot, or a
  `futures::sink::unfold` client that waits on a semaphore per frame) and wait
  until it has started. Then assert `lock.try_write()` succeeds:
  deterministic, no timing, and it fails on a held read guard too. Only after
  that, drive the real route (`crate::api::router` + `oneshot`) under a
  generous bound (60 s); on a regression the `try_write` assert fails first,
  so the test never blocks on the route. From INSIDE the step, read with
  `try_read()`: the same task holding the write guard would deadlock on
  `.read().await`.
- **A handler: poll it ONCE by hand while the test holds the memory pool's
  only connection** (`test_state()`'s pool has one). Two statements, so the
  future stays alive through the checks:
  `let mut fut = std::pin::pin!(status(State(state.clone())));` then
  `assert!(futures::poll!(fut.as_mut()).is_pending());`. It parks on the
  pool. Change every status there through `try_write()` (a held guard fails
  it), drop the held connection, finish `fut` under a 60 s bound, and assert
  the answer carries the values from BEFORE the wait: that also catches a
  read moved back below the first query. Never
  `poll!(std::pin::pin!(fut).as_mut())` in one expression: the pinned
  temporary dies at the `;`, guards and all, and the checks after it pass on
  the buggy code. See `routes_tests.rs`
  (`the_status_route_holds_no_status_lock_while_it_waits_on_the_database`),
  `tools_ready.rs` and `websocket.rs`.

## The startup DB open waits out a lock; a failed server never leaves the shell up (#229)

A restart can start the new process while the old one still holds
`songplayer.db` (its shutdown checkpoint). `start()` opens it through
`db::startup_open::open`: a retryable error (the pool's 2 s acquire timing
out, SQLite BUSY/LOCKED by the low byte of the code) waits 1, 2, 4, 8, 15,
15, 15 s (60 s) and tries again, a WARN each; any other error is returned.
Normal requests keep `pool_tuning`'s 2 s acquire timeout. If `start()`
still fails, `src-tauri/src/lib.rs` logs it, waits 500 ms for the
non-blocking log writer, and EXITS 1: a live shell with no server would
keep the tray and the single-instance lock (a relaunch only focuses it)
while `:8920` refuses everything (PP, 9.10.2026 18:57Z → 19:06Z).

