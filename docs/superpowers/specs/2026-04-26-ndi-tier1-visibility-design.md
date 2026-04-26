# NDI Tier-1 Visibility — Design

**Date:** 2026-04-26
**Closes:** #46 (NDI dark wall recurring)
**Implements:** #56 acceptance criteria
**Companion to:** PR #54 (Resolume push hardening) — same shape, NDI side.
**Status:** brainstormed and approved by user.

## Problem

After PR #54 (Resolume push chain visibility), the production ledger has
two independent root causes for "wall goes dark while dashboard says
playing": NDI delivery silently failing, and AV1-codec decoder underrun
(tracked separately as #55). The current code is blind to NDI delivery
failures because:

1. `NDIlib_send_send_video_async_v2` is `void` by SDK design — no return
   code to check.
2. `submitter.submit_nv12` returns `()`; nothing counts frames going out.
3. Connection count from the receiver side (`NDIlib_send_get_no_connections`)
   is not bound in `sp-ndi` at all.

When delivery breaks, the pipeline thread keeps decoding and emitting
`PipelineEvent::Position` events, the dashboard advances seconds, but the
LED wall is dark. The operator finds out by looking at the wall —
unacceptable for live production.

## Goal

Add Tier-1 observability to the NDI pipeline so the dashboard surfaces a
real-time alert the moment delivery breaks, before the audience does.
Same alert-only model as the Resolume health card from PR #54: render
nothing while healthy, compact alert only when something is wrong.

## Non-goals (explicit out-of-scope)

- **Tier 2 auto-recovery** (destroy + recreate sender on stuck connections).
  Separate follow-up issue.
- **Tier 3 CI NDI receiver harness** (probe each `SP-*` sender from CI to
  catch silent-delivery class of bugs at PR-time). Separate follow-up.
- **AV1 codec avoidance** (#55). Same dark-wall symptom, different root
  cause; separate sub-project.
- **Database changes / pipeline version bump.** Pure observability addition.

## Architecture

Five layers, each with a single responsibility:

```
sp-ndi (FFI)            → NdiLib gains send_get_no_connections symbol
                          NdiBackend trait + RealNdiBackend + MockNdiBackend
                          NdiSender::get_no_connections(timeout_ms=0) wrapper

submitter (counters)    → FrameSubmitter tracks frames_submitted_total,
                          last_submit_ts, rolling 5s frame count.
                          Cross-platform; unit-testable on Linux.

pipeline thread (host)  → Windows decode loop owns the heartbeat clock.
                          Outer loop: cmd_rx.recv_timeout(5s) instead of recv().
                          Inner loop: between frames, if 5s elapsed → heartbeat.
                          Heartbeat samples connections, computes fps,
                          emits PipelineEvent::HealthSnapshot.

engine (aggregator)     → PlaylistPipeline.cached_health updated on
                          HealthSnapshot. PlaybackEngine::ndi_health_snapshots()
                          returns Vec.

api + dashboard (out)   → GET /api/v1/ndi/health → Vec<PipelineHealthSnapshot>
                          <NdiHealthCard> alert-only Leptos component, polls 5s.
```

## Data shape

```rust
// crates/sp-server/src/playback/ndi_health.rs (new module)
#[derive(Clone, Debug, Serialize)]
pub struct PipelineHealthSnapshot {
    pub playlist_id: i64,
    pub ndi_name: String,                  // e.g. "SP-fast"
    pub state: PlaybackStateLabel,         // Idle | WaitingForScene | Playing | Paused
    pub connections: i32,                  // -1 if never polled
    pub frames_submitted_total: u64,
    pub frames_submitted_last_5s: u32,
    pub observed_fps: f32,
    pub nominal_fps: f32,
    pub last_submit_ts: Option<DateTime<Utc>>,
    pub last_heartbeat_ts: Option<DateTime<Utc>>,
    pub consecutive_bad_polls: u32,        // for the >=2 gate
    pub degraded_reason: Option<String>,   // populated server-side when
                                            // consecutive_bad_polls >= 2;
                                            // dashboard renders verbatim.
}

#[derive(Clone, Debug, Serialize)]
pub enum PlaybackStateLabel {
    Idle,
    WaitingForScene,
    Playing,
    Paused,
}
```

## Alert rules

A "bad poll" while `state == Playing`:

- `connections == 0`, OR
- `observed_fps < 0.5 * nominal_fps` (and `nominal_fps > 0`), OR
- `last_submit_ts` is more than 10 seconds ago

Tracked on the pipeline thread. `consecutive_bad_polls` increments on a
bad poll, resets to 0 on a clean poll.

Frontend `problem()` (matches `<ResolumeHealthCard>::problem()` shape):

| state                  | bad-poll branch                      | consecutive | Result                                     |
|------------------------|--------------------------------------|-------------|--------------------------------------------|
| `!= Playing`           | (any)                                | (any)       | `None` — silent                            |
| `Playing`              | `connections == 0`                   | `>= 2`      | `Some("no NDI receiver — wall is dark")`   |
| `Playing`              | `observed_fps < nominal/2`           | `>= 2`      | `Some("underrunning ({obs:.0}/{nom:.0} fps)")` |
| `Playing`              | `last_submit_ts > 10s ago`           | `>= 2`      | `Some("no frames in {N}s")`                |
| (otherwise)            |                                      |             | `None`                                     |

Idle / WaitingForScene / Paused are silent — black-frame standby with
no OBS subscriber is the expected steady state and must not raise
alerts. The `>= 2 consecutive` gate matches the
`FAIL_WARN_THRESHOLD = 2` precedent set by PR #54's
`should_emit_repeated_failure_warn` in
`crates/sp-server/src/resolume/driver.rs`.

## FFI surface (sp-ndi)

```rust
// crates/sp-ndi/src/ndi_sdk.rs — new pieces
type FnSendGetNoConnections =
    unsafe extern "C" fn(*mut NDIlib_send_instance_t, u32) -> i32;

pub struct NdiLib {
    // ...existing fields...
    pub(crate) send_get_no_connections: FnSendGetNoConnections,
}

// Resolved in NdiLib::load() under b"NDIlib_send_get_no_connections\0".
```

```rust
// crates/sp-ndi/src/sender.rs — extend NdiBackend
pub trait NdiBackend: Send + Sync {
    // ...existing methods...
    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32;
}

impl NdiBackend for RealNdiBackend { /* call self.lib.send_get_no_connections */ }

// New safe wrapper
impl<B: NdiBackend> NdiSender<B> {
    pub fn get_no_connections(&self, timeout_ms: u32) -> i32 { ... }
}
```

`MockNdiBackend` (in `sp-ndi::test_util`) gains:

```rust
impl MockNdiBackend {
    pub fn set_connection_count(&self, n: i32) { ... }
}

impl NdiBackend for MockNdiBackend {
    fn send_get_no_connections(&self, _handle: usize, _timeout_ms: u32) -> i32 {
        self.connection_count.load(Ordering::SeqCst)
    }
}
```

This lets every alert-rule branch be exercised on the Linux mutation
runner without an NDI runtime.

**Timeout semantics.** SDK convention: with `timeout_ms = 0` the call
returns immediately with the cached count. With `> 0` it blocks until
the count changes or timeout. We always pass 0; the pipeline thread
never blocks on this call.

## Submitter counters (cross-platform)

```rust
// crates/sp-server/src/playback/submitter.rs — additions
pub struct FrameSubmitter<B: NdiBackend> {
    // ...existing fields...
    frames_submitted_total: u64,
    frame_window_start: Instant,
    frames_in_window: u32,
    last_submit_ts: Option<Instant>,
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// Snapshot the rolling window and reset the counter. Caller is
    /// responsible for combining this with sender.get_no_connections().
    pub fn drain_window(&mut self) -> WindowStats { ... }

    pub fn frames_submitted_total(&self) -> u64 { self.frames_submitted_total }
    pub fn last_submit_ts(&self) -> Option<Instant> { self.last_submit_ts }
}

pub struct WindowStats {
    pub frames_in_window: u32,
    pub window_secs: f32,         // wall-clock seconds since last drain
}
```

`submit_nv12` increments `frames_submitted_total`, `frames_in_window`,
and updates `last_submit_ts` to `Instant::now()`. `send_black_bgra` does
NOT count — black-frame standby is not "playback".

## Pipeline thread integration (Windows-only)

`crates/sp-server/src/playback/pipeline.rs::run_loop_windows`:

1. Maintain heartbeat state on the stack:
   - `last_heartbeat: Instant`
   - `consecutive_bad_polls: u32`
2. **Outer loop:** `cmd_rx.recv()` → `cmd_rx.recv_timeout(Duration::from_secs(5))`.
   On `RecvTimeoutError::Timeout`, run `heartbeat(...)` and re-loop.
3. **Inner decode loop:** between frame submissions, if
   `last_heartbeat.elapsed() >= Duration::from_secs(5)`, run
   `heartbeat(...)` and update `last_heartbeat`.
4. **`heartbeat(...)`** function:
   - Sample `submitter.sender().get_no_connections(0) → connections`.
   - `WindowStats { frames_in_window, window_secs } = submitter.drain_window()`.
   - `observed_fps = frames_in_window as f32 / window_secs.max(0.001)`.
   - Determine local `state` from loop position (outer wait → Idle/Paused
     depending on `paused`, inner decode → Playing). For `WaitingForScene`
     the engine knows but the pipeline doesn't; pipeline reports `Idle`
     and the engine maps it before publishing to the API (see Engine
     aggregator below).
   - Compute bad-poll predicate; bump or reset `consecutive_bad_polls`.
   - Build `PipelineEvent::HealthSnapshot { ... }` and send via
     `event_tx`. Engine reconciles state label.

`run_loop_windows` is `mutants::skip` already; the heartbeat-scheduling
mutations are exercised by FrameSubmitter unit tests + an engine-side
state machine test that drives `cached_health` from synthetic
`HealthSnapshot` events. Only the Windows-only `recv_timeout` plumbing
itself is untested by mutation; that's the same status quo as the
existing decode loop.

## Engine aggregator

```rust
// crates/sp-server/src/playback/mod.rs — new variant
pub enum PipelineEvent {
    // ...existing...
    HealthSnapshot {
        connections: i32,
        frames_submitted_total: u64,
        frames_submitted_last_5s: u32,
        observed_fps: f32,
        nominal_fps: f32,
        last_submit_ts: Option<Instant>,
        last_heartbeat_ts: Instant,
        consecutive_bad_polls: u32,
        reported_state: PlaybackStateLabel,
    },
}

struct PlaylistPipeline {
    // ...existing...
    cached_health: Option<PipelineHealthSnapshot>,
}

impl PlaybackEngine {
    pub fn ndi_health_snapshots(&self) -> Vec<PipelineHealthSnapshot> { ... }
}
```

The engine reconciles `reported_state` against its own canonical
`PlayState`: when the engine knows a pipeline is `WaitingForScene` (no
program scene matches), it overrides the pipeline's `Idle` label with
`WaitingForScene` before publishing. The pipeline thread isn't aware of
scene state and shouldn't be.

When `HealthSnapshot` arrives, the engine maps `Instant` → `DateTime<Utc>`
using a fixed `Instant`-to-`SystemTime` reference captured at engine
startup, so the API output uses absolute timestamps.

**File-size budget for `playback/mod.rs`.** Currently 966 lines; cap is
1000. Health-snapshot handling lands in a new sibling
`crates/sp-server/src/playback/ndi_health.rs` (mirror of
`crates/sp-server/src/playback/recovery.rs`, 61 lines) so `mod.rs`
gains only the `mod ndi_health;` declaration plus one event-handler
delegate. **Plan task 0** is "extract a `ndi_health.rs` skeleton with
the type definitions" before any other code is added, so the file-size
gate never trips during the rest of the plan.

## API endpoint

```rust
// crates/sp-server/src/api/routes.rs
pub async fn get_ndi_health(
    State(state): State<AppState>,
) -> Json<Vec<PipelineHealthSnapshot>> {
    Json(state.playback_engine.ndi_health_snapshots())
}
```

Wired in `crates/sp-server/src/api/mod.rs`:

```rust
.route("/api/v1/ndi/health", axum::routing::get(routes::get_ndi_health))
```

Empty registry returns `[]`. Tests assert non-empty registry returns
populated array (kills the `Json::from(vec![])` mutant the same way
PR #54's `resolume_health_endpoint_returns_registered_hosts` did).

## Dashboard component

`sp-ui/src/components/ndi_health.rs`:

```rust
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
    fn problem(&self) -> Option<String> {
        if self.state != "Playing" { return None; }
        if self.consecutive_bad_polls < 2 { return None; }
        if self.connections == 0 {
            return Some("no NDI receiver — wall is dark".into());
        }
        if self.nominal_fps > 0.0 && self.observed_fps < self.nominal_fps / 2.0 {
            return Some(format!(
                "underrunning ({obs:.0}/{nom:.0} fps)",
                obs = self.observed_fps, nom = self.nominal_fps,
            ));
        }
        // Stale-frames branch — at this point consecutive_bad_polls >= 2
        // but neither connections nor fps tripped, so by elimination the
        // server already classified this as "no frames in 10s". Use the
        // server-provided `degraded_reason` field directly so the
        // dashboard does not depend on browser/server clock alignment.
        self.degraded_reason.clone()
    }
}

#[component]
pub fn NdiHealthCard() -> impl IntoView {
    // GET /api/v1/ndi/health every 5s; <Show when=any-problem fallback=empty>;
    // <For> renders one .nh-alert per pipeline with a problem.
}
```

CSS classes `.ndi-health-alert`, `.nh-alert`, `.nh-alert-dot` mirror the
Resolume `.resolume-health-alert` family in `sp-ui/style.css`.

The card mounts in the same dashboard region as `<ResolumeHealthCard>`.
When healthy: zero pixels rendered; matches the user's hard rule from
PR #54 ("alert-only, render nothing when healthy").

## Logging

- `info!` on every connection-count change for a pipeline:
  `ndi: connections changed pipeline=N name=SP-fast prev=1 now=0`.
- `warn!` the first time `consecutive_bad_polls` hits the 2 threshold:
  `ndi: pipeline degraded ... reason=...`.
- `info!` when a previously-degraded pipeline returns to clean:
  `ndi: pipeline recovered ...`.
- Fps logging is `debug!` per heartbeat; `info!` only at degradation/recovery
  transitions. Avoids log spam (5s ticks × 6 playlists = 4320 lines/hour at
  `info!` if every tick logged).

## Testing strategy

| Layer | Test type | Test file |
|---|---|---|
| FFI loader resolves new symbol | unit | `crates/sp-ndi/src/ndi_sdk.rs` (existing tests) |
| `NdiBackend::send_get_no_connections` on Real impl | unit | `crates/sp-ndi/src/sender.rs` |
| `NdiBackend::send_get_no_connections` on Mock impl | unit | `crates/sp-ndi/src/sender.rs` (test_util section) |
| `NdiSender::get_no_connections` round-trip | unit | `crates/sp-ndi/src/sender.rs` |
| Submitter increments counters | unit | `crates/sp-server/src/playback/submitter.rs` |
| Submitter `drain_window` resets the bucket | unit | same |
| `send_black_bgra` does NOT count | unit | same |
| Engine maps `HealthSnapshot` event → `cached_health` | unit | new `crates/sp-server/src/playback/ndi_health.rs` |
| `ndi_health_snapshots()` returns one entry per pipeline | unit | same |
| Engine overrides `Idle` → `WaitingForScene` correctly | unit | same |
| `problem()` returns None on `state != Playing` | unit (Rust + WASM) | both health files |
| `problem()` returns connections message on >= 2 bad polls | unit | same |
| `problem()` returns underrun message on >= 2 bad polls | unit | same |
| `problem()` returns None on 1 bad poll | unit | same |
| HTTP `GET /api/v1/ndi/health` returns `[]` empty | route test | `crates/sp-server/src/api/routes_tests.rs` |
| HTTP populated registry → JSON array | route test | same (kills `Json::from(vec![])` mutant) |
| Dashboard hides card when all healthy | E2E | `e2e/post-deploy.spec.ts` |
| Dashboard shows card on simulated fault | optional E2E | same (mock-api injected fault) |

## File-size budget

| File | Current | After | Headroom |
|---|---:|---:|---:|
| `crates/sp-ndi/src/ndi_sdk.rs` | 198 | ~225 | comfortable |
| `crates/sp-ndi/src/sender.rs` | 803 | ~900 | tight; if it threatens 1000 the Mock impl + Mock tests split into `sp-ndi/src/test_util.rs` |
| `crates/sp-server/src/playback/submitter.rs` | 345 | ~440 | comfortable |
| `crates/sp-server/src/playback/pipeline.rs` | 738 | ~830 | comfortable |
| `crates/sp-server/src/playback/mod.rs` | 966 | ~985 | extracted via plan task 0 (`ndi_health.rs`) |
| `crates/sp-server/src/playback/ndi_health.rs` | new | ~150 | new |
| `crates/sp-server/src/api/routes.rs` | (large) | +25 | already healthy |
| `sp-ui/src/components/ndi_health.rs` | new | ~110 | new |

## Risks

1. **`get_no_connections` blocking semantics.** Mitigated by always
   passing `timeout_ms = 0`.
2. **`recv_timeout` on the outer loop changes startup latency.** The
   first heartbeat fires up to 5s after pipeline start; acceptable.
3. **State-label inference drift between pipeline thread and engine.**
   The engine is canonical; mismatch falls back to the engine's view.
   Handler test asserts the override.
4. **CI test for the recv_timeout path.** Windows-only and tied to live
   NDI; the cross-platform alternative is engine-side state-machine
   tests using synthetic `HealthSnapshot` events. Same status quo as
   `run_loop_windows`.
5. **No DB migration.** Pure additive observability; no schema or
   pipeline-version implications.

## Acceptance criteria (mapping #56 → this design)

- [x] Bind `NDIlib_send_get_no_connections(handle, timeout_ms)` in `sp-ndi`. → FFI surface section.
- [x] Per-pipeline heartbeat polls connection count every 5 s. → Pipeline thread integration.
- [x] `info!` on connection-count changes; `warn!` when count drops to 0 while Playing. → Logging section.
- [x] Submitter tracks frame submissions; rate / 5s. `warn!` if rate < 50% nominal. → Submitter counters + alert rules.
- [x] New endpoint `GET /api/v1/ndi/health`. → API endpoint section.
- [x] New `<NdiHealthCard>` Leptos component, polls every 5s, alert-only. → Dashboard component section.

## Self-review note (for the implementer)

Tasks must follow the airuleset:
- TDD strict: failing test first → confirm fail → implement → confirm pass → `cargo fmt --all --check` → commit.
- Never run `cargo clippy/test/build` locally; rely on CI.
- File-size cap 1000 lines; Plan task 0 extracts `ndi_health.rs` first.
- `mutants::skip` requires a one-line justification inline.
- One commit per "Commit" step in the plan; controller pushes once per phase.
- No DB migration / pipeline-version bump.
