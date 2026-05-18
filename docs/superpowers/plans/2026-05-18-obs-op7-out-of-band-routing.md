# OBS op=7 Out-of-Band Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop in-flight OBS request waiters from consuming legitimate op=5 scene-change events (issue #43).

**Architecture:** Split the OBS WebSocket connection into two halves. A dedicated reader task owns `SplitStream<read>` and routes every inbound message: op=5 events go to the existing `broadcast::Sender<ObsEvent>` (after internal scene-detection plumbing via an mpsc), op=7 responses are looked up by `requestId` in `Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>` and forwarded to the waiter. The main loop owns `SplitSink<write>` plus the dispatcher and issues requests by registering a oneshot before sending.

**Tech Stack:** Rust 2024, tokio (broadcast, mpsc, oneshot, Mutex), tokio-tungstenite, futures::SplitStream/SplitSink, serde_json.

**Source spec:** GitHub issue #43 body. Verified architecture and constraints from Explore agent walk-through of `crates/sp-server/src/obs/{mod,ndi_discovery,scene}.rs` and `crates/sp-server/tests/scene_detection.rs`.

---

## Airuleset rules (verbatim, non-negotiable)

- **TDD strict RED→GREEN per commit.** Failing test FIRST, run it (or describe the failure mode in the commit message when the harness cannot run locally), THEN implement, THEN commit. The bug-fix portion (Task 2 → Task 3) MUST appear in git history as a `test:` commit BEFORE the `fix:` commit.
- **Local commands allowed: ONLY `cargo fmt --all --check`.** No `cargo clippy`, `cargo test`, `cargo build`, `cargo check` locally. CI runs everything.
- **File cap 1000 lines per file.** Current sizes leave room: `obs/mod.rs` 691, `obs/ndi_discovery.rs` 341, `obs/scene.rs` 234, `tests/scene_detection.rs` 385. New code goes in a new sibling file `crates/sp-server/src/obs/dispatcher.rs`.
- **One commit per Task.** No bundling commits. Six tasks = six commits.
- **`mutants::skip` with inline justification only where unavoidable.** Justifications go in the source line above the attribute, mirroring `crates/sp-ndi/src/sender.rs:212` style.
- **No push between commits — controller pushes once after all six commits land.**
- **`LYRICS_PIPELINE_VERSION` untouched.** Unrelated subsystem.
- **`feedback_take_ownership.md`:** Root-cause fix only. The dispatcher is the structural fix; do NOT keep `wait_for_response` around as a shim.
- **`feedback_no_legacy_code.md`:** Delete both copies of `wait_for_response` entirely after the dispatcher lands. No deprecated stub. No `#[deprecated]` attribute. The whole function — and the duplicated impl in `scene.rs` — disappears.
- **`regression-test-first.md`:** Bug-fix commits MUST include `Closes #43` (the issue is labeled-equivalent to a regression — events dropped on the production code path). The Task 2 RED commit precedes the Task 3 GREEN commit. PR-level completion-report `✅ Regression test:` line is required.

---

## File structure

| File | Action | Why |
|---|---|---|
| `crates/sp-server/src/obs/dispatcher.rs` | **Create** | New module owning the pending-request map, the response router, and the awaiter API. Pure data-flow plumbing; no WebSocket types in the public API. |
| `crates/sp-server/src/obs/mod.rs` | Modify | Split connect_and_run into handshake → spawn reader task → run main write loop. Remove the hard-coded `GetCurrentProgramScene` op=7 branch (it becomes a dispatcher waiter like the others). Wire dispatcher into rebuild + scene + cmd paths. |
| `crates/sp-server/src/obs/ndi_discovery.rs` | Modify | `rebuild_ndi_source_map`, `fetch_ndi_input_names`, `fetch_input_ndi_sender_name` now take `&Dispatcher` instead of `&mut SplitStream`. Delete `wait_for_response` definition entirely. |
| `crates/sp-server/src/obs/scene.rs` | Modify | `check_scene_items`, `check_scene_items_recursive` take `&Dispatcher` instead of `&mut SplitStream`. Delete duplicated `wait_for_response`. |
| `crates/sp-server/tests/scene_detection.rs` | Modify | (Task 2) Add new RED regression test. (Task 4) Drop the `sleep(2500)` workaround comment + delay in `rebuild_failure_does_not_wipe_ndi_source_map`. |
| `crates/sp-server/tests/common/mod.rs` | Modify (small) | Add `FakeObsServer::send_unmatched_op7()` helper for the unmatched-op=7-warns test in Task 1 unit tests area. Optional — only if it cannot be triggered via existing surfaces. |

Module structure inside `dispatcher.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::warn;

/// Default per-call timeout for awaiting an op=7 response. Matches the
/// previous `wait_for_response` 2 s bound so steady-state timing
/// behavior is preserved. Callers that need a different timeout (e.g.
/// `GetSceneItemList` on a large scene) pass a custom `Duration` to
/// `await_response`.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Errors a dispatcher waiter may observe.
#[derive(Debug)]
pub enum DispatcherError {
    /// No response arrived within the per-call timeout.
    Timeout,
    /// The dispatcher was drained (connection closed) before the
    /// response arrived. Every pending waiter receives this error
    /// when the reader task exits.
    Closed,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

#[derive(Clone)]
pub struct Dispatcher {
    pending: Pending,
}

impl Dispatcher {
    pub fn new() -> Self { /* ... */ }

    /// Register a waiter for a future op=7 response with the given
    /// request_id. Returns the receiver half; the caller must await
    /// it (typically with `await_response` for the timeout wrapper).
    pub async fn register(&self, req_id: String) -> oneshot::Receiver<Value> { /* ... */ }

    /// Convenience: register + await + timeout. Returns the response
    /// JSON or a DispatcherError.
    pub async fn await_response(
        &self,
        req_id: String,
        timeout_dur: Duration,
    ) -> Result<Value, DispatcherError> { /* ... */ }

    /// Called by the reader task when an op=7 arrives. Looks up the
    /// pending waiter by request_id and forwards the value. If no
    /// waiter is registered, logs a warning and drops the payload.
    pub async fn complete(&self, req_id: &str, value: Value) { /* ... */ }

    /// Drains all pending waiters, dropping their oneshot::Sender.
    /// Each waiter sees `Err(_)` on its receiver. Called by the reader
    /// task on close so no waiter hangs forever.
    pub async fn drain_and_close(&self) { /* ... */ }
}
```

Internal mpsc between the reader task and the main loop (added to `obs/mod.rs`):

```rust
enum ReaderMessage {
    SceneEvent { scene_name: String },
    Closed,
}
```

---

## Task 1: Add `dispatcher` module — pending map, register/await/complete, unit tests

**Files:**
- Create: `crates/sp-server/src/obs/dispatcher.rs`
- Modify: `crates/sp-server/src/obs/mod.rs:3-5` (add `pub mod dispatcher;` next to existing `pub mod`s)

This task ships the dispatcher in isolation. Nothing in production wires it yet — it is dead code at the end of Task 1. Task 3 wires it up. Splitting Task 1 from Task 3 is justified because:

- The dispatcher has six pure-tokio unit tests that lock its semantics before it touches WebSocket code.
- A reviewer can read the public API and confirm it matches the issue body's design in a self-contained diff.

`#[allow(dead_code)]` is permitted at the module level for this commit only. Task 3 removes it.

### Step 1.1: Add the module declaration to `obs/mod.rs`

- [ ] **Step 1.1 — Modify `crates/sp-server/src/obs/mod.rs`**

In the module-declaration block at the top of `obs/mod.rs` (currently lines 3-5):

```rust
pub mod ndi_discovery;
pub mod scene;
pub mod text;
```

Add a fourth line so the block reads:

```rust
pub mod dispatcher;
pub mod ndi_discovery;
pub mod scene;
pub mod text;
```

### Step 1.2: Write the failing unit tests inside `dispatcher.rs`

- [ ] **Step 1.2 — Create `crates/sp-server/src/obs/dispatcher.rs` with the test module first**

Create the file with ONLY the test module (no `impl` yet). The file at end of step 1.2 looks like this:

```rust
//! Response dispatcher for the OBS WebSocket client.
//!
//! Owns a `request_id → oneshot::Sender<Value>` pending map. A single
//! reader task reads every inbound op=7 message and calls
//! `Dispatcher::complete(req_id, payload)`, which forwards the payload
//! to the waiter registered for that request_id. Callers register
//! before sending the request, send via the SplitSink half, and await
//! the receiver with `await_response`.
//!
//! Replaces the per-call `wait_for_response` helpers in
//! `ndi_discovery.rs` and `scene.rs` that consumed and dropped any
//! op=5 event arriving while a request was in flight (issue #43).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::warn;

pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum DispatcherError {
    Timeout,
    Closed,
}

#[derive(Clone, Default)]
pub struct Dispatcher {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
}

// Implementation lives in step 1.3. The tests below MUST FAIL to
// compile or fail at runtime against the empty struct.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn register_and_complete_delivers_payload() {
        let d = Dispatcher::default();
        let rx = d.register("req-1".to_string()).await;

        let payload = json!({"op": 7, "d": {"requestId": "req-1", "ok": true}});
        d.complete("req-1", payload.clone()).await;

        let got = rx.await.expect("oneshot must deliver");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn await_response_returns_payload_on_complete() {
        let d = Dispatcher::default();
        let d2 = d.clone();

        let waiter = tokio::spawn(async move {
            d2.await_response("req-2".to_string(), Duration::from_secs(1)).await
        });

        // Give the spawn time to register.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let payload = json!({"op": 7, "d": {"requestId": "req-2"}});
        d.complete("req-2", payload.clone()).await;

        let result = waiter.await.expect("task joins").expect("ok");
        assert_eq!(result, payload);
    }

    #[tokio::test]
    async fn await_response_returns_timeout_when_no_response() {
        let d = Dispatcher::default();
        let result = d
            .await_response("req-3".to_string(), Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(DispatcherError::Timeout)));
    }

    #[tokio::test]
    async fn drain_and_close_fails_all_pending() {
        let d = Dispatcher::default();
        let rx_a = d.register("a".to_string()).await;
        let rx_b = d.register("b".to_string()).await;

        d.drain_and_close().await;

        assert!(rx_a.await.is_err(), "drain must close oneshot");
        assert!(rx_b.await.is_err(), "drain must close oneshot");
    }

    #[tokio::test]
    async fn complete_with_no_waiter_does_not_panic() {
        // Unmatched op=7 (sender gave up / timed out) MUST be a soft
        // case — log + drop, not crash.
        let d = Dispatcher::default();
        d.complete("nobody-cares", json!({})).await;
        // If we reach here without panicking the test passes.
    }

    #[tokio::test]
    async fn double_register_replaces_previous_waiter() {
        // Same request_id registered twice — the first waiter sees an
        // error on the dropped sender, the second receives the value.
        // This shape can only happen if a caller reuses an ID by
        // mistake; we want it to fail loudly on the first waiter,
        // not silently merge the channels.
        let d = Dispatcher::default();
        let rx_first = d.register("dup".to_string()).await;
        let rx_second = d.register("dup".to_string()).await;

        d.complete("dup", json!("v")).await;

        assert!(rx_first.await.is_err(), "first waiter must fail (sender dropped)");
        assert_eq!(rx_second.await.expect("second wins"), json!("v"));
    }
}
```

Expected at end of step 1.2: the file compiles but the tests must fail (the impl block is empty). The implementer-subagent MAY skip running anything locally per the rules — the RED state is established by inspection of the empty `impl` block.

### Step 1.3: Implement the dispatcher

- [ ] **Step 1.3 — Add the `impl Dispatcher` block to `dispatcher.rs`**

Insert this block AFTER the `Dispatcher` struct definition and BEFORE the `#[cfg(test)]` module:

```rust
impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for a future op=7 response with the given
    /// `request_id`. Returns the receiver half; the caller awaits it
    /// (typically via `await_response`).
    ///
    /// If a waiter is already registered for the same `request_id`,
    /// it is replaced. The previous oneshot::Sender is dropped and
    /// its receiver observes an `Err(_)`. Callers MUST generate
    /// unique UUID request IDs (all current call sites already do
    /// via `uuid::Uuid::new_v4`).
    pub async fn register(&self, req_id: String) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().await;
        guard.insert(req_id, tx);
        rx
    }

    /// Register + await + per-call timeout. Returns the response JSON
    /// on success, `Timeout` if `timeout_dur` elapses before a
    /// matching op=7 arrives, or `Closed` if the dispatcher was
    /// drained while the call was pending.
    ///
    /// On timeout the registration is removed so a late-arriving
    /// response does not leak into the pending map.
    pub async fn await_response(
        &self,
        req_id: String,
        timeout_dur: Duration,
    ) -> Result<Value, DispatcherError> {
        let rx = self.register(req_id.clone()).await;
        match timeout(timeout_dur, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(DispatcherError::Closed),
            Err(_) => {
                // Remove our stale entry so the reader task does not
                // try to deliver into a dropped channel later.
                let mut guard = self.pending.lock().await;
                guard.remove(&req_id);
                Err(DispatcherError::Timeout)
            }
        }
    }

    /// Forward an inbound op=7 payload to the registered waiter, if
    /// any. An unmatched response is logged at WARN — it means the
    /// sender gave up (timed out, was dropped) before the response
    /// arrived.
    pub async fn complete(&self, req_id: &str, value: Value) {
        let mut guard = self.pending.lock().await;
        match guard.remove(req_id) {
            Some(tx) => {
                // If the receiver was already dropped (e.g. caller
                // raced past its timeout), send returns Err — that's
                // a soft case, not a panic.
                let _ = tx.send(value);
            }
            None => {
                warn!(
                    request_id = req_id,
                    "OBS dispatcher: received op=7 with no registered \
                     waiter (sender likely timed out earlier)"
                );
            }
        }
    }

    /// Drop every pending waiter so its receiver observes `Err(_)`.
    /// Called by the reader task on connection close so no caller
    /// hangs on a never-arriving response.
    pub async fn drain_and_close(&self) {
        let mut guard = self.pending.lock().await;
        guard.clear();
    }
}
```

### Step 1.4: Verify formatting

- [ ] **Step 1.4 — Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: no diff. If there is a diff, run `cargo fmt --all` and re-check. This is the ONLY local cargo command allowed.

### Step 1.5: Commit

- [ ] **Step 1.5 — Commit Task 1**

```bash
git add crates/sp-server/src/obs/dispatcher.rs crates/sp-server/src/obs/mod.rs
git commit -m "$(cat <<'EOF'
feat(obs): add ResponseDispatcher for op=7 routing (#43)

Pure data-flow plumbing: pending HashMap<request_id, oneshot::Sender>,
register/await_response/complete/drain_and_close. No WebSocket types
in the public API. Dead code at this point — Task 3 wires it in.

Six unit tests pin the semantics:
- register + complete delivers the payload
- await_response returns the payload after complete
- await_response returns Timeout when no response arrives
- drain_and_close fails every pending waiter
- complete with no waiter logs warn and does not panic
- double-register replaces previous waiter (id collision is loud)

EOF
)"
```

---

## Task 2: RED regression test — events must not be eaten by in-flight requests

**Files:**
- Modify: `crates/sp-server/tests/scene_detection.rs` (append a new `#[tokio::test]`)

This is the bug-fix RED commit per `regression-test-first.md`. The test exercises the production path and MUST fail against the current code: a `CurrentProgramSceneChanged` event arriving while `wait_for_response` is consuming the read stream gets dropped.

The test relies on `FakeObsServer::suppress_get_input_list = true` (already present) to keep the rebuild-loop's `wait_for_response` open for the full 2-second timeout. While it is open the test pushes a scene-change event and asserts the event arrives at the subscriber within 500 ms — far less than the 2 s wait window. Against the buggy code, the event is consumed by `wait_for_response` and the assertion times out.

### Step 2.1: Append the failing test

- [ ] **Step 2.1 — Add test `event_during_pending_request_must_be_delivered_fast` to `tests/scene_detection.rs`**

Append at the end of the file (after the last `}` of `rebuild_failure_does_not_wipe_ndi_source_map`):

```rust
/// Issue #43 regression: a `CurrentProgramSceneChanged` event that
/// arrives WHILE an op=7 waiter (`wait_for_response`) is reading the
/// shared stream MUST be delivered to subscribers. The buggy code
/// consumed and dropped the event inside `wait_for_response`.
///
/// Setup:
/// 1. Spawn FakeObsServer with `suppress_get_input_list = true` so
///    every GetInputList request hangs without a reply. This keeps
///    the rebuild-helper's `wait_for_response` actively reading the
///    stream for the full 2 s timeout.
/// 2. Wait for the client to connect and run its initial rebuild
///    (which immediately times out per the suppression flag).
/// 3. Trigger another rebuild explicitly via the broadcast — this
///    opens a fresh 2 s `wait_for_response` window.
/// 4. While that window is open, push a `CurrentProgramSceneChanged`
///    event.
/// 5. Assert that `ObsEvent::SceneChanged` is observed within 500 ms.
///
/// Against the buggy code the event is consumed by `wait_for_response`
/// and the assertion fails (3 s timeout). After the fix it arrives
/// immediately because the reader task routes op=5 separately from
/// op=7.
#[tokio::test]
async fn event_during_pending_request_must_be_delivered_fast() {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (7, 'ytfast', 'https://yt/f', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut fake_state = FakeObsState::default();
    fake_state
        .inputs
        .insert("sp-fast_video".into(), "ndi_source".into());
    fake_state.input_settings.insert(
        "sp-fast_video".into(),
        serde_json::json!({ "ndi_source_name": "RESOLUME-SNV (SP-fast)" }),
    );
    fake_state.scene_items.insert(
        "sp-fast".into(),
        vec![("sp-fast_video".into(), false, "ndi_source".into())],
    );
    // Force every GetInputList to hang for its full timeout. This
    // creates the "stream is being read by wait_for_response" window
    // that the bug requires.
    fake_state.suppress_get_input_list = true;

    let fake_obs = FakeObsServer::spawn_with_state(fake_state).await;

    let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
    let obs_state = Arc::new(RwLock::new(obs::ObsState::default()));
    let (obs_event_tx, mut obs_event_rx) = broadcast::channel::<obs::ObsEvent>(16);
    let (obs_rebuild_tx, obs_rebuild_rx) = broadcast::channel::<()>(4);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    let _client = obs::ObsClient::spawn(
        obs::ObsConfig {
            url: fake_obs.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        obs_event_tx.clone(),
        obs_rebuild_rx,
        shutdown_rx,
    );

    // Wait for connect.
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if obs_state.read().await.connected {
            break;
        }
        if std::time::Instant::now() > connect_deadline {
            panic!("ObsClient did not connect within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Let the initial rebuild attempts run + time out. The startup
    // path retries 5x with 2s spacing = up to 10s of in-flight
    // wait_for_response windows. We don't need to wait that whole
    // window — we trigger our own fresh rebuild below.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Drain any startup events.
    while obs_event_rx.try_recv().is_ok() {}

    // Open a fresh wait_for_response window by firing a rebuild
    // signal. The fake OBS will not respond, so wait_for_response
    // sits on read.next() for the full 2 s timeout.
    let _ = obs_rebuild_tx.send(());

    // Wait a small slice so the rebuild has definitely started its
    // wait_for_response loop, but well before the 2 s timeout.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Push the scene-change event INTO the window. Against the buggy
    // code this gets consumed and dropped by wait_for_response.
    fake_obs.push_program_scene_change("sp-fast").await;

    // The event MUST arrive within 500 ms — far less than the 2 s
    // wait window. After the fix the reader task delivers it
    // immediately.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let active_ids = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, obs_event_rx.recv()).await {
            Ok(Ok(obs::ObsEvent::SceneChanged {
                scene_name,
                active_playlist_ids,
            })) if scene_name == "sp-fast" => break active_playlist_ids,
            Ok(Ok(_other)) => continue,
            Ok(Err(e)) => panic!("event channel error: {e}"),
            Err(_) => panic!(
                "SceneChanged for sp-fast NOT delivered within 500ms — \
                 event was eaten by in-flight wait_for_response. \
                 This is the #43 regression."
            ),
        }
    };

    assert!(
        active_ids.contains(&7),
        "active_playlist_ids should contain 7, got {active_ids:?}"
    );

    let _ = shutdown_tx.send(());
    fake_obs.shutdown().await;
}
```

### Step 2.2: Confirm RED state (by inspection)

- [ ] **Step 2.2 — Inspect the test against the current code**

The implementer subagent verifies the RED state by:
- Reading `crates/sp-server/src/obs/ndi_discovery.rs::wait_for_response` (lines 242-274).
- Confirming the loop calls `read.next().await` and discards every message whose `op` is not 7 OR whose `requestId` does not match.
- Noting that an op=5 message arriving in step 2.1's push window matches neither condition → it is dropped → the `obs_event_rx.recv()` in the test never receives it within 500 ms → the test fails on the `Err(_) => panic!` arm.

No `cargo test` run is required; the failure mode is mechanically obvious from the source. The commit message records this RED reasoning.

### Step 2.3: Verify formatting

- [ ] **Step 2.3 — Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: no diff.

### Step 2.4: Commit

- [ ] **Step 2.4 — Commit Task 2**

```bash
git add crates/sp-server/tests/scene_detection.rs
git commit -m "$(cat <<'EOF'
test(obs): RED — events must not be eaten by in-flight requests (#43)

Adds event_during_pending_request_must_be_delivered_fast — pushes a
CurrentProgramSceneChanged while wait_for_response is reading the
shared SplitStream. Asserts ObsEvent::SceneChanged arrives within
500 ms.

Against the current code wait_for_response consumes and drops the
event (ndi_discovery.rs:242-274), the recv() at the end times out
at 500 ms, and the test panics on the "event was eaten" arm. The
GREEN commit (Task 3) makes this test pass by routing op=5 events
through a dedicated reader task separate from the op=7 waiters.

EOF
)"
```

---

## Task 3: GREEN — wire dispatcher through reader task; refactor every helper

**Files:**
- Modify: `crates/sp-server/src/obs/mod.rs` — split connect_and_run, spawn reader task, drop the hard-coded GetCurrentProgramScene op=7 branch
- Modify: `crates/sp-server/src/obs/ndi_discovery.rs` — helpers take `&Dispatcher`, delete `wait_for_response`
- Modify: `crates/sp-server/src/obs/scene.rs` — helpers take `&Dispatcher`, delete `wait_for_response`
- Modify: `crates/sp-server/src/obs/dispatcher.rs` — remove the `#![allow(dead_code)]`

This is one large refactor commit. Intermediate states (e.g. half-refactored helpers) do not compile, so splitting Task 3 across multiple commits is impossible without temporary scaffolding that violates `feedback_no_legacy_code.md`. The commit closes #43.

### Step 3.1: Replace `ndi_discovery.rs` helpers' signatures

- [ ] **Step 3.1 — Modify `crates/sp-server/src/obs/ndi_discovery.rs`**

Replace the file's `use` block (lines 9-20) with:

```rust
use std::collections::HashMap;

use futures::SinkExt;
use futures::stream::SplitSink;
use sqlx::{Row, SqlitePool};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::text::{get_input_list_request, get_input_settings_request};
```

`StreamExt` and `SplitStream` are no longer needed — only `SinkExt` + `SplitSink` for `write`.

Replace `rebuild_ndi_source_map` (lines 49-126) with:

```rust
pub async fn rebuild_ndi_source_map(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
    pool: &SqlitePool,
) -> Option<HashMap<String, i64>> {
    let mut map = HashMap::new();

    let by_ndi_name = match load_playlist_ndi_names(pool).await {
        Ok(m) => m,
        Err(e) => {
            warn!("rebuild_ndi_source_map: failed to load playlists: {e}");
            return None;
        }
    };

    if by_ndi_name.is_empty() {
        debug!("rebuild_ndi_source_map: no active playlists with ndi_output_name");
        return Some(map);
    }

    let input_names = match fetch_ndi_input_names(write, dispatcher).await {
        Some(names) => names,
        None => {
            warn!(
                "rebuild_ndi_source_map: GetInputList returned nothing; \
                 keeping previous map so scene detection stays alive"
            );
            return None;
        }
    };

    if input_names.is_empty() {
        debug!("rebuild_ndi_source_map: OBS has no NDI source inputs");
        return Some(map);
    }

    for input_name in input_names {
        let sender_name = match fetch_input_ndi_sender_name(write, dispatcher, &input_name).await {
            Some(s) => s,
            None => {
                debug!(
                    "rebuild_ndi_source_map: input '{input_name}' has no ndi_source_name setting"
                );
                continue;
            }
        };

        let stream_name = extract_ndi_stream_name(&sender_name);

        if let Some(&playlist_id) = by_ndi_name.get(stream_name) {
            debug!(
                "rebuild_ndi_source_map: '{input_name}' → playlist {playlist_id} (NDI sender '{sender_name}', stream '{stream_name}')"
            );
            map.insert(input_name, playlist_id);
        } else {
            debug!(
                "rebuild_ndi_source_map: no playlist matches NDI sender '{sender_name}' (stream '{stream_name}')"
            );
        }
    }

    info!(count = map.len(), "rebuilt NDI source map from OBS + DB");
    Some(map)
}
```

Replace `fetch_ndi_input_names` (lines 189-208) with:

```rust
async fn fetch_ndi_input_names(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
) -> Option<Vec<String>> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_list_request(&req_id);
    let rx = dispatcher.register(req_id.clone()).await;
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("fetch_ndi_input_names: send GetInputList failed: {e}");
        return None;
    }

    let response = match tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            warn!("fetch_ndi_input_names: dispatcher closed before reply");
            return None;
        }
        Err(_) => {
            warn!("fetch_ndi_input_names: GetInputList timed out");
            return None;
        }
    };
    let arr = response["d"]["responseData"]["inputs"].as_array()?;

    Some(
        arr.iter()
            .filter_map(|v| v["inputName"].as_str().map(|s| s.to_string()))
            .collect(),
    )
}
```

Replace `fetch_input_ndi_sender_name` (lines 213-229) with:

```rust
async fn fetch_input_ndi_sender_name(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
    input_name: &str,
) -> Option<String> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_settings_request(&req_id, input_name);
    let rx = dispatcher.register(req_id.clone()).await;
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("fetch_input_ndi_sender_name: send GetInputSettings failed for {input_name}: {e}");
        return None;
    }

    let response = match tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            warn!(
                "fetch_input_ndi_sender_name: dispatcher closed before reply for {input_name}"
            );
            return None;
        }
        Err(_) => {
            warn!("fetch_input_ndi_sender_name: GetInputSettings timed out for {input_name}");
            return None;
        }
    };
    response["d"]["responseData"]["inputSettings"]["ndi_source_name"]
        .as_str()
        .map(|s| s.to_string())
}
```

Delete entirely (no replacement, no stub):
- The `WAIT_FOR_RESPONSE_TIMEOUT` constant (lines 22-27 of the original — it now lives in `dispatcher::DEFAULT_RESPONSE_TIMEOUT`).
- The `wait_for_response` function (lines 242-274 of the original).

The file shrinks; the existing `extract_ndi_stream_name` + `load_playlist_ndi_names` + their tests remain untouched.

### Step 3.2: Refactor `scene.rs` to take `&Dispatcher`

- [ ] **Step 3.2 — Modify `crates/sp-server/src/obs/scene.rs`**

Replace the `use` block (lines 3-12) with:

```rust
use std::collections::{HashMap, HashSet};

use futures::SinkExt;
use futures::stream::SplitSink;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::text::get_scene_items_request;
```

Replace `check_scene_items` (lines 18-27) with:

```rust
pub async fn check_scene_items(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
) -> HashSet<i64> {
    let mut active_ids = HashSet::new();
    check_scene_items_recursive(write, dispatcher, scene_name, ndi_sources, &mut active_ids, 0)
        .await;
    active_ids
}
```

Replace `check_scene_items_recursive` (lines 32-97) with:

```rust
async fn check_scene_items_recursive(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
    active_ids: &mut HashSet<i64>,
    depth: u32,
) {
    if depth >= MAX_RECURSION_DEPTH {
        warn!("max scene recursion depth reached for '{scene_name}'");
        return;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let req = get_scene_items_request(&request_id, scene_name);

    let rx = dispatcher.register(request_id.clone()).await;
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("failed to send GetSceneItemList: {e}");
        return;
    }

    let items = match tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            warn!("no response for GetSceneItemList request (dispatcher closed)");
            return;
        }
        Err(_) => {
            warn!("timed out waiting for GetSceneItemList for '{scene_name}'");
            return;
        }
    };

    let scene_items = match items["d"]["responseData"]["sceneItems"].as_array() {
        Some(arr) => arr,
        None => return,
    };

    for item in scene_items {
        let source_name = match item["sourceName"].as_str() {
            Some(name) => name,
            None => continue,
        };

        if let Some(&playlist_id) = ndi_sources.get(source_name) {
            debug!("found NDI source '{source_name}' (playlist {playlist_id}) in '{scene_name}'");
            active_ids.insert(playlist_id);
        }

        let is_group = item["isGroup"].as_bool().unwrap_or(false);
        let input_kind = item["inputKind"].as_str().unwrap_or("");
        let is_scene_source = input_kind == "scene" || is_group;

        if is_scene_source {
            debug!("recursing into nested scene/group '{source_name}'");
            Box::pin(check_scene_items_recursive(
                write,
                dispatcher,
                source_name,
                ndi_sources,
                active_ids,
                depth + 1,
            ))
            .await;
        }
    }
}
```

Delete entirely:
- The duplicated `wait_for_response` function (lines 99-146 of the original).
- The `use futures::StreamExt;` (no longer needed — only `SinkExt` survives).

### Step 3.3: Refactor `mod.rs` — split connect_and_run, spawn reader task

- [ ] **Step 3.3 — Modify `crates/sp-server/src/obs/mod.rs`**

The current `connect_and_run` (lines 211-370) does handshake + initial rebuild + main loop in one coroutine. After this step:
- Handshake stays in `connect_and_run`.
- After Identified, `connect_and_run` constructs the `Dispatcher`, spawns the reader task with the `read` half, the dispatcher, an internal `mpsc::Sender<ReaderMessage>`, and a clone of `event_tx`.
- `connect_and_run` then runs a new main loop that owns `write`, `dispatcher`, `cmd_rx`, `rebuild_rx`, and `reader_rx`.

Add a new internal type at the module level (insert near `ObsCommand`, after line 78):

```rust
/// Internal messages from the reader task to the main loop.
enum ReaderMessage {
    /// `CurrentProgramSceneChanged` arrived. Main loop must issue
    /// follow-up GetSceneItemList queries (via dispatcher) and emit
    /// the upstream `ObsEvent::SceneChanged`.
    SceneChange { scene_name: String },
    /// Stream closed cleanly OR errored. Main loop must exit so the
    /// outer reconnect loop fires.
    Closed,
}
```

Insert a `use` for `oneshot` is not needed — the dispatcher hides it. The existing `tokio::sync::{RwLock, broadcast, mpsc}` import already covers `mpsc`.

Add to the existing `use` block at top of file (line 17):

```rust
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
```

Add a new helper function below `compute_auth` (insert before the current `connect_and_run`):

```rust
/// Reader task: owns the SplitStream<read> after handshake. Reads
/// every inbound message and routes it: op=5 events go to the
/// internal mpsc → main loop (which then issues follow-up queries
/// via the dispatcher), op=7 responses go to the dispatcher's
/// pending map. Other op codes are debug-logged.
///
/// Exits when the WebSocket closes or read errors. Always sends
/// `ReaderMessage::Closed` and calls `dispatcher.drain_and_close()`
/// before returning so no waiter hangs forever and the main loop
/// drops cleanly.
async fn run_reader_task(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    dispatcher: Dispatcher,
    reader_tx: mpsc::Sender<ReaderMessage>,
) {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                let json: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("OBS reader: invalid JSON: {e}");
                        continue;
                    }
                };
                let op = json["op"].as_u64().unwrap_or(u64::MAX);
                match op {
                    5 => {
                        let event_type = json["d"]["eventType"].as_str().unwrap_or("");
                        debug!("OBS event: {event_type}");
                        if event_type == "CurrentProgramSceneChanged"
                            && let Some(scene_name) =
                                json["d"]["eventData"]["sceneName"].as_str()
                        {
                            let _ = reader_tx
                                .send(ReaderMessage::SceneChange {
                                    scene_name: scene_name.to_string(),
                                })
                                .await;
                        }
                    }
                    7 => {
                        let req_id = json["d"]["requestId"].as_str().unwrap_or("").to_string();
                        if req_id.is_empty() {
                            warn!("OBS reader: op=7 without requestId, dropping");
                            continue;
                        }
                        dispatcher.complete(&req_id, json).await;
                    }
                    _ => {
                        debug!("unhandled OBS message op={op}");
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                info!("OBS WebSocket closed");
                break;
            }
            Some(Ok(_)) => {} // ping/pong/binary
            Some(Err(e)) => {
                warn!("OBS reader: stream error: {e}");
                break;
            }
        }
    }

    dispatcher.drain_and_close().await;
    let _ = reader_tx.send(ReaderMessage::Closed).await;
}
```

Replace `connect_and_run` (lines 211-370) with the refactored version below. The handshake part is preserved verbatim; everything after `Identified` is rewritten:

```rust
async fn connect_and_run(
    config: &ObsConfig,
    pool: &SqlitePool,
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
    cmd_rx: &mut mpsc::Receiver<ObsCommand>,
    rebuild_rx: &mut broadcast::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Step 1: Hello (op 0).
    let hello = read_json_message(&mut read).await?;
    let op = hello["op"].as_u64().unwrap_or(u64::MAX);
    if op != 0 {
        anyhow::bail!("expected Hello (op 0), got op {op}");
    }
    debug!("received OBS Hello");

    // Step 2: Identify (op 1).
    let mut identify_data = serde_json::json!({
        "rpcVersion": 1,
        "eventSubscriptions": 4  // Scenes events
    });
    if let Some(password) = &config.password
        && let Some(auth) = hello["d"]["authentication"].as_object()
    {
        let challenge = auth
            .get("challenge")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing auth challenge"))?;
        let salt = auth
            .get("salt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing auth salt"))?;
        identify_data["authentication"] =
            serde_json::Value::String(compute_auth(password, challenge, salt));
    }
    let identify_msg = serde_json::json!({"op": 1, "d": identify_data});
    write
        .send(Message::Text(identify_msg.to_string().into()))
        .await?;

    // Step 3: Identified (op 2).
    let identified = read_json_message(&mut read).await?;
    let op = identified["op"].as_u64().unwrap_or(u64::MAX);
    if op != 2 {
        anyhow::bail!("expected Identified (op 2), got op {op}");
    }
    info!("connected to OBS WebSocket");

    {
        let mut s = state.write().await;
        s.connected = true;
    }
    let _ = event_tx.send(ObsEvent::Connected);

    // Step 4: build dispatcher + spawn reader task.
    let dispatcher = Dispatcher::new();
    let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderMessage>(32);
    let reader_handle = tokio::spawn(run_reader_task(read, dispatcher.clone(), reader_tx));

    // Step 5: initial NDI source map rebuild (same retry-on-empty
    // policy as before — the rebuild now goes via the dispatcher).
    for attempt in 1..=5 {
        let result = rebuild_ndi_source_map(&mut write, &dispatcher, pool).await;
        let is_empty = result.as_ref().map(|m| m.is_empty()).unwrap_or(true);
        apply_rebuild_result(ndi_sources, result).await;
        if !is_empty {
            break;
        }
        if attempt < 5 {
            warn!("NDI source map empty after rebuild (attempt {attempt}/5); retrying in 2s");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        } else {
            warn!(
                "NDI source map still empty after 5 rebuild attempts — scene \
                 detection will not work until the next external rebuild signal"
            );
        }
    }

    // Step 6: initial GetCurrentProgramScene via dispatcher (was a
    // hard-coded op=7 branch in handle_message; now a normal waiter).
    let initial_scene_req_id = uuid::Uuid::new_v4().to_string();
    let initial_scene_req = get_current_scene_request(&initial_scene_req_id);
    let initial_scene_rx = dispatcher.register(initial_scene_req_id.clone()).await;
    write
        .send(Message::Text(initial_scene_req.to_string().into()))
        .await?;
    if let Ok(Ok(response)) =
        tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, initial_scene_rx).await
        && let Some(scene_name) =
            response["d"]["responseData"]["currentProgramSceneName"].as_str()
    {
        let sources = ndi_sources.read().await;
        let active_ids =
            check_scene_items(&mut write, &dispatcher, scene_name, &sources).await;
        drop(sources);

        let mut s = state.write().await;
        s.current_scene = Some(scene_name.to_string());
        s.active_playlist_ids = active_ids.clone();
        drop(s);

        let _ = event_tx.send(ObsEvent::SceneChanged {
            scene_name: scene_name.to_string(),
            active_playlist_ids: active_ids,
        });
    } else {
        debug!("initial GetCurrentProgramScene did not return a scene name");
    }

    // Step 7: main loop — write side + reader-event side.
    let result = loop {
        tokio::select! {
            reader_msg = reader_rx.recv() => {
                match reader_msg {
                    Some(ReaderMessage::SceneChange { scene_name }) => {
                        let sources = ndi_sources.read().await;
                        let active_ids =
                            check_scene_items(&mut write, &dispatcher, &scene_name, &sources)
                                .await;
                        drop(sources);

                        let mut s = state.write().await;
                        s.current_scene = Some(scene_name.clone());
                        s.active_playlist_ids = active_ids.clone();
                        drop(s);

                        let _ = event_tx.send(ObsEvent::SceneChanged {
                            scene_name,
                            active_playlist_ids: active_ids,
                        });
                    }
                    Some(ReaderMessage::Closed) | None => {
                        break Ok(());
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ObsCommand::SetTextSource { source_name, text } => {
                        // Fire-and-forget: SetInputSettings replies
                        // with an op=7 success/failure status. We
                        // don't currently surface it. Task 5
                        // tightens this to wait + log status.
                        let req_id = uuid::Uuid::new_v4().to_string();
                        let req = text::set_text_request(&req_id, &source_name, &text);
                        write.send(Message::Text(req.to_string().into())).await?;
                        info!(source_name, "sent SetTextSource to OBS");
                    }
                }
            }
            rebuild_result = rebuild_rx.recv() => {
                match rebuild_result {
                    Ok(()) => {
                        debug!("received rebuild signal, refreshing NDI source map");
                        apply_rebuild_result(
                            ndi_sources,
                            rebuild_ndi_source_map(&mut write, &dispatcher, pool).await,
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            "rebuild signal channel lagged by {n} messages, \
                             refreshing NDI source map once"
                        );
                        apply_rebuild_result(
                            ndi_sources,
                            rebuild_ndi_source_map(&mut write, &dispatcher, pool).await,
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed — outer shutdown will catch it.
                    }
                }
            }
        }
    };

    // Reader task may still be alive on the OK path (clean close); on
    // the Err path it might already be gone. Either way the
    // dispatcher gets drained when the task exits.
    reader_handle.abort();
    result
}
```

Delete entirely the `handle_message` function (lines 372-432 of the original) — its work has moved into the reader task (op=5 routing) plus the dispatcher (op=7 routing).

### Step 3.4: Remove the `#![allow(dead_code)]` from `dispatcher.rs`

- [ ] **Step 3.4 — Edit `crates/sp-server/src/obs/dispatcher.rs`**

Remove the line:

```rust
#![allow(dead_code)]
```

The dispatcher is fully used as of this commit.

### Step 3.5: Verify formatting

- [ ] **Step 3.5 — Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: no diff. Run `cargo fmt --all` and re-check if diff appears.

### Step 3.6: Commit

- [ ] **Step 3.6 — Commit Task 3**

```bash
git add crates/sp-server/src/obs/ \
        crates/sp-server/src/obs/dispatcher.rs \
        crates/sp-server/src/obs/mod.rs \
        crates/sp-server/src/obs/ndi_discovery.rs \
        crates/sp-server/src/obs/scene.rs
git commit -m "$(cat <<'EOF'
fix(obs): route op=7 out-of-band via ResponseDispatcher (#43)

Closes #43

Before: every wait_for_response loop read from the shared
SplitStream<read> and discarded any non-matching op=7 OR any op=5
event arriving in the window. A CurrentProgramSceneChanged that
happened to land mid-request was silently dropped, leaving the
upstream broadcast::Receiver waiting forever (or, after the 2 s
hard cap, never).

After: a dedicated reader task owns SplitStream<read> after the
handshake completes. It dispatches op=5 events through a private
mpsc to the main loop (which then issues follow-up scene queries
via the dispatcher), and forwards op=7 responses to the matching
oneshot waiter registered in the dispatcher's pending map. The
main loop owns SplitSink<write>; helpers (rebuild_ndi_source_map,
fetch_ndi_input_names, fetch_input_ndi_sender_name,
check_scene_items_recursive) take &Dispatcher and register a
oneshot before sending each request.

Removed:
- crates/sp-server/src/obs/ndi_discovery.rs::wait_for_response
- crates/sp-server/src/obs/scene.rs::wait_for_response (duplicate)
- mod.rs::handle_message's hard-coded GetCurrentProgramScene op=7
  branch (initial scene fetch now goes through the dispatcher like
  every other request)

Verified RED→GREEN: Task 2's
event_during_pending_request_must_be_delivered_fast moves from
"event eaten by wait_for_response → 500 ms timeout panic" to
"reader routes op=5 to mpsc → event delivered immediately".

EOF
)"
```

---

## Task 4: Drop sleep(2500) workaround from existing rebuild-failure test

**Files:**
- Modify: `crates/sp-server/tests/scene_detection.rs:333-353` (the comment + sleep + downstream assert in `rebuild_failure_does_not_wipe_ndi_source_map`)

The Task 3 fix means the test no longer needs to wait past the 2 s `wait_for_response` window for the scene event to land — events arrive immediately regardless of whether a request is in flight.

### Step 4.1: Replace the workaround block

- [ ] **Step 4.1 — Modify `crates/sp-server/tests/scene_detection.rs`**

Find this block (lines 333-341 of the current file):

```rust
    // Wait past the wait_for_response timeout (2s) so the rebuild-
    // retry loop has returned None and the main event loop is back
    // to reading messages. If we pushed the scene event during the
    // 2s wait, it would be consumed and dropped by wait_for_response.
    // That's a narrower race than the original "forever hang" but
    // still present; out-of-band routing of responses vs events is
    // a separate refactor (tracked as a TODO).
    tokio::time::sleep(Duration::from_millis(2500)).await;
```

Replace with:

```rust
    // No sleep needed: after #43 the reader task routes op=5 events
    // separately from op=7 waiters, so a scene-change pushed mid-
    // rebuild is delivered immediately. A 200 ms grace lets the
    // failing rebuild's GetInputList timeout fire so the assertion
    // below sees the preserved-on-None outcome cleanly.
    tokio::time::sleep(Duration::from_millis(200)).await;
```

The rest of the test (the map-preserved assertion, the scene-change push, the active-playlists assertion) is unchanged. The test still verifies the 2026-04-19 regression AND now also verifies that the failed rebuild does not break event delivery — same coverage with a shorter wall-clock.

### Step 4.2: Verify formatting

- [ ] **Step 4.2 — Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: no diff.

### Step 4.3: Commit

- [ ] **Step 4.3 — Commit Task 4**

```bash
git add crates/sp-server/tests/scene_detection.rs
git commit -m "$(cat <<'EOF'
refactor(obs): drop sleep(2500) workaround from rebuild-failure test

After #43 the reader task delivers op=5 events independent of any
op=7 waiter state, so the test no longer needs to wait past the
2 s wait_for_response window to push a scene-change. 200 ms grace
keeps the GetInputList-timeout signal clean.

Same coverage; faster test.

EOF
)"
```

---

## Task 5: Refactor SetTextSource through dispatcher with status logging

**Files:**
- Modify: `crates/sp-server/src/obs/mod.rs` — the `ObsCommand::SetTextSource` arm in the main loop

SetInputSettings replies with an op=7 success/failure status that the current code throws away. After Task 3 it falls into the dispatcher's "unmatched op=7 → warn" branch, which is the wrong signal (every successful text update would warn). This task registers a waiter, awaits the status, and logs at appropriate level.

### Step 5.1: Modify the SetTextSource arm

- [ ] **Step 5.1 — Modify the `ObsCommand::SetTextSource` arm in `crates/sp-server/src/obs/mod.rs`**

Find the arm (added in Task 3, Step 3.3):

```rust
                    ObsCommand::SetTextSource { source_name, text } => {
                        // Fire-and-forget: SetInputSettings replies
                        // with an op=7 success/failure status. We
                        // don't currently surface it. Task 5
                        // tightens this to wait + log status.
                        let req_id = uuid::Uuid::new_v4().to_string();
                        let req = text::set_text_request(&req_id, &source_name, &text);
                        write.send(Message::Text(req.to_string().into())).await?;
                        info!(source_name, "sent SetTextSource to OBS");
                    }
```

Replace with:

```rust
                    ObsCommand::SetTextSource { source_name, text } => {
                        let req_id = uuid::Uuid::new_v4().to_string();
                        let req = text::set_text_request(&req_id, &source_name, &text);
                        let rx = dispatcher.register(req_id.clone()).await;
                        write.send(Message::Text(req.to_string().into())).await?;

                        match tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, rx).await {
                            Ok(Ok(response)) => {
                                let ok = response["d"]["requestStatus"]["result"]
                                    .as_bool()
                                    .unwrap_or(false);
                                if ok {
                                    info!(source_name, "SetTextSource ok");
                                } else {
                                    let code = response["d"]["requestStatus"]["code"]
                                        .as_u64()
                                        .unwrap_or(0);
                                    let comment = response["d"]["requestStatus"]["comment"]
                                        .as_str()
                                        .unwrap_or("");
                                    warn!(
                                        source_name,
                                        code,
                                        comment,
                                        "SetTextSource: OBS reported failure"
                                    );
                                }
                            }
                            Ok(Err(_)) => {
                                warn!(
                                    source_name,
                                    "SetTextSource: dispatcher closed before reply"
                                );
                            }
                            Err(_) => {
                                warn!(source_name, "SetTextSource: timed out");
                            }
                        }
                    }
```

### Step 5.2: Verify formatting

- [ ] **Step 5.2 — Run `cargo fmt --all --check`**

Run: `cargo fmt --all --check`
Expected: no diff.

### Step 5.3: Commit

- [ ] **Step 5.3 — Commit Task 5**

```bash
git add crates/sp-server/src/obs/mod.rs
git commit -m "$(cat <<'EOF'
refactor(obs): wait on SetTextSource status via dispatcher

SetInputSettings returns an op=7 with requestStatus.result and
requestStatus.code. Pre-#43 the response landed in the hard-coded
GetCurrentProgramScene-only op=7 branch and was silently dropped.
Post-#43 it would land in the dispatcher's unmatched-warn path,
making every successful text update emit a noisy "no waiter"
warning.

Register a waiter, await the status, log info on success or warn
with code+comment on failure or timeout. Same wire behavior, real
visibility.

EOF
)"
```

---

## Task 6: Update text source name handling — file-cap audit + completion-report inputs

**Files:**
- (no source changes by default; this task is a final audit + plan-check pass)

This task records the file-size budget after the refactor and produces the inputs the controller needs for the completion-report `✅ Regression test:` line.

### Step 6.1: Audit file sizes

- [ ] **Step 6.1 — Verify every touched file stays under the 1000-line cap**

Run: `wc -l crates/sp-server/src/obs/*.rs crates/sp-server/tests/scene_detection.rs`

Expected outcome (rough — actual numbers verified at commit time):

| File | Before | After | Cap |
|---|---|---|---|
| `obs/mod.rs` | 691 | ≈ 720-780 | 1000 |
| `obs/ndi_discovery.rs` | 341 | ≈ 280-310 (shorter, wait_for_response removed) | 1000 |
| `obs/scene.rs` | 234 | ≈ 200-230 (shorter, wait_for_response removed) | 1000 |
| `obs/dispatcher.rs` | (new) | ≈ 170-220 | 1000 |
| `tests/scene_detection.rs` | 385 | ≈ 480-510 (new RED test added) | 1000 |

If any file breaches 1000, split into a sibling submodule BEFORE creating the PR. Most likely candidate is `obs/mod.rs` if the refactor inflates it further than estimated — in that case `run_reader_task` moves into `obs/dispatcher.rs` as `Dispatcher::run_reader_task(self, read, reader_tx)` and the cap is restored.

### Step 6.2: Record regression-test evidence

- [ ] **Step 6.2 — Capture the line + SHAs for the completion report**

After all six commits land locally:

```bash
RED_SHA=$(git log --grep='test(obs): RED' --pretty=%h -n 1)
GREEN_SHA=$(git log --grep='fix(obs): route op=7' --pretty=%h -n 1)
TEST_LINE=$(grep -n 'fn event_during_pending_request_must_be_delivered_fast' \
            crates/sp-server/tests/scene_detection.rs | cut -d: -f1)
echo "Regression test: crates/sp-server/tests/scene_detection.rs:${TEST_LINE} — RED on ${RED_SHA}, GREEN on ${GREEN_SHA}"
```

The controller pastes the resulting line into the completion-report `✅ Regression test:` field.

### Step 6.3: (no commit — this is an audit task)

Task 6 does not produce a commit unless Step 6.1 reveals a cap breach. In the cap-breach case, do the split, run `cargo fmt --all --check`, and commit:

```bash
git commit -m "$(cat <<'EOF'
refactor(obs): move run_reader_task into dispatcher to stay under file cap
EOF
)"
```

---

## Verification

After all tasks ship locally:

1. `cargo fmt --all --check` exits 0.
2. `git log --oneline` shows commits in this order (newest first):
   - `refactor(obs): wait on SetTextSource status via dispatcher`
   - `refactor(obs): drop sleep(2500) workaround from rebuild-failure test`
   - `fix(obs): route op=7 out-of-band via ResponseDispatcher (#43)`
   - `test(obs): RED — events must not be eaten by in-flight requests (#43)`
   - `feat(obs): add ResponseDispatcher for op=7 routing (#43)`
   - (optional cap-split commit if Task 6 produced one)
3. The RED commit precedes the GREEN commit, satisfying `regression-test-first.md`.
4. `wait_for_response` does not appear in any source file — `git grep wait_for_response crates/sp-server/src/` returns nothing.
5. `Dispatcher` is used in `mod.rs`, `ndi_discovery.rs`, and `scene.rs`.
6. The `tokio::time::sleep(Duration::from_millis(2500))` line is gone from `tests/scene_detection.rs`.

Controller pushes the dev branch once after the local six-commit chain is complete, monitors CI to all-green, opens a PR (dev → main), runs `/plan-check` + `/review` + `superpowers:requesting-code-review`, applies fixes, runs the deploy + Playwright post-deploy verification, and sends the completion report.

---

Plan committed locally — dispatching subagents now.
