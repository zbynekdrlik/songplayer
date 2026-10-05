---
paths:
  - "crates/sp-server/src/lib.rs"
  - "crates/sp-server/src/tools_ready.rs"
---

# Server startup tasks — a status lock is never held across a slow await (#144)

`lib.rs::start` spawns tasks that publish state the API reads: `tools_status`,
`tool_paths`, `lan_status`, `obs_state`. A tokio `RwLock` write guard bound
with `let mut ts = lock.write().await;` lives to the END OF ITS BLOCK, so every
`.await` after it in that block runs with every reader blocked.

- **The incident.** The startup tools task kept `tools_status`'s guard to the
  end of its `Ok(paths)` arm: through the yt-dlp self-update, the whole
  catalogue's sample-rate sweep, the startup sync and every worker spawn.
  `GET /api/v1/status` and a new dashboard socket (`api/websocket.rs`, its
  first snapshot) hung after every start. The post-deploy suite passed only
  because earlier specs ran first.
- **The shape now.** `tools_ready::publish_then(sinks, paths, found, follow_ups)`.
  Its `publish` owns the guard and drops it on return; only then is the
  `follow_ups` future that `lib.rs` hands in awaited. A new slow startup step
  goes INSIDE that future. A new tools field is published in
  `tools_ready::publish`, never with a guard in `lib.rs`.
- **Any status lock.** Set the fields in a block or a fn that ends before the
  next unrelated `.await`. A temporary `*lock.write().await = v;` is fine (the
  guard drops at the `;`), and so is a read guard dropped before the await
  (the sync handler's `drop(paths)`).
- **Testing it** (`tools_ready.rs` tests). Hold the slow step open with a
  oneshot and wait on a "started" oneshot. Then assert `lock.try_write()`
  succeeds: deterministic, no timing. Only after that, drive the real route
  (`crate::api::router` + `oneshot`) under a generous bound (60 s). On a
  regression the `try_write` assert fails first, so the test never blocks on
  the route. From INSIDE the follow-ups read with `try_read()`: the same task
  holding the write guard would deadlock on `.read().await`.
