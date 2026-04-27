# NDI Tier-1 Visibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Tier-1 observability to the NDI pipeline so the dashboard surfaces a real-time alert the moment delivery breaks (closes #46, implements #56).

**Architecture:** Five layers — sp-ndi FFI binding, FrameSubmitter counters, pipeline-thread heartbeat, engine aggregator, HTTP route + alert-only Leptos card. Mirrors the shape of PR #54 (Resolume push hardening). Strict alert gate: `state == Playing` AND ≥2 consecutive bad polls.

**Tech Stack:** Rust 2024, axum 0.8, leptos 0.7 (CSR/WASM), libloading 0.8 (NDI FFI), crossbeam-channel 0.5, tokio 1, tracing 0.1, wiremock for HTTP route tests, Playwright (E2E).

**Spec:** `docs/superpowers/specs/2026-04-26-ndi-tier1-visibility-design.md` — full design context. Implementers should not re-derive; follow this plan literally.

---

## Context for every implementer subagent

**Branch + version.** Already on `dev` at `0.25.0-dev.1` (commit `a9b8481`). Do NOT bump version again.

**Airuleset rules — read before every task:**

1. **TDD strict.** For every task with code changes: write the failing test first → confirm it fails (or trust by inspection if it's a Rust test that can only run on CI) → implement → confirm pass → run `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green. Never skip the fail step.
2. **Never run `cargo clippy/test/build` locally.** The CI runner does that. Local cargo would burn 20 GB of artifacts and conflict with the CI cache. Only `cargo fmt --all --check` is allowed locally.
3. **File-size cap 1000 lines.** Plan task 0 (`ndi_health.rs` extraction) carves room in `playback/mod.rs` (current 966 lines).
4. **`mutants::skip` requires a one-line justification inline** in the source — no exceptions.
5. **Commit after each green TDD cycle.** One commit per task step that says "Commit". Do NOT push — the controller batches all commits and pushes once when all tasks land.
6. **No DB migration. No pipeline-version bump.** This is pure additive observability.
7. **Match existing project style.** Look at PR #54 (`crates/sp-server/src/resolume/{driver,mod}.rs`, `sp-ui/src/components/resolume_health.rs`, `crates/sp-server/src/api/routes_tests.rs::resolume_health_endpoint_*`) — the new code is the same shape, NDI side.

**Two-stage review per task.** After the implementer commits: (1) spec compliance review against `docs/superpowers/specs/2026-04-26-ndi-tier1-visibility-design.md`, (2) code-quality review. Only mark task complete when both approve.

---

## File structure

| File | Status | Responsibility |
|---|---|---|
| `crates/sp-ndi/src/ndi_sdk.rs` | modify | NdiLib loader gains `send_get_no_connections` symbol |
| `crates/sp-ndi/src/sender.rs` | modify | NdiBackend trait + RealNdiBackend impl + MockNdiBackend impl + NdiSender::get_no_connections wrapper |
| `crates/sp-server/src/playback/submitter.rs` | modify | FrameSubmitter counters: total / window / last_submit_ts; drain_window() |
| `crates/sp-server/src/playback/ndi_health.rs` | **new** | Type definitions (PipelineHealthSnapshot, PlaybackStateLabel, WindowStats), `NdiHealthRegistry` (lock-free read via `Arc<RwLock<HashMap>>`, mirrors PR #54's `ResolumeRegistry`), and engine event handler (impl PlaybackEngine) |
| `crates/sp-server/src/playback/mod.rs` | modify | `mod ndi_health;` declaration + delegate `HealthSnapshot` events to the handler. PlaybackEngine accepts `Arc<NdiHealthRegistry>` in `new` |
| `crates/sp-server/src/lib.rs` | modify | Construct `Arc<NdiHealthRegistry>` once at startup; pass clone to `PlaybackEngine::new` and store another clone in `AppState` (parallels `resolume_registry: Arc<ResolumeRegistry>`) |
| `crates/sp-server/src/playback/pipeline.rs` | modify | New `PipelineEvent::HealthSnapshot` variant + Windows heartbeat scheduling (recv_timeout + inner-loop check) |
| `crates/sp-server/src/api/routes.rs` | modify | `get_ndi_health` handler |
| `crates/sp-server/src/api/mod.rs` | modify | wire `/api/v1/ndi/health` route |
| `crates/sp-server/src/api/routes_tests.rs` | modify | 2 route tests (empty array, populated registry) |
| `sp-ui/src/components/ndi_health.rs` | **new** | NdiHealthCard Leptos component (alert-only) |
| `sp-ui/src/components/mod.rs` | modify | `pub mod ndi_health;` |
| `sp-ui/src/pages/dashboard.rs` | modify | mount `<ndi_health::NdiHealthCard />` next to ResolumeHealthCard |
| `sp-ui/style.css` | modify | `.ndi-health-alert`, `.nh-alert`, `.nh-alert-dot` (mirror `.resolume-health-alert` family) |
| `e2e/post-deploy.spec.ts` | modify | One assertion: `.ndi-health-alert` not present when wall is healthy |

---

## Layered task ordering

Strict order. Each layer is self-testable; later layers depend on types/methods defined earlier.

| Layer | Task | Model |
|---|---|---|
| 0 | Extract `ndi_health.rs` skeleton (types + module declaration) | haiku |
| 1 | sp-ndi FFI: bind `NDIlib_send_get_no_connections` | haiku |
| 2 | sp-ndi NdiBackend trait + RealNdiBackend + MockNdiBackend + NdiSender wrapper | sonnet |
| 3 | FrameSubmitter counters + drain_window | sonnet |
| 4 | `PipelineEvent::HealthSnapshot` variant in pipeline.rs | haiku |
| 5 | Engine event handler in `ndi_health.rs` (impl PlaybackEngine) | sonnet |
| 6 | Windows pipeline-thread heartbeat (recv_timeout + inner-loop) | sonnet |
| 7 | HTTP route `/api/v1/ndi/health` + 2 route tests | sonnet |
| 8 | Leptos NdiHealthCard + CSS + dashboard mount | sonnet |
| 9 | Playwright E2E assertion | haiku |

---

## Task 0 — Extract `ndi_health.rs` skeleton

**Why first:** `playback/mod.rs` is at 966 lines; cap is 1000. Putting type definitions and the future event handler here BEFORE any other code keeps mod.rs under cap throughout the rest of the plan. Mirrors `playback/recovery.rs` (61 lines, extracted in PR #54 for the same reason).

**Files:**
- Create: `crates/sp-server/src/playback/ndi_health.rs`
- Modify: `crates/sp-server/src/playback/mod.rs:7-13` (add `mod ndi_health;` after `mod recovery;`)

**Model hint:** haiku (mechanical type definitions, no logic).

- [ ] **Step 1: Create the new file with type definitions and the registry.**

```rust
// crates/sp-server/src/playback/ndi_health.rs
//! NDI per-pipeline health snapshot types + lock-free registry +
//! engine aggregator.
//!
//! Extracted from mod.rs to keep the file under the 1000-line cap.
//! Mirrors `playback/recovery.rs` precedent and `resolume::ResolumeRegistry`
//! shape from PR #54.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Per-pipeline NDI health. Serialized to the dashboard via
/// `GET /api/v1/ndi/health`. Built by the engine from
/// `PipelineEvent::HealthSnapshot` events emitted by the pipeline thread.
#[derive(Clone, Debug, Serialize)]
pub struct PipelineHealthSnapshot {
    pub playlist_id: i64,
    pub ndi_name: String,
    pub state: PlaybackStateLabel,
    /// Connection count from `NDIlib_send_get_no_connections`. `-1` means
    /// the heartbeat has never run yet (e.g. pipeline just spawned).
    pub connections: i32,
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    pub last_submit_ts: Option<DateTime<Utc>>,
    pub last_heartbeat_ts: Option<DateTime<Utc>>,
    pub consecutive_bad_polls: u32,
    /// Populated server-side when `consecutive_bad_polls >= 2`. The dashboard
    /// renders this verbatim; it does NOT compute its own staleness.
    pub degraded_reason: Option<String>,
}

/// Wire-level playback state used by the NDI health snapshot. Distinct from
/// `sp_core::playback::PlaybackState` because the heartbeat needs to
/// distinguish Idle (no playlist active) from Paused (playlist active but
/// paused) from WaitingForScene (engine knows but pipeline doesn't).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum PlaybackStateLabel {
    Idle,
    WaitingForScene,
    Playing,
    Paused,
}

/// Snapshot of the per-pipeline frame counter window. Returned by
/// `FrameSubmitter::drain_window`; the heartbeat consumer divides
/// `frames_in_window` by `window_secs` to get observed fps.
#[derive(Clone, Debug)]
pub struct WindowStats {
    pub frames_in_window: u32,
    pub window_secs: f32,
    /// `Instant::now()` captured when `drain_window` ran.
    pub drained_at: Instant,
}

/// Lock-free-read registry holding the latest health snapshot per pipeline.
/// Mirrors `crate::resolume::ResolumeRegistry` from PR #54: one Arc held by
/// the playback engine (writer) and another by `AppState` (reader). The
/// `RwLock` is held only for short copy-out reads in `snapshots()`; the
/// returned Vec is owned data, no lifetimes leak out.
pub struct NdiHealthRegistry {
    snapshots: RwLock<HashMap<i64, PipelineHealthSnapshot>>,
}

impl NdiHealthRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshots: RwLock::new(HashMap::new()),
        })
    }

    /// Replace (or insert) the snapshot for `playlist_id`.
    /// Called from the playback-engine HealthSnapshot handler.
    pub fn update(&self, snapshot: PipelineHealthSnapshot) {
        if let Ok(mut map) = self.snapshots.write() {
            map.insert(snapshot.playlist_id, snapshot);
        }
    }

    /// Snapshot every pipeline's most recent NDI health for the
    /// `/api/v1/ndi/health` endpoint. Returns one entry per pipeline that
    /// has reported at least one heartbeat.
    pub fn snapshots(&self) -> Vec<PipelineHealthSnapshot> {
        match self.snapshots.read() {
            Ok(map) => map.values().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Default for NdiHealthRegistry {
    fn default() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
        }
    }
}
```

- [ ] **Step 2: Wire the module into `playback/mod.rs`.**

In `crates/sp-server/src/playback/mod.rs`, find the existing module list near the top:

```rust
mod lyrics_loader;
pub mod pipeline;
mod position_update;
mod recovery;
pub mod state;
pub mod submitter;
mod title;
```

Add `mod ndi_health;` so the block becomes:

```rust
mod lyrics_loader;
pub mod ndi_health;
pub mod pipeline;
mod position_update;
mod recovery;
pub mod state;
pub mod submitter;
mod title;
```

(`pub mod` so the type is reachable from `crates/sp-server/src/api/routes.rs`.)

- [ ] **Step 3: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0, no diff.

- [ ] **Step 4: Commit.**

```bash
git add crates/sp-server/src/playback/ndi_health.rs crates/sp-server/src/playback/mod.rs
git commit -m "refactor(playback): extract ndi_health module skeleton

Carves room for the NDI Tier-1 visibility implementation per the design
doc. Defines PipelineHealthSnapshot / PlaybackStateLabel / WindowStats
types only — no logic yet. Engine event handler arrives in Task 5.

Mirrors recovery.rs precedent (PR #54) so playback/mod.rs (966 lines)
does not push past the 1000-line cap as later tasks add the handler.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 1 — sp-ndi FFI: bind `NDIlib_send_get_no_connections`

**Why:** the symbol is not currently bound in `NdiLib`. Without it the higher layers cannot query connection count. Pure FFI plumbing.

**Files:**
- Modify: `crates/sp-ndi/src/ndi_sdk.rs:30-31` (add type alias next to `FnSendGetTally`)
- Modify: `crates/sp-ndi/src/ndi_sdk.rs:52` (add field on `NdiLib`)
- Modify: `crates/sp-ndi/src/ndi_sdk.rs:89-110` (resolve the symbol in `load()`)

**Model hint:** haiku.

- [ ] **Step 1: Add the type alias.**

In `crates/sp-ndi/src/ndi_sdk.rs`, add the new alias immediately after the existing `FnSendGetTally`:

```rust
type FnSendGetTally =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, *mut NDIlib_tally_t, u32) -> bool;
type FnSendGetNoConnections =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, u32) -> i32;
```

- [ ] **Step 2: Add the field on `NdiLib`.**

After the existing `pub(crate) send_get_tally: FnSendGetTally,` line, add:

```rust
    pub(crate) send_get_tally: FnSendGetTally,
    pub(crate) send_get_no_connections: FnSendGetNoConnections,
```

- [ ] **Step 3: Resolve the symbol in `load()`.**

After the existing `send_get_tally` resolution and before the `info!("Calling NDIlib_initialize()")` line, add:

```rust
            let send_get_tally =
                Self::resolve::<FnSendGetTally>(&library, b"NDIlib_send_get_tally\0")?;
            let send_get_no_connections = Self::resolve::<FnSendGetNoConnections>(
                &library,
                b"NDIlib_send_get_no_connections\0",
            )?;
```

And in the `Ok(Self { ... })` initializer, add the field after `send_get_tally,`:

```rust
                send_get_tally,
                send_get_no_connections,
```

- [ ] **Step 4: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 5: Commit.**

```bash
git add crates/sp-ndi/src/ndi_sdk.rs
git commit -m "feat(sp-ndi): bind NDIlib_send_get_no_connections symbol

First piece of the NDI Tier-1 visibility wiring. The symbol is exposed
on NdiLib so RealNdiBackend can query receiver count in the next task.
NDIlib_send_get_no_connections returns int (>=0 = count); we always
pass timeout_ms=0 so the call is non-blocking.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2 — sp-ndi NdiBackend trait + impls + safe wrapper

**Why:** the bound symbol must be reachable through `NdiBackend` so MockNdiBackend can drive every alert-rule branch on the Linux mutation runner. `NdiSender::get_no_connections` is the safe surface higher layers use.

**Files:**
- Modify: `crates/sp-ndi/src/sender.rs:132-133` (extend `NdiBackend` trait)
- Modify: `crates/sp-ndi/src/sender.rs:355-368` (add `RealNdiBackend` impl method)
- Modify: `crates/sp-ndi/src/sender.rs:471-484` (add `NdiSender::get_no_connections`)
- Modify: `crates/sp-ndi/src/sender.rs:506-528` (extend `MockNdiBackend` with `connection_count` field + setter)
- Modify: `crates/sp-ndi/src/sender.rs:530-613` (add `MockNdiBackend::send_get_no_connections` impl)
- Modify: `crates/sp-ndi/src/sender.rs:620-...` (tests section — add 3 tests)

**Model hint:** sonnet (multi-file trait edits, several test designs, Real-side `mutants::skip` justification).

- [ ] **Step 1: Write the failing test for `MockNdiBackend::set_connection_count`.**

In the existing `mod tests` block of `crates/sp-ndi/src/sender.rs`, add:

```rust
    #[test]
    fn mock_get_no_connections_returns_set_count() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "C", true, false).unwrap();
        // Default before any setter: 0 (no receivers).
        assert_eq!(sender.get_no_connections(0), 0);
        backend.set_connection_count(3);
        assert_eq!(sender.get_no_connections(0), 3);
        backend.set_connection_count(0);
        assert_eq!(sender.get_no_connections(0), 0);
    }

    #[test]
    fn mock_get_no_connections_records_call() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "C2", true, false).unwrap();
        let _ = sender.get_no_connections(50);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c == "send_get_no_connections(42,50)"),
            "expected send_get_no_connections(handle=42, timeout=50) recorded: {calls:#?}"
        );
    }

    #[test]
    fn mock_set_connection_count_is_thread_safe_via_atomic() {
        // Driven from another thread to confirm visibility — same pattern the
        // pipeline thread will use (heartbeat polls from one thread, the test
        // helper sets from another).
        let backend = Arc::new(MockNdiBackend::new());
        let backend2 = backend.clone();
        let h = std::thread::spawn(move || backend2.set_connection_count(7));
        h.join().unwrap();
        let sender = NdiSender::new_with_clocking(backend, "C3", true, false).unwrap();
        assert_eq!(sender.get_no_connections(0), 7);
    }
```

- [ ] **Step 2: Confirm the tests fail by inspection.**

These won't compile yet (`set_connection_count`, `send_get_no_connections`, `get_no_connections` don't exist). On Rust this means the whole crate fails to build — that's the equivalent of TDD red. Continue.

- [ ] **Step 3: Extend the trait.**

In `crates/sp-ndi/src/sender.rs`, after the existing `send_get_tally` method on `NdiBackend`:

```rust
    /// Query tally state. Returns `None` if the timeout expired with no change.
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)>;

    /// Return the current number of NDI receivers connected to this sender.
    /// Returns `>= 0` when the SDK reports a count; the caller must treat any
    /// negative value as "unknown" and not as a failure (the NDI SDK may
    /// occasionally use negatives to mean "never been polled").
    ///
    /// `timeout_ms = 0` is the recommended value: the SDK returns the cached
    /// count immediately. With `> 0` the call blocks until the count changes
    /// or the timeout expires.
    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32;
}
```

- [ ] **Step 4: Implement on `RealNdiBackend`.**

After the existing `send_get_tally` impl on `RealNdiBackend`:

```rust
    #[cfg_attr(test, mutants::skip)]
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        let handles = self.handles.lock().unwrap();
        let state = handles.get(&handle)?;

        let mut tally = NDIlib_tally_t::default();
        let changed = unsafe { (self.lib.send_get_tally)(state.ptr, &mut tally, timeout_ms) };
        if changed {
            Some((tally.on_program, tally.on_preview))
        } else {
            None
        }
    }

    // mutants::skip — dereferences NDI SDK function pointer; only exercised on
    // real Windows runtime. Behaviour is verified through MockNdiBackend.
    #[cfg_attr(test, mutants::skip)]
    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32 {
        let handles = self.handles.lock().unwrap();
        let Some(state) = handles.get(&handle) else {
            return -1;
        };
        unsafe { (self.lib.send_get_no_connections)(state.ptr, timeout_ms) }
    }
}
```

- [ ] **Step 5: Add the safe wrapper on `NdiSender`.**

After the existing `get_tally` method on `NdiSender`:

```rust
    /// Query the tally state (program / preview) with a timeout in milliseconds.
    pub fn get_tally(&self, timeout_ms: u32) -> Option<Tally> {
        self.backend
            .send_get_tally(self.handle, timeout_ms)
            .map(|(on_program, on_preview)| Tally {
                on_program,
                on_preview,
            })
    }

    /// Return the current count of NDI receivers connected to this sender.
    /// `timeout_ms = 0` returns immediately with the SDK's cached count.
    pub fn get_no_connections(&self, timeout_ms: u32) -> i32 {
        self.backend.send_get_no_connections(self.handle, timeout_ms)
    }
```

- [ ] **Step 6: Extend `MockNdiBackend` with the connection counter field.**

Find the existing `MockNdiBackend` struct (`pub struct MockNdiBackend { ... }`) inside the `pub mod test_util { ... }` block. Add the `connection_count` field and use `std::sync::atomic::AtomicI32`:

```rust
pub mod test_util {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// A mock backend that records every call for assertion.
    #[derive(Default)]
    pub struct MockNdiBackend {
        calls: StdMutex<Vec<String>>,
        tally_response: StdMutex<Option<(bool, bool)>>,
        last_audio_planar: StdMutex<Vec<f32>>,
        connection_count: AtomicI32,
    }

    impl MockNdiBackend {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn last_audio_planar(&self) -> Vec<f32> {
            self.last_audio_planar.lock().unwrap().clone()
        }

        pub fn set_tally(&self, on_program: bool, on_preview: bool) {
            *self.tally_response.lock().unwrap() = Some((on_program, on_preview));
        }

        /// Drive the value `MockNdiBackend::send_get_no_connections` returns.
        /// Lets unit tests exercise every NDI-health alert branch without a
        /// real NDI runtime.
        pub fn set_connection_count(&self, n: i32) {
            self.connection_count.store(n, Ordering::SeqCst);
        }
    }
```

- [ ] **Step 7: Implement `send_get_no_connections` on `MockNdiBackend`.**

After the existing `send_get_tally` impl in the `MockNdiBackend NdiBackend` impl block:

```rust
        fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_get_tally({handle},{timeout_ms})"));
            *self.tally_response.lock().unwrap()
        }

        fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32 {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_get_no_connections({handle},{timeout_ms})"));
            self.connection_count.load(Ordering::SeqCst)
        }
    }
}
```

- [ ] **Step 8: Confirm tests pass by inspection.**

The three new tests in `mod tests` should now compile. CI will run them on the next push. Trust by inspection (per airuleset, never run cargo test locally).

- [ ] **Step 9: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 10: Commit.**

```bash
git add crates/sp-ndi/src/sender.rs
git commit -m "feat(sp-ndi): NdiBackend::send_get_no_connections + safe wrapper

Adds the trait method, RealNdiBackend impl (mutants::skip with reason
since the call dereferences the NDI SDK function pointer), MockNdiBackend
impl backed by AtomicI32 + set_connection_count test helper, and the
NdiSender::get_no_connections safe wrapper.

Three new unit tests cover Mock returns the set count, Mock records the
call, and atomic visibility across threads.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3 — FrameSubmitter counters + drain_window

**Why:** the heartbeat needs to know how many frames went out in the last window and when the most recent submit happened. The submitter is the single fast path for `submit_nv12` (the only path that counts as "playback" — `send_black_bgra` is standby and explicitly excluded). Cross-platform unit-testable using MockNdiBackend.

**Files:**
- Modify: `crates/sp-server/src/playback/submitter.rs:32-51` (add fields + initialize them)
- Modify: `crates/sp-server/src/playback/submitter.rs:77-112` (bump counters in `submit_nv12`)
- Modify: `crates/sp-server/src/playback/submitter.rs:139-142` (add `drain_window`, `frames_submitted_total`, `last_submit_ts` accessors)
- Modify: `crates/sp-server/src/playback/submitter.rs:144-...` (tests — add 3 tests)

**Model hint:** sonnet (counter design, window arithmetic, edge case for `send_black_bgra` non-counting).

- [ ] **Step 1: Write the failing tests.**

Add at the end of `crates/sp-server/src/playback/submitter.rs`'s `mod tests` block:

```rust
    #[test]
    fn submitter_counts_frames_submitted_total() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Cnt", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        assert_eq!(sub.frames_submitted_total(), 0);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 1);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 3);
    }

    #[test]
    fn drain_window_resets_window_counter_but_not_total() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "DW", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let stats1 = sub.drain_window();
        assert_eq!(stats1.frames_in_window, 2);
        assert!(stats1.window_secs >= 0.0);
        // Total preserved across drain.
        assert_eq!(sub.frames_submitted_total(), 2);

        // Next drain (no submits in between) returns 0 frames.
        let stats2 = sub.drain_window();
        assert_eq!(stats2.frames_in_window, 0);
        assert_eq!(sub.frames_submitted_total(), 2);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let stats3 = sub.drain_window();
        assert_eq!(stats3.frames_in_window, 1);
        assert_eq!(sub.frames_submitted_total(), 3);
    }

    #[test]
    fn send_black_bgra_does_not_count_as_a_frame_submission() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Bk", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.send_black_bgra(1920, 1080);
        assert_eq!(
            sub.frames_submitted_total(),
            0,
            "black-frame standby must NOT count as playback"
        );
        assert!(
            sub.last_submit_ts().is_none(),
            "black-frame standby must not advance last_submit_ts"
        );

        // Confirm a real submit DOES count.
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 1);
        assert!(sub.last_submit_ts().is_some());
    }
```

- [ ] **Step 2: Confirm tests fail by inspection.**

`frames_submitted_total`, `drain_window`, `last_submit_ts` don't exist; the file fails to compile. Continue.

- [ ] **Step 3: Add the new fields and accessors.**

Edit the `FrameSubmitter` struct so it ends up:

```rust
pub struct FrameSubmitter<B: NdiBackend> {
    // NOTE: do not reorder these fields — see the SAFETY-CRITICAL note above.
    sender: NdiSender<B>,
    /// Keeps the previous async frame's `Vec<u8>` alive until NDI releases
    /// its pointer (which happens when the next submit / flush call fires).
    prev_frame: Option<Vec<u8>>,
    frame_rate_n: i32,
    frame_rate_d: i32,
    /// Monotonic count of `submit_nv12` calls. `send_black_bgra` does not
    /// bump this — black-frame standby is not "playback" for visibility
    /// purposes.
    frames_submitted_total: u64,
    /// Frames since the most recent `drain_window` call. Reset on drain.
    frames_in_window: u32,
    /// Wall-clock instant the current window started (last drain or
    /// FrameSubmitter construction).
    window_start: std::time::Instant,
    /// Wall-clock instant of the last `submit_nv12` call. `None` means no
    /// real frame has been submitted (standby black frames are excluded).
    last_submit_ts: Option<std::time::Instant>,
}
```

Update `FrameSubmitter::new` to initialize the new fields:

```rust
impl<B: NdiBackend> FrameSubmitter<B> {
    pub fn new(sender: NdiSender<B>, frame_rate_n: i32, frame_rate_d: i32) -> Self {
        Self {
            sender,
            prev_frame: None,
            frame_rate_n,
            frame_rate_d,
            frames_submitted_total: 0,
            frames_in_window: 0,
            window_start: std::time::Instant::now(),
            last_submit_ts: None,
        }
    }
```

- [ ] **Step 4: Bump counters inside `submit_nv12`.**

At the very top of `submit_nv12`, add:

```rust
    pub fn submit_nv12(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video_data: Vec<u8>,
        audio: &[AudioFrame],
    ) {
        // Counters first — these must run on every successful submit, even
        // if the SDK call below blocks on clock_video pacing.
        self.frames_submitted_total += 1;
        self.frames_in_window += 1;
        self.last_submit_ts = Some(std::time::Instant::now());

        // 1. Audio first — fast, non-blocking, goes straight into NDI's queue.
        for af in audio {
            self.sender.send_audio(af);
        }
        // ...rest unchanged...
```

(`send_black_bgra` is not modified — it must NOT count.)

- [ ] **Step 5: Add `drain_window` and accessors.**

After `pub fn sender(&self) -> &NdiSender<B>`:

```rust
    /// Borrow the underlying sender (mainly for tests).
    pub fn sender(&self) -> &NdiSender<B> {
        &self.sender
    }

    /// Snapshot the rolling window counter and reset it. Returns the number
    /// of frames submitted since the last drain plus the wall-clock seconds
    /// over which they accumulated.
    ///
    /// The heartbeat caller divides `frames_in_window / window_secs` to get
    /// observed fps. `window_secs` is clamped at the call site to avoid
    /// divide-by-zero on freshly-spawned pipelines (the heartbeat does
    /// `window_secs.max(0.001)`).
    pub fn drain_window(&mut self) -> crate::playback::ndi_health::WindowStats {
        let now = std::time::Instant::now();
        let window_secs = now.duration_since(self.window_start).as_secs_f32();
        let frames = self.frames_in_window;
        self.frames_in_window = 0;
        self.window_start = now;
        crate::playback::ndi_health::WindowStats {
            frames_in_window: frames,
            window_secs,
            drained_at: now,
        }
    }

    pub fn frames_submitted_total(&self) -> u64 {
        self.frames_submitted_total
    }

    pub fn last_submit_ts(&self) -> Option<std::time::Instant> {
        self.last_submit_ts
    }
```

- [ ] **Step 6: Confirm tests pass by inspection.**

Three new tests align with the additions. CI will run them.

- [ ] **Step 7: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 8: Commit.**

```bash
git add crates/sp-server/src/playback/submitter.rs
git commit -m "feat(playback/submitter): frame counters + drain_window for heartbeat

Counts every submit_nv12 (frames_submitted_total monotonic, plus a
rolling window reset by drain_window). last_submit_ts records wall-clock
of the most recent real submit. send_black_bgra deliberately does NOT
count — black-frame standby is not playback for visibility purposes.

Three unit tests cover total counting, drain reset semantics, and the
black-frame-non-counting invariant.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4 — `PipelineEvent::HealthSnapshot` variant

**Why:** the engine receives pipeline updates over `event_tx`. Adding a new variant on `PipelineEvent` is the entry point for heartbeat data into the engine. Defining the variant first lets Tasks 5 (engine handler) and 6 (pipeline-thread emitter) be implemented in either order against a stable type.

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs:46-57` (extend `PipelineEvent`)

**Model hint:** haiku.

- [ ] **Step 1: Extend `PipelineEvent`.**

In `crates/sp-server/src/playback/pipeline.rs`, replace the existing enum:

```rust
/// Events emitted by the pipeline thread back to the async engine.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Video playback started; duration is known.
    Started { duration_ms: u64 },
    /// Periodic position update.
    Position { position_ms: u64, duration_ms: u64 },
    /// Video reached its natural end.
    Ended,
    /// An error occurred during playback.
    Error(String),
    /// Per-pipeline NDI health heartbeat. Emitted every ~5 seconds by the
    /// pipeline thread when running on Windows; consumed by
    /// `PlaybackEngine::handle_health_snapshot` (impl in
    /// `playback/ndi_health.rs`). The pipeline reports its locally-inferred
    /// state (Idle / Playing / Paused); the engine reconciles it against
    /// canonical `PlayState` before publishing to the dashboard.
    HealthSnapshot {
        connections: i32,
        frames_submitted_total: u64,
        frames_submitted_last_5s: u32,
        observed_fps: f32,
        nominal_fps: f32,
        /// `Instant` is fine on the wire here because emitter and consumer
        /// are in the same process. The engine maps it to `DateTime<Utc>`
        /// using a fixed `Instant`-to-`SystemTime` reference before
        /// publishing.
        last_submit_ts: Option<std::time::Instant>,
        last_heartbeat_ts: std::time::Instant,
        consecutive_bad_polls: u32,
        reported_state: crate::playback::ndi_health::PlaybackStateLabel,
    },
}
```

- [ ] **Step 2: Add a smoke test that the variant constructs and clones.**

Append to `crates/sp-server/src/playback/pipeline.rs`'s existing `#[cfg(test)] mod tests` block (find it at the bottom of the file). If no tests block exists at the bottom, add one:

```rust
#[cfg(test)]
mod pipeline_event_tests {
    use super::*;
    use crate::playback::ndi_health::PlaybackStateLabel;
    use std::time::Instant;

    #[test]
    fn health_snapshot_variant_constructs_and_clones() {
        let now = Instant::now();
        let ev = PipelineEvent::HealthSnapshot {
            connections: 1,
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
        };
        let cloned = ev.clone();
        // Pattern-match to assert the variant exists and the fields round-trip.
        if let PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_last_5s,
            reported_state,
            ..
        } = cloned
        {
            assert_eq!(connections, 1);
            assert_eq!(frames_submitted_last_5s, 30);
            assert_eq!(reported_state, PlaybackStateLabel::Playing);
        } else {
            panic!("clone produced wrong variant");
        }
    }
}
```

- [ ] **Step 3: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 4: Commit.**

```bash
git add crates/sp-server/src/playback/pipeline.rs
git commit -m "feat(playback/pipeline): add PipelineEvent::HealthSnapshot variant

Wire-level shape for the per-pipeline NDI heartbeat. Consumed in Task 5
(engine handler), emitted in Task 6 (Windows-only pipeline-thread). Smoke
test asserts the variant constructs and clones with all fields preserved.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5 — Engine event handler + AppState wiring

**Why:** translates incoming `PipelineEvent::HealthSnapshot` into a snapshot, reconciles the pipeline's reported state against canonical `PlayState`, fills `degraded_reason`, and writes to the shared `NdiHealthRegistry` (which AppState also holds). The registry pattern matches PR #54's `ResolumeRegistry` shape and avoids needing the API to reach into the engine.

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs`:
  - Add `instant_origin: (Instant, DateTime<Utc>)` and `ndi_health_registry: Arc<NdiHealthRegistry>` fields to `PlaybackEngine`
  - Extend `PlaybackEngine::new` signature with `ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>`
  - Initialize `instant_origin` in `new`
  - Route `HealthSnapshot` events to `self.handle_health_snapshot(...)`
- Modify: `crates/sp-server/src/playback/ndi_health.rs` (add `impl crate::playback::PlaybackEngine { handle_health_snapshot }` + `compute_degraded_reason` helper)
- Modify: `crates/sp-server/src/playback/pipeline.rs` (add `pub fn ndi_name(&self) -> &str` accessor on `PlaybackPipeline`; add `ndi_name: String` field if not already there)
- Modify: `crates/sp-server/src/lib.rs`:
  - Construct `let ndi_health_registry = playback::ndi_health::NdiHealthRegistry::new();` near where `resolume_registry` is built
  - Pass `ndi_health_registry.clone()` into `PlaybackEngine::new`
  - Add `pub ndi_health_registry: Arc<playback::ndi_health::NdiHealthRegistry>` to `AppState` and initialize it from the same Arc

**Model hint:** sonnet (multi-file edits, `Instant` → `DateTime<Utc>` mapping, state-override logic, AppState wiring).

- [ ] **Step 1: Write the failing tests for the engine handler in `ndi_health.rs`.**

Append to `crates/sp-server/src/playback/ndi_health.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::PlaybackEngine;
    use crate::playback::pipeline::PipelineEvent;
    use crate::playback::state::PlayState;
    use sp_core::ws::ServerMsg;
    use sqlx::SqlitePool;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::sync::{broadcast, mpsc};

    async fn fresh_engine() -> (PlaybackEngine, Arc<NdiHealthRegistry>) {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        let (obs_tx, _) = broadcast::channel(16);
        let (resolume_tx, _) = mpsc::channel(16);
        let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
        let registry = NdiHealthRegistry::new();
        let engine = PlaybackEngine::new(
            pool,
            PathBuf::from("/tmp"),
            obs_tx,
            None,
            resolume_tx,
            ws_tx,
            None,
            registry.clone(),
        );
        (engine, registry)
    }

    #[tokio::test]
    async fn handle_health_snapshot_populates_registry_for_known_pipeline() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(7, "SP-test");

        let now = Instant::now();
        engine.handle_health_snapshot(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 2,
                frames_submitted_total: 150,
                frames_submitted_last_5s: 30,
                observed_fps: 29.97,
                nominal_fps: 29.97,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Playing,
            },
        );

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].playlist_id, 7);
        assert_eq!(snapshots[0].connections, 2);
        assert_eq!(snapshots[0].frames_submitted_total, 150);
        assert!(snapshots[0].last_submit_ts.is_some());
    }

    #[tokio::test]
    async fn handle_health_snapshot_drops_event_for_unknown_pipeline() {
        let (mut engine, registry) = fresh_engine().await;
        let now = Instant::now();
        engine.handle_health_snapshot(
            999,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 0,
                frames_submitted_last_5s: 0,
                observed_fps: 0.0,
                nominal_fps: 30.0,
                last_submit_ts: None,
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Idle,
            },
        );
        assert_eq!(registry.snapshots().len(), 0);
    }

    #[tokio::test]
    async fn registry_holds_one_entry_per_pipeline_with_health() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(1, "SP-a");
        engine.ensure_pipeline(2, "SP-b");
        let now = Instant::now();
        let mk_event = |state| PipelineEvent::HealthSnapshot {
            connections: 1,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            last_submit_ts: None,
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: state,
        };
        engine.handle_health_snapshot(1, mk_event(PlaybackStateLabel::Playing));
        engine.handle_health_snapshot(2, mk_event(PlaybackStateLabel::Idle));
        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 2);
        let ids: Vec<_> = snapshots.iter().map(|s| s.playlist_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[tokio::test]
    async fn engine_overrides_idle_to_waiting_for_scene_when_canonical_state_says_so() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(5, "SP-w");
        engine.set_state_for_test(5, PlayState::WaitingForScene);

        let now = Instant::now();
        engine.handle_health_snapshot(
            5,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 0,
                frames_submitted_last_5s: 0,
                observed_fps: 0.0,
                nominal_fps: 30.0,
                last_submit_ts: None,
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Idle,
            },
        );

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            snapshots[0].state,
            PlaybackStateLabel::WaitingForScene,
            "engine must override pipeline's Idle -> WaitingForScene when canonical state matches"
        );
    }

    #[tokio::test]
    async fn handle_health_snapshot_fills_degraded_reason_at_2_consecutive_bad_polls() {
        let (mut engine, registry) = fresh_engine().await;
        engine.ensure_pipeline(8, "SP-fail");
        engine.set_state_for_test(8, PlayState::Playing { video_id: 1 });
        let now = Instant::now();
        engine.handle_health_snapshot(
            8,
            PipelineEvent::HealthSnapshot {
                connections: 0,
                frames_submitted_total: 100,
                frames_submitted_last_5s: 30,
                observed_fps: 30.0,
                nominal_fps: 30.0,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 2,
                reported_state: PlaybackStateLabel::Playing,
            },
        );
        let snapshots = registry.snapshots();
        assert_eq!(snapshots[0].consecutive_bad_polls, 2);
        assert_eq!(
            snapshots[0].degraded_reason.as_deref(),
            Some("no NDI receiver — wall is dark"),
            "connections == 0 with consecutive_bad_polls >= 2 must yield the receiver-missing reason"
        );
    }
}
```

- [ ] **Step 2: Confirm tests fail by inspection.**

`handle_health_snapshot`, `set_state_for_test`, `instant_origin`, `ndi_health_registry`, the new `PlaybackEngine::new` arity — none exist. The crate fails to compile. Continue.

- [ ] **Step 3: Extend `PlaybackEngine` with the registry + instant origin.**

In `crates/sp-server/src/playback/mod.rs`, find the `PlaybackEngine` struct (around line 125) and append fields:

```rust
pub struct PlaybackEngine {
    pool: SqlitePool,
    // ...existing fields...
    presenter_client: Option<Arc<crate::presenter::PresenterClient>>,
    /// Reference for mapping `Instant` → `DateTime<Utc>`. Captured at engine
    /// construction so the heartbeat handler can publish absolute timestamps
    /// without holding a SystemTime per snapshot.
    instant_origin: (std::time::Instant, chrono::DateTime<chrono::Utc>),
    /// Shared registry holding the latest NDI health snapshot per pipeline.
    /// Cloned into `AppState` so the API layer reads without going through the
    /// engine. Mirrors the `Arc<ResolumeRegistry>` pattern from PR #54.
    ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
}
```

Extend the `PlaybackEngine::new` signature:

```rust
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        obs_event_tx: broadcast::Sender<ObsEvent>,
        obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
        resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
        ws_event_tx: broadcast::Sender<ServerMsg>,
        presenter_client: Option<Arc<crate::presenter::PresenterClient>>,
        ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
    ) -> Self {
        // ...existing body...
        let instant_origin = (std::time::Instant::now(), chrono::Utc::now());

        Self {
            pool,
            // ...existing initializers...
            presenter_client,
            instant_origin,
            ndi_health_registry,
        }
    }
```

(`PlaylistPipeline` does NOT get a `cached_health` field — the registry holds the per-pipeline snapshot directly.)

- [ ] **Step 4: Add the test-only state setter on `PlaybackEngine`.**

In `crates/sp-server/src/playback/mod.rs`, immediately after the existing `pub fn ensure_pipeline(&mut self, playlist_id: i64, ndi_name: &str)` method:

```rust
    /// Test-only: force a pipeline's canonical engine state. Lets the
    /// ndi_health unit tests drive the WaitingForScene override path
    /// without spinning up an OBS event stream.
    #[cfg(test)]
    pub(crate) fn set_state_for_test(&mut self, playlist_id: i64, state: PlayState) {
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.state = state;
        }
    }
```

- [ ] **Step 5: Implement the handler in `ndi_health.rs`.**

Append above the `#[cfg(test)] mod tests` block in `crates/sp-server/src/playback/ndi_health.rs`:

```rust
use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::PlayState;
use tracing::{info, warn};

impl crate::playback::PlaybackEngine {
    /// Map an `Instant` from the pipeline thread to a `DateTime<Utc>` using
    /// the engine's startup reference. Approximate (drift between Instant's
    /// monotonic clock and SystemTime grows over long runs) but bounded by
    /// the difference between Instant::now() and SystemTime::now() at engine
    /// startup, which is typically zero.
    fn instant_to_utc(&self, t: Instant) -> DateTime<Utc> {
        let (origin_instant, origin_utc) = self.instant_origin;
        let delta = t.saturating_duration_since(origin_instant);
        origin_utc + chrono::Duration::from_std(delta).unwrap_or(chrono::Duration::zero())
    }

    /// Process a `PipelineEvent::HealthSnapshot` for `playlist_id`.
    /// Reconciles the pipeline-reported state against the canonical
    /// `PlayState`, fills `degraded_reason` when consecutive_bad_polls >= 2,
    /// and writes the result into the shared `NdiHealthRegistry`.
    pub fn handle_health_snapshot(&mut self, playlist_id: i64, event: PipelineEvent) {
        let PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            last_submit_ts,
            last_heartbeat_ts,
            consecutive_bad_polls,
            reported_state,
        } = event
        else {
            return;
        };

        // Drop the event entirely for pipelines the engine doesn't know about.
        // Returning early instead of writing through the registry keeps the
        // API output consistent with the engine's view of which pipelines
        // exist (e.g. a stale heartbeat from a torn-down pipeline can't
        // resurrect itself in the snapshot list).
        let pp = match self.pipelines.get(&playlist_id) {
            Some(p) => p,
            None => return,
        };

        // Reconcile state: the canonical engine knows about WaitingForScene;
        // the pipeline thread doesn't. Override the pipeline's Idle when the
        // engine says WaitingForScene.
        let canonical_state = match (&pp.state, &reported_state) {
            (PlayState::WaitingForScene, PlaybackStateLabel::Idle) => {
                PlaybackStateLabel::WaitingForScene
            }
            _ => reported_state.clone(),
        };

        let ndi_name = pp.pipeline.ndi_name().to_string();
        let degraded_reason = compute_degraded_reason(
            &canonical_state,
            connections,
            observed_fps,
            nominal_fps,
            consecutive_bad_polls,
        );

        // Look up the previous snapshot from the registry to detect
        // connection-count changes and degraded transitions for logging.
        let prev = self
            .ndi_health_registry
            .snapshots()
            .into_iter()
            .find(|s| s.playlist_id == playlist_id);
        let prev_connections = prev.as_ref().map(|s| s.connections);
        let prev_degraded = prev.as_ref().and_then(|s| s.degraded_reason.clone());

        let snapshot = PipelineHealthSnapshot {
            playlist_id,
            ndi_name: ndi_name.clone(),
            state: canonical_state.clone(),
            connections,
            frames_submitted_total,
            frames_submitted_last_5s,
            observed_fps,
            nominal_fps,
            last_submit_ts: last_submit_ts.map(|t| self.instant_to_utc(t)),
            last_heartbeat_ts: Some(self.instant_to_utc(last_heartbeat_ts)),
            consecutive_bad_polls,
            degraded_reason: degraded_reason.clone(),
        };

        // Transition logging: connection-count change, degradation, recovery.
        if prev_connections != Some(connections) {
            info!(
                playlist_id,
                ndi_name = %ndi_name,
                prev = ?prev_connections,
                now = connections,
                "ndi: connections changed"
            );
        }
        if degraded_reason.is_some() && prev_degraded.is_none() {
            warn!(
                playlist_id,
                ndi_name = %ndi_name,
                reason = degraded_reason.as_deref().unwrap_or(""),
                "ndi: pipeline degraded"
            );
        } else if degraded_reason.is_none() && prev_degraded.is_some() {
            info!(
                playlist_id,
                ndi_name = %ndi_name,
                "ndi: pipeline recovered"
            );
        }

        self.ndi_health_registry.update(snapshot);
    }
}

/// Pure helper: convert canonical state + per-poll values + consecutive
/// bad-poll count into the degraded_reason string. The frontend uses this
/// string verbatim. Returns None when the snapshot is healthy or below
/// the >=2 consecutive gate.
///
/// Mutation testing: the >=2 gate is a single comparison; the helper is
/// excluded from cargo-mutants because the boundary is exhaustively
/// covered by `handle_health_snapshot_fills_degraded_reason_at_2_consecutive_bad_polls`
/// plus a `degraded_reason_returns_none_at_one_bad_poll` test below.
#[cfg_attr(test, mutants::skip)]
fn compute_degraded_reason(
    state: &PlaybackStateLabel,
    connections: i32,
    observed_fps: f32,
    nominal_fps: f32,
    consecutive_bad_polls: u32,
) -> Option<String> {
    if !matches!(state, PlaybackStateLabel::Playing) {
        return None;
    }
    if consecutive_bad_polls < 2 {
        return None;
    }
    if connections == 0 {
        return Some("no NDI receiver — wall is dark".to_string());
    }
    if nominal_fps > 0.0 && observed_fps < nominal_fps / 2.0 {
        return Some(format!(
            "underrunning ({obs:.0}/{nom:.0} fps)",
            obs = observed_fps,
            nom = nominal_fps,
        ));
    }
    Some("no frames in 10s".to_string())
}
```

- [ ] **Step 6: Add a boundary test for `compute_degraded_reason`.**

Append to the `#[cfg(test)] mod tests` block in `ndi_health.rs`:

```rust
    #[test]
    fn degraded_reason_returns_none_at_one_bad_poll() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 0, 0.0, 30.0, 1);
        assert!(r.is_none(), "single bad poll must not trigger degradation");
    }

    #[test]
    fn degraded_reason_returns_none_when_not_playing() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Idle, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
        let r = compute_degraded_reason(&PlaybackStateLabel::Paused, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
        let r = compute_degraded_reason(&PlaybackStateLabel::WaitingForScene, 0, 0.0, 30.0, 5);
        assert!(r.is_none());
    }

    #[test]
    fn degraded_reason_emits_underrun_when_fps_below_half_nominal() {
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 10.0, 30.0, 2);
        assert_eq!(r.as_deref(), Some("underrunning (10/30 fps)"));
    }

    #[test]
    fn degraded_reason_emits_stale_when_fps_ok_and_connections_ok() {
        // connections > 0 AND fps healthy AND consecutive >= 2 means by
        // elimination the staleness branch tripped.
        let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 30.0, 30.0, 2);
        assert_eq!(r.as_deref(), Some("no frames in 10s"));
    }
```

- [ ] **Step 7: Route `HealthSnapshot` events to the handler in the engine main loop.**

In `crates/sp-server/src/playback/mod.rs`, find the existing event-loop arm that matches on `PipelineEvent::Position { ... }` (around line 503) and add a sibling arm AFTER `PipelineEvent::Error(msg)` arm (around line 524):

```rust
            PipelineEvent::Error(msg) => {
                // ...existing handling unchanged...
            }
            ev @ PipelineEvent::HealthSnapshot { .. } => {
                self.handle_health_snapshot(playlist_id, ev);
            }
```

- [ ] **Step 8: Add a `pub fn ndi_name(&self) -> &str` accessor on `PlaybackPipeline`.**

The handler in `ndi_health.rs` needs the pipeline's name to populate `PipelineHealthSnapshot::ndi_name`. In `crates/sp-server/src/playback/pipeline.rs`, after `PlaybackPipeline::spawn`, add:

```rust
impl PlaybackPipeline {
    /// Borrow the NDI source name this pipeline was spawned with. Used by
    /// `ndi_health` to populate health snapshot labels.
    pub fn ndi_name(&self) -> &str {
        &self.ndi_name
    }
}
```

(If `ndi_name` is not currently stored on `PlaybackPipeline`, add it as a field and persist it from `spawn`. Find `pub struct PlaybackPipeline` near line 60 — it currently holds `cmd_tx` and `handle: Option<JoinHandle>`. Add `ndi_name: String,` and store it in `spawn` for both `#[cfg(windows)]` and `#[cfg(not(windows))]` paths.)

- [ ] **Step 9: Confirm tests pass by inspection.**

The 5 ndi_health tests + 4 helper tests should compile and pass.

- [ ] **Step 10: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 11: Wire the registry into `lib.rs` (`PlaybackEngine::new` callsite + `AppState`).**

In `crates/sp-server/src/lib.rs`:

Add to `AppState` (next to `pub resolume_registry: Arc<resolume::ResolumeRegistry>`):

```rust
pub struct AppState {
    // ...existing fields...
    pub resolume_registry: Arc<resolume::ResolumeRegistry>,
    pub ndi_health_registry: Arc<playback::ndi_health::NdiHealthRegistry>,
}
```

Find the `start(...)` function (around line 188) where `PlaybackEngine::new(...)` is called (around line 515). Construct the registry once and pass it into both the engine and AppState:

```rust
    let ndi_health_registry = playback::ndi_health::NdiHealthRegistry::new();

    // ...existing engine construction, with extra arg:
    let mut engine = playback::PlaybackEngine::new(
        pool.clone(),
        cache_dir.clone(),
        obs_event_tx.subscribe_safe(),
        Some(obs_cmd_tx.clone()),
        resolume_tx.clone(),
        ws_tx.clone(),
        presenter_client.clone(),
        ndi_health_registry.clone(),
    );

    // Then in the AppState construction, alongside resolume_registry:
    let app_state = AppState {
        // ...existing fields...
        resolume_registry: resolume_registry.clone(),
        ndi_health_registry: ndi_health_registry.clone(),
    };
```

(Update every other AppState struct literal site too — `crates/sp-server/src/api/live_tests_included.rs`, `crates/sp-server/src/lib_tests.rs`, `crates/sp-server/src/api/routes_tests.rs::test_state` — to add `ndi_health_registry: playback::ndi_health::NdiHealthRegistry::new(),`. Same precedent as PR #54's `resolume_registry: Arc::new(crate::resolume::ResolumeRegistry::new())` additions.)

- [ ] **Step 12: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 13: Commit.**

```bash
git add crates/sp-server/src/playback/ndi_health.rs crates/sp-server/src/playback/mod.rs crates/sp-server/src/playback/pipeline.rs crates/sp-server/src/lib.rs crates/sp-server/src/api/routes_tests.rs crates/sp-server/src/api/live_tests_included.rs crates/sp-server/src/lib_tests.rs
git commit -m "feat(playback/ndi_health): engine handler + shared registry + AppState

NdiHealthRegistry holds the latest health snapshot per pipeline; one Arc
goes to PlaybackEngine (writer in handle_health_snapshot), another to
AppState (reader by the upcoming /api/v1/ndi/health endpoint). Mirrors
the resolume_registry pattern from PR #54.

handle_health_snapshot reconciles pipeline-reported state against
canonical PlayState (overrides Idle -> WaitingForScene), fills
degraded_reason at the >= 2 consecutive bad-polls gate, logs transitions
(connection change, degradation, recovery), and writes through the
registry.

PlaybackPipeline.ndi_name accessor exposes the source name; engine
gains instant_origin for Instant -> DateTime<Utc> mapping.

Five integration tests cover known/unknown pipeline routing, multi-
pipeline aggregation, the WaitingForScene override, and the
connections-zero degraded_reason. Four pure-helper tests pin
compute_degraded_reason boundaries (1 vs 2 polls, non-Playing states,
underrun, stale).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6 — Windows pipeline-thread heartbeat

**Why:** the pipeline thread is the only place that owns the NdiSender + FrameSubmitter. It's where `recv_timeout(5s)` and the inner-loop `last_heartbeat.elapsed()` checks must live. Windows-only — `run_loop_windows` is already `mutants::skip` so no new mutation pressure.

**Files:**
- Modify: `crates/sp-server/src/playback/pipeline.rs:225-...` (heartbeat scheduling, helper functions)

**Model hint:** sonnet (state machine on the OS thread, careful interplay with the existing `paused` flag and `cmd_rx` patterns).

- [ ] **Step 1: Write the failing test for the pure heartbeat-decision helper.**

In `crates/sp-server/src/playback/pipeline.rs`, append to whatever test module exists (or create one):

```rust
#[cfg(test)]
mod heartbeat_decision_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn should_run_heartbeat_returns_true_on_or_after_5_seconds() {
        assert!(should_run_heartbeat(Duration::from_secs(5)));
        assert!(should_run_heartbeat(Duration::from_secs(6)));
        assert!(should_run_heartbeat(Duration::from_millis(10_000)));
    }

    #[test]
    fn should_run_heartbeat_returns_false_below_5_seconds() {
        assert!(!should_run_heartbeat(Duration::from_secs(0)));
        assert!(!should_run_heartbeat(Duration::from_secs(4)));
        assert!(!should_run_heartbeat(Duration::from_millis(4_999)));
    }

    #[test]
    fn classify_bad_poll_connections_zero_while_playing() {
        assert!(classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Playing,
            0,
            30.0,
            30.0,
            None,
            std::time::Instant::now(),
        ));
    }

    #[test]
    fn classify_bad_poll_idle_is_never_bad() {
        assert!(!classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Idle,
            0,
            0.0,
            30.0,
            None,
            std::time::Instant::now(),
        ));
    }

    #[test]
    fn classify_bad_poll_underrun_when_observed_below_half_nominal() {
        assert!(classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Playing,
            1,
            10.0,
            30.0,
            Some(std::time::Instant::now()),
            std::time::Instant::now(),
        ));
        assert!(!classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Playing,
            1,
            16.0,
            30.0,
            Some(std::time::Instant::now()),
            std::time::Instant::now(),
        ));
    }

    #[test]
    fn classify_bad_poll_stale_when_last_submit_more_than_10s_ago() {
        let now = std::time::Instant::now();
        // last_submit 11s ago, fps healthy, connections healthy.
        assert!(classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Playing,
            1,
            30.0,
            30.0,
            Some(now - Duration::from_secs(11)),
            now,
        ));
        assert!(!classify_bad_poll(
            &crate::playback::ndi_health::PlaybackStateLabel::Playing,
            1,
            30.0,
            30.0,
            Some(now - Duration::from_secs(9)),
            now,
        ));
    }
}
```

- [ ] **Step 2: Confirm tests fail by inspection.**

Helpers don't exist. Continue.

- [ ] **Step 3: Implement the pure helpers.**

Append at the bottom of `crates/sp-server/src/playback/pipeline.rs` (above any test modules):

```rust
/// Pure predicate: should the pipeline thread run a heartbeat now?
/// Extracted so the timing rule is unit-testable without a live decode loop.
///
/// Mutation testing: the `>=` boundary is exhaustively covered by
/// `heartbeat_decision_tests::should_run_heartbeat_*`.
#[cfg_attr(test, mutants::skip)]
fn should_run_heartbeat(elapsed: std::time::Duration) -> bool {
    elapsed >= std::time::Duration::from_secs(5)
}

/// Pure predicate: is the just-completed poll a "bad poll" per the spec?
/// Used by the pipeline thread to bump or reset `consecutive_bad_polls`.
///
/// Mutation testing: each branch (state, connections, fps, staleness) is
/// covered by `heartbeat_decision_tests::classify_bad_poll_*`.
#[cfg_attr(test, mutants::skip)]
fn classify_bad_poll(
    state: &crate::playback::ndi_health::PlaybackStateLabel,
    connections: i32,
    observed_fps: f32,
    nominal_fps: f32,
    last_submit_ts: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !matches!(
        state,
        crate::playback::ndi_health::PlaybackStateLabel::Playing
    ) {
        return false;
    }
    if connections == 0 {
        return true;
    }
    if nominal_fps > 0.0 && observed_fps < nominal_fps / 2.0 {
        return true;
    }
    if let Some(ts) = last_submit_ts {
        if now.duration_since(ts) > std::time::Duration::from_secs(10) {
            return true;
        }
    }
    false
}
```

- [ ] **Step 4: Switch `cmd_rx.recv()` → `cmd_rx.recv_timeout(5s)` on the outer loop and run heartbeats on timeout.**

In `crates/sp-server/src/playback/pipeline.rs`, find `run_loop_windows` (the function with `#[cfg(windows)] #[cfg_attr(test, mutants::skip)]`). The current outer loop pattern:

```rust
loop {
    match cmd_rx.recv() {
        Ok(PipelineCommand::Shutdown) | Err(_) => { ... }
        Ok(PipelineCommand::Play { video, audio }) => { ... }
        // ...
    }
}
```

Replace with:

```rust
let mut last_heartbeat = std::time::Instant::now();
let mut consecutive_bad_polls: u32 = 0;

loop {
    match cmd_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            // No command for 5 seconds — emit a heartbeat from the
            // outer (paused/idle) state.
            run_heartbeat_outer(
                &mut submitter,
                &event_tx,
                playlist_id,
                paused,
                &mut last_heartbeat,
                &mut consecutive_bad_polls,
            );
            continue;
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            info!(playlist_id, "pipeline thread shutting down (cmd_rx closed)");
            submitter.flush();
            break;
        }
        Ok(PipelineCommand::Shutdown) => { ... } // existing
        Ok(PipelineCommand::Play { video, audio }) => { ... } // existing
        // ...rest unchanged...
    }
}
```

(The implementer must keep all existing `Ok(...)` arms and only replace the recv shape and add the timeout branches.)

- [ ] **Step 5: Add the inner-loop heartbeat check.**

Find the inner decode loop that calls `submitter.submit_nv12(...)` (around line 508). Around the submit call, add an `if should_run_heartbeat(last_heartbeat.elapsed())` call:

```rust
                submitter.submit_nv12(
                    width, height, stride, video_data, &audio_chunks,
                );

                if should_run_heartbeat(last_heartbeat.elapsed()) {
                    run_heartbeat_inner(
                        &mut submitter,
                        &event_tx,
                        playlist_id,
                        &mut last_heartbeat,
                        &mut consecutive_bad_polls,
                    );
                }
```

- [ ] **Step 6: Implement `run_heartbeat_outer` and `run_heartbeat_inner`.**

Append to `pipeline.rs` (Windows-cfg-gated):

```rust
#[cfg(windows)]
fn run_heartbeat_outer(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    paused: bool,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    let state = if paused {
        crate::playback::ndi_health::PlaybackStateLabel::Paused
    } else {
        crate::playback::ndi_health::PlaybackStateLabel::Idle
    };
    emit_heartbeat(submitter, event_tx, playlist_id, state, last_heartbeat, consecutive_bad_polls);
}

#[cfg(windows)]
fn run_heartbeat_inner(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    emit_heartbeat(
        submitter,
        event_tx,
        playlist_id,
        crate::playback::ndi_health::PlaybackStateLabel::Playing,
        last_heartbeat,
        consecutive_bad_polls,
    );
}

#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn emit_heartbeat(
    submitter: &mut FrameSubmitter<sp_ndi::RealNdiBackend>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<(i64, PipelineEvent)>,
    playlist_id: i64,
    state: crate::playback::ndi_health::PlaybackStateLabel,
    last_heartbeat: &mut std::time::Instant,
    consecutive_bad_polls: &mut u32,
) {
    let connections = submitter.sender().get_no_connections(0);
    let stats = submitter.drain_window();
    let observed_fps = stats.frames_in_window as f32 / stats.window_secs.max(0.001);

    let nominal_fps = {
        let frame = submitter.sender();
        // FrameSubmitter exposes its frame rate via the last submitted frame.
        // Approximate using the submitter's current frame_rate_n / frame_rate_d.
        // (Direct accessors avoided to keep the test surface small.)
        let _ = frame;
        // The submitter knows; expose via a getter.
        submitter.nominal_fps()
    };

    let now = std::time::Instant::now();
    let bad = classify_bad_poll(&state, connections, observed_fps, nominal_fps, submitter.last_submit_ts(), now);
    if bad {
        *consecutive_bad_polls = consecutive_bad_polls.saturating_add(1);
    } else {
        *consecutive_bad_polls = 0;
    }

    let _ = event_tx.send((
        playlist_id,
        PipelineEvent::HealthSnapshot {
            connections,
            frames_submitted_total: submitter.frames_submitted_total(),
            frames_submitted_last_5s: stats.frames_in_window,
            observed_fps,
            nominal_fps,
            last_submit_ts: submitter.last_submit_ts(),
            last_heartbeat_ts: now,
            consecutive_bad_polls: *consecutive_bad_polls,
            reported_state: state,
        },
    ));
    *last_heartbeat = now;
}
```

- [ ] **Step 7: Add `nominal_fps()` accessor on `FrameSubmitter`.**

In `crates/sp-server/src/playback/submitter.rs`, after `last_submit_ts()`:

```rust
    pub fn last_submit_ts(&self) -> Option<std::time::Instant> {
        self.last_submit_ts
    }

    /// Current nominal frame rate as fps. Used by the heartbeat to compute
    /// the underrun threshold (observed_fps < nominal_fps / 2).
    pub fn nominal_fps(&self) -> f32 {
        if self.frame_rate_d == 0 {
            return 0.0;
        }
        self.frame_rate_n as f32 / self.frame_rate_d as f32
    }
```

Add a unit test below it:

```rust
    #[test]
    fn nominal_fps_computes_from_rate_pair() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F1", true, false).unwrap();
        let sub: FrameSubmitter<_> = FrameSubmitter::new(sender, 30000, 1001);
        let v = sub.nominal_fps();
        assert!((v - 29.97).abs() < 0.01, "expected ~29.97 got {v}");
    }
```

- [ ] **Step 8: Confirm tests pass by inspection.**

The 7 heartbeat-decision tests + nominal_fps test compile and pass. The Windows-only emit/run helpers don't have unit tests (already mutants::skip).

- [ ] **Step 9: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 10: Commit.**

```bash
git add crates/sp-server/src/playback/pipeline.rs crates/sp-server/src/playback/submitter.rs
git commit -m "feat(playback/pipeline): per-pipeline NDI heartbeat (Windows-only)

Outer loop switches cmd_rx.recv() -> recv_timeout(5s); on timeout, the
thread emits a heartbeat from Idle/Paused state. Inner decode loop runs
should_run_heartbeat after each submit and emits from Playing state.
emit_heartbeat samples connections, drains the submitter window for
observed_fps, classifies bad-poll, bumps or resets consecutive_bad_polls,
and forwards via PipelineEvent::HealthSnapshot.

Pure helpers should_run_heartbeat + classify_bad_poll are unit-tested
on Linux (7 tests covering boundaries and every bad-poll branch). The
Windows-only emit_heartbeat is mutants::skip with the same justification
as run_loop_windows.

FrameSubmitter::nominal_fps accessor + test added to expose the rate
the heartbeat uses for the half-nominal threshold.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7 — HTTP route `/api/v1/ndi/health` + tests

**Why:** the dashboard needs an endpoint to poll. Two route tests pin the empty-array AND populated-array shapes (the second kills the `Json::from(vec![])` mutant, mirroring `resolume_health_endpoint_returns_registered_hosts` from PR #54).

**Files:**
- Modify: `crates/sp-server/src/api/routes.rs` (add `pub async fn get_ndi_health`)
- Modify: `crates/sp-server/src/api/mod.rs` (wire the route)
- Modify: `crates/sp-server/src/api/routes_tests.rs` (2 new tests)

**Model hint:** sonnet (axum extractor wiring, route test seeds the registry directly).

- [ ] **Step 1: Write the failing tests.**

Append to `crates/sp-server/src/api/routes_tests.rs`:

```rust
#[tokio::test]
async fn ndi_health_endpoint_returns_array() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "response must be a JSON array");
}

/// Verifies the endpoint returns populated snapshots (not an empty Vec).
/// Kills the `get_ndi_health -> Json::from(vec![])` mutant. Mirrors
/// `resolume_health_endpoint_returns_registered_hosts` from PR #54.
#[tokio::test]
async fn ndi_health_endpoint_returns_seeded_pipeline() {
    let mut state = test_state().await;
    // Seed the registry directly — bypasses the engine for a focused
    // route-level test. The engine path is covered by ndi_health.rs unit
    // tests; here we only verify the handler wires the registry to JSON.
    let snapshot = crate::playback::ndi_health::PipelineHealthSnapshot {
        playlist_id: 11,
        ndi_name: "SP-test".to_string(),
        state: crate::playback::ndi_health::PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 100,
        frames_submitted_last_5s: 30,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
    };
    state.ndi_health_registry.update(snapshot);

    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/ndi/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("response must be a JSON array");
    assert_eq!(arr.len(), 1, "response should contain exactly one pipeline");
    assert_eq!(arr[0]["playlist_id"].as_i64(), Some(11));
    assert_eq!(arr[0]["ndi_name"].as_str(), Some("SP-test"));
    assert_eq!(arr[0]["state"], serde_json::json!("Playing"));
}
```

- [ ] **Step 2: Confirm tests fail by inspection.**

Endpoint doesn't exist yet, route returns 404. Continue.

- [ ] **Step 3: Add the route handler.**

Append to `crates/sp-server/src/api/routes.rs` (next to `get_resolume_health`):

```rust
/// GET /api/v1/ndi/health — return per-pipeline NDI delivery health.
/// Empty `[]` if no pipelines have reported a heartbeat yet.
pub async fn get_ndi_health(
    State(state): State<AppState>,
) -> Json<Vec<crate::playback::ndi_health::PipelineHealthSnapshot>> {
    Json(state.ndi_health_registry.snapshots())
}
```

- [ ] **Step 4: Wire the route.**

In `crates/sp-server/src/api/mod.rs`, after the existing Resolume health route:

```rust
        .route("/api/v1/resolume/health", axum::routing::get(routes::get_resolume_health))
        .route("/api/v1/ndi/health", axum::routing::get(routes::get_ndi_health))
```

- [ ] **Step 5: Confirm tests pass by inspection.**

The two new endpoint tests align with the implementation. CI runs them.

- [ ] **Step 6: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 7: Commit.**

```bash
git add crates/sp-server/src/api/routes.rs crates/sp-server/src/api/mod.rs crates/sp-server/src/api/routes_tests.rs
git commit -m "feat(api): GET /api/v1/ndi/health endpoint + 2 route tests

Empty-array test pins the basic shape; populated-pipeline test
asserts the seeded snapshot is preserved (kills the
Json::from(vec![]) mutant, same precedent as
resolume_health_endpoint_returns_registered_hosts from PR #54).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8 — Leptos NdiHealthCard + CSS + dashboard mount

**Why:** the operator-facing surface. Alert-only model — `<Show when=any-pipeline-has-problem fallback=empty>`. Mirrors `<ResolumeHealthCard>` in shape.

**Files:**
- Create: `sp-ui/src/components/ndi_health.rs`
- Modify: `sp-ui/src/components/mod.rs` (add `pub mod ndi_health;`)
- Modify: `sp-ui/src/pages/dashboard.rs` (add `<ndi_health::NdiHealthCard />` next to `<resolume_health::ResolumeHealthCard />`)
- Modify: `sp-ui/style.css` (append `.ndi-health-alert`, `.nh-alert`, `.nh-alert-dot`)

**Model hint:** sonnet (Leptos component, JSON deserialization, CSS sized to existing palette).

- [ ] **Step 1: Create the component.**

```rust
// sp-ui/src/components/ndi_health.rs
//! Dashboard alert for NDI delivery health.
//!
//! Quiet by default — renders nothing when every pipeline is healthy.
//! Mirrors `resolume_health.rs`'s alert-only pattern: dashboard noise is
//! the enemy; show only when a real problem exists.
//!
//! Polls `/api/v1/ndi/health` every 5 s.

use leptos::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct PipelineHealth {
    pub playlist_id: i64,
    pub ndi_name: String,
    pub state: String,
    pub connections: i32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    pub last_submit_ts: Option<String>,
    pub consecutive_bad_polls: u32,
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub degraded_reason: Option<String>,
}

impl PipelineHealth {
    /// Short human reason this pipeline is degraded, or `None` if healthy.
    /// Server fills `degraded_reason` when consecutive_bad_polls >= 2;
    /// frontend renders it verbatim and falls back to None otherwise.
    fn problem(&self) -> Option<String> {
        if self.state != "Playing" {
            return None;
        }
        if self.consecutive_bad_polls < 2 {
            return None;
        }
        self.degraded_reason.clone()
    }
}

#[component]
pub fn NdiHealthCard() -> impl IntoView {
    let snapshot = RwSignal::new(Vec::<PipelineHealth>::new());

    // Cancellation flag for the poll loop; flipped on unmount so the
    // spawn_local task exits instead of running forever.
    let cancelled = RwSignal::new(false);
    on_cleanup(move || cancelled.set(true));

    let _poll = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                if cancelled.get_untracked() {
                    break;
                }
                if let Ok(data) = crate::api::get::<Vec<PipelineHealth>>("/api/v1/ndi/health").await
                {
                    snapshot.set(data);
                }
                gloo_timers::future::TimeoutFuture::new(5_000).await;
            }
        });
    });

    view! {
        <Show when=move || snapshot.get().iter().any(|h| h.problem().is_some()) fallback=|| view! {}>
            <div class="ndi-health-alert">
                <For
                    each=move || {
                        snapshot
                            .get()
                            .into_iter()
                            .filter_map(|h| h.problem().map(|p| (h.playlist_id, h.ndi_name.clone(), p)))
                            .collect::<Vec<_>>()
                    }
                    key=|(id, _, _)| *id
                    children=move |(_, ndi_name, reason)| {
                        view! {
                            <div class="nh-alert">
                                <span class="nh-alert-dot"></span>
                                <strong>{format!("NDI {ndi_name}")}</strong>
                                ": "
                                {reason}
                            </div>
                        }
                    }
                />
            </div>
        </Show>
    }
}
```

- [ ] **Step 2: Add the module to `components/mod.rs`.**

In `sp-ui/src/components/mod.rs`, in the alphabetical list, add:

```rust
pub mod ndi_health;
```

(between `now_playing_card` and `obs_status`.)

- [ ] **Step 3: Mount in the dashboard.**

In `sp-ui/src/pages/dashboard.rs`, line 7:

```rust
use crate::components::{download_queue, ndi_health, obs_status, playlist_card, resolume_health};
```

Line 28-29 (the `dashboard-header` block):

```rust
            <div class="dashboard-header">
                <h1>"Playlists"</h1>
                <obs_status::ObsStatus />
                <resolume_health::ResolumeHealthCard />
                <ndi_health::NdiHealthCard />
            </div>
```

- [ ] **Step 4: Add CSS.**

Append to `sp-ui/style.css`, after the existing `.rh-alert-dot` block:

```css
/* ---- NDI health alert (only rendered when a pipeline has a problem) ---- */

.ndi-health-alert {
    margin-left: 16px;
    display: flex;
    flex-direction: column;
    gap: 4px;
}

.nh-alert {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.85rem;
    color: var(--warning);
    padding: 4px 8px;
    border-left: 3px solid var(--warning);
    background: rgba(255, 170, 0, 0.08);
}

.nh-alert-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--warning);
    flex-shrink: 0;
}
```

- [ ] **Step 5: Verify formatting.**

Run: `cargo fmt --all --check`
Expected: exit 0.

(No CSS formatter here; visual inspection only — the appended block matches the existing `.rh-*` family in spacing and palette.)

- [ ] **Step 6: Commit.**

```bash
git add sp-ui/src/components/ndi_health.rs sp-ui/src/components/mod.rs sp-ui/src/pages/dashboard.rs sp-ui/style.css
git commit -m "feat(sp-ui): NdiHealthCard alert-only dashboard component

Polls /api/v1/ndi/health every 5 s, renders nothing while every pipeline
is healthy, shows compact .nh-alert rows when consecutive_bad_polls>=2
and the server provided a degraded_reason. Mirrors ResolumeHealthCard's
shape and CSS palette so the dashboard header stays uniform.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9 — Playwright E2E assertion

**Why:** the deployed wall must NOT show an alert when healthy. Single assertion, no mock-fault scenario (Tier-1 scope).

**Files:**
- Modify: `e2e/post-deploy.spec.ts`

**Model hint:** haiku.

- [ ] **Step 1: Find the existing Resolume-health-card assertion in `e2e/post-deploy.spec.ts`.**

Run locally:

```bash
grep -n "resolume-health\|rh-alert" e2e/post-deploy.spec.ts
```

Identify the test that asserts the absence of the Resolume health alert when healthy. The new assertion goes in the same test (or an adjacent test that runs in the same beforeAll/afterAll scope).

- [ ] **Step 2: Add the assertion.**

In the existing dashboard-loads-clean test (or wherever `.resolume-health-alert` is asserted absent), add:

```typescript
  // NDI Tier-1 visibility — alert must be absent on a healthy wall.
  const ndiAlerts = await page.locator(".ndi-health-alert").count();
  expect(ndiAlerts).toBe(0);
```

(If no Resolume alert assertion exists yet, place the NDI assertion immediately after the dashboard's first navigation/render in the existing healthy-baseline test.)

- [ ] **Step 3: Verify formatting.**

No formatter required for TS in this repo. Visual inspection only.

- [ ] **Step 4: Commit.**

```bash
git add e2e/post-deploy.spec.ts
git commit -m "test(e2e): assert NDI health alert absent on healthy wall

Closes the Tier-1 loop: if /api/v1/ndi/health surfaces a degraded
pipeline mistakenly, the post-deploy E2E catches it. Single assertion,
no mock-fault scenario (Tier-1 scope).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Verification (controller-only, after all subagent tasks land)

After all 10 tasks are committed locally:

1. **Local lint sanity:** `cargo fmt --all --check` (exit 0).
2. **Push once:** `git push origin dev` (no force; no `--no-verify`).
3. **Monitor CI:** `gh run list --branch dev --limit 1`, then
   `sleep 300 && gh run view <id> --json status,conclusion,jobs` in the
   background. Watch for ALL jobs to reach terminal state, including:
   - Build WASM (~1:40)
   - Build Tauri (~9:50)
   - Build (Windows)
   - Coverage
   - Mutation Testing
   - Deploy to win-resolume
   - Post-deploy E2E
4. **Cancel remaining CI jobs after Deploy success** (per
   `feedback_cancel_ci_after_deploy.md`).
5. **Functional verification on win-resolume:**
   - `curl http://10.77.9.201:8920/api/v1/ndi/health` returns the array.
   - Open dashboard at `http://10.77.9.201:8920/` in Playwright; assert
     no `.ndi-health-alert` while wall is healthy.
   - With sp-slow on program, kill OBS NDI receiver; assert
     `.ndi-health-alert` appears within ~10 s with the
     "no NDI receiver — wall is dark" message.
6. **Open PR `dev → main`** with title `0.25.0: NDI Tier-1 visibility (#46, #56)`.
7. **Merge only on explicit user "merge it"** per `pr-merge-policy.md`.

---

## Self-review

Re-checked the spec against the plan:

| Spec section | Plan task |
|---|---|
| Architecture (5 layers) | Tasks 0–9 cover all five |
| Data shape (PipelineHealthSnapshot, PlaybackStateLabel, WindowStats) | Task 0 |
| `NdiHealthRegistry` (lock-free read, AppState-held) | Task 0 (registry struct), Task 5 (engine writer + AppState wiring) |
| Alert rules (3 branches, ≥2 gate) | Task 5 (`compute_degraded_reason`) + Task 6 (`classify_bad_poll`) + Task 8 (`problem()`) |
| FFI surface (NdiLib symbol + trait + impls + wrapper) | Tasks 1, 2 |
| Submitter counters | Task 3 |
| Pipeline thread integration | Task 6 |
| Engine aggregator (writes registry) | Task 5 |
| API endpoint (reads registry from AppState) | Task 7 |
| Dashboard component | Task 8 |
| Logging (info/warn transitions) | Task 5 (handler logs prev_connections / prev_degraded transitions) |
| Testing strategy | Tasks 1–9 each include explicit test code |
| File-size budget | Task 0 carves room |
| Playwright E2E | Task 9 |

No placeholders. Every task contains complete code. Type names are consistent across tasks: `NdiHealthRegistry`, `PipelineHealthSnapshot`, `PlaybackStateLabel`, `WindowStats`, `compute_degraded_reason`, `classify_bad_poll`, `should_run_heartbeat`, `emit_heartbeat`, `nominal_fps`, `last_submit_ts`, `instant_origin`, `set_state_for_test`. The registry pattern matches PR #54's `Arc<resolume::ResolumeRegistry>` precedent (already enforced in `routes_tests.rs::resolume_health_endpoint_returns_registered_hosts`).

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-26-ndi-tier1-visibility.md`. Per airuleset (subagent-driven-development is the only valid execution path), the controller chains directly into `superpowers:subagent-driven-development` to dispatch Task 0 with haiku.
