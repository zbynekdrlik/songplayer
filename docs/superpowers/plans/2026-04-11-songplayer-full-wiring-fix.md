# SongPlayer Full Wiring Fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every bug that prevents SongPlayer from working end-to-end in production (scene detection, dashboard buttons, NowPlaying broadcast, duration reporting, Gemini cooldown), AND add the missing integration and Playwright E2E tests that would have caught each bug under green CI.

**Architecture:** Surgical fixes across `sp-server` (scene detection populates `ndi_sources` from DB; engine broadcasts `ServerMsg`; reprocess cooldown; new `Previous` endpoint), `sp-ui` (dashboard buttons use path-based REST), `sp-decoder` (read `MF_PD_DURATION` at open), and `.github/workflows/ci.yml` (post-deploy E2E actually drives OBS and the dashboard). All work lands in a single PR on `dev`.

**Tech Stack:** Rust 2024, Tauri 2, Leptos 0.7, Axum 0.8, sqlx 0.8, tokio-tungstenite 0.26 (for the fake OBS test harness), windows 0.58 (`MF_PD_DURATION`), Playwright + obs-websocket-js 5.0 (for real post-deploy drive).

**Design spec:** this document. The bugs were diagnosed in the 2026-04-11 investigation session and filed as issues #8, #9, #11, #12; the duration bug was found during investigation and is in-scope per user decision; the missing test coverage is mandatory per airuleset `e2e-real-user-testing` and the new `never skip CI tests` rule.

**Branch:** `dev`, target `main`.

**CRITICAL RULES — do not violate:**
- **TDD red-green-refactor on every task.** Write the failing test first, run it to see RED, implement the minimum to see GREEN, run the full workspace test suite to catch regressions, commit.
- **NEVER skip, ignore, `#[ignore]`, `continue-on-error`, or bypass a test failure.** If a test fails, fix the root cause. If the test itself is wrong, fix the test. If you cannot, halt and escalate to the user.
- **NEVER run `cargo check`, `cargo clippy`, `cargo test`, `cargo build`, `cargo tauri build`, or `trunk build` locally.** Only `cargo fmt --all --check` runs locally. Everything else runs on CI. (Exception: mutation-skipped Windows-only code still gets `cargo fmt`.)
- **Bugs first, polish second.** Do not refactor surrounding code or reorganise modules unless the bug fix requires it. YAGNI.
- **NO `duration_ms: 0` sentinel survives into `ServerMsg::NowPlaying`.** The decoder fix must land before (or as part of) the NowPlaying broadcast task.

---

## Context

The 2026-04-11 live investigation on win-resolume revealed that **scene-driven playback has never worked** — `crates/sp-server/src/lib.rs:268` creates the `ndi_sources: NdiSourceMap` as `HashMap::new()` and nothing ever populates it. Every scene-item lookup in `check_scene_items` returns `None`, so the playback engine state machine stays stuck in `Idle` for every playlist, and no video reaches OBS unless a human hits `POST /api/v1/playback/{id}/play` directly.

At the same time, the dashboard's Play/Pause/Skip buttons POST to `/api/v1/control` (which does not exist, returning 405), the playback engine receives `PipelineEvent::Started`/`Position`/`Ended` events but never forwards them as `ServerMsg::NowPlaying`/`PlaybackStateChanged` to the WebSocket broadcast channel (so the dashboard card stays on "Nothing playing" forever), the Gemini reprocess worker hammers rate-limited requests in a tight loop with no cooldown, and `sp-decoder::MediaReader::duration_ms` returns `0` at open time (only updated as frames decode, so `PipelineEvent::Started { duration_ms: 0 }` breaks the title-hide 3.5s-before-end timer).

The common thread behind all of these shipping under green CI is that the existing tests exercise mock data structures rather than the real production wiring. `scene.rs::test_ndi_source_matching` hand-builds a HashMap and tests `.get()`. `e2e/frontend.spec.ts` runs against a mock API that lies about routes. No test simulates an OBS scene change end-to-end. No test clicks the Play button and asserts the backend response. This plan fixes all of that in one PR.

**Reference files to study before implementing:**
- `crates/sp-server/src/lib.rs` — server orchestration, `AppState`, wiring of all workers.
- `crates/sp-server/src/obs/mod.rs` — OBS WS client loop, reconnect handling.
- `crates/sp-server/src/obs/scene.rs` — scene-item recursion + matching against `ndi_sources`.
- `crates/sp-server/src/obs/text.rs` — helper functions that build OBS WS request JSON.
- `crates/sp-server/src/playback/mod.rs` — `PlaybackEngine`, `handle_pipeline_event`, `execute_action`.
- `crates/sp-server/src/api/routes.rs` — REST handlers for `play`, `pause`, `skip`, `set_mode`.
- `crates/sp-server/src/reprocess/mod.rs` — reprocess worker structure and timing.
- `crates/sp-decoder/src/reader.rs` — `MediaReader::open()`, where duration should be read.
- `sp-ui/src/components/playback_controls.rs` — current broken `/api/v1/control` POSTs.
- `sp-ui/src/store.rs` — dashboard state signals and `dispatch(ServerMsg)`.
- `crates/sp-core/src/ws.rs` — `ClientMsg` / `ServerMsg` definitions.

---

## Phase 1: Foundation — version bump and test harness

### Task 1: Bump VERSION for this development cycle

**Files:**
- Modify: `VERSION`
- Modify: all crate `Cargo.toml` files via `scripts/sync-version.sh`
- Modify: `Cargo.lock` via `cargo update --workspace --offline`

- [ ] **Step 1: Set new dev version**

Change `VERSION` from `0.9.0-dev.1` to `0.9.0-dev.2`.

- [ ] **Step 2: Propagate version to all Cargo.toml + tauri.conf.json**

Run: `./scripts/sync-version.sh`

- [ ] **Step 3: Refresh Cargo.lock without building**

Run: `cargo update --workspace --offline 2>&1 | tail -20`
Expected: lock file picks up new version strings for the 4 workspace crates.
If offline mode fails, use: `cargo update -p sp-core -p sp-ndi -p sp-decoder -p sp-server --offline`
Do NOT run any build/check/test command locally.

- [ ] **Step 4: Verify formatting**

Run: `cargo fmt --all --check`
Expected: no output, exit 0.

- [ ] **Step 5: Commit**

```bash
git add VERSION Cargo.lock Cargo.toml crates/*/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json sp-ui/Cargo.toml
git commit -m "chore: bump version to 0.9.0-dev.2 for full wiring fix"
```

---

### Task 2: Add a FakeObsServer integration test harness

**Why:** The scene detection fix needs an integration test that simulates a real OBS WebSocket server — replying to `Hello`/`Identify`, `GetInputList`, `GetInputSettings`, `GetSceneItemList`, and pushing `CurrentProgramSceneChanged` events. Unit tests against hand-built HashMaps are exactly how the bug shipped. We need a harness.

**Files:**
- Create: `crates/sp-server/tests/fake_obs.rs` — module with `FakeObsServer` that listens on `127.0.0.1:<random>`, accepts one client, performs the OBS WS 5.x auth handshake (no auth), and responds to a scripted set of requests. Push-events via an `mpsc::Sender` handle.

- [ ] **Step 1: Write a placeholder integration test that fails**

```rust
// crates/sp-server/tests/fake_obs.rs
mod harness;

#[tokio::test]
async fn fake_obs_server_accepts_identify() {
    let server = harness::FakeObsServer::spawn().await;
    let url = server.url();

    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut write, mut read) = futures::StreamExt::split(ws);

    // Expect Hello (op 0)
    let hello = harness::read_next_text(&mut read).await.unwrap();
    assert_eq!(hello["op"], 0, "expected Hello op=0");

    // Send Identify (op 1)
    let identify = serde_json::json!({
        "op": 1,
        "d": { "rpcVersion": 1 }
    });
    use futures::SinkExt;
    write.send(tokio_tungstenite::tungstenite::Message::Text(
        identify.to_string().into(),
    )).await.unwrap();

    // Expect Identified (op 2)
    let identified = harness::read_next_text(&mut read).await.unwrap();
    assert_eq!(identified["op"], 2, "expected Identified op=2");

    server.shutdown().await;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Do NOT run `cargo test` locally. Instead, push a branch-ephemeral commit labelled `ci: wip run` and let CI verify the test compiles and fails on the missing harness module. Alternatively, confirm by inspection — since the `harness` module does not exist yet, the compiler will reject the `mod harness;` line.

- [ ] **Step 3: Implement the harness**

Create `crates/sp-server/tests/harness/mod.rs`:

```rust
//! Fake OBS WebSocket server for integration tests.
//!
//! Implements just enough of the OBS WebSocket 5.x protocol to exercise
//! scene detection logic: Hello/Identify handshake, scripted replies to
//! GetInputList / GetInputSettings / GetSceneItemList, and on-demand
//! CurrentProgramSceneChanged events pushed via a control channel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Default)]
pub struct FakeObsState {
    /// Map of OBS input name to inputKind.
    pub inputs: HashMap<String, String>,
    /// Map of OBS input name to a settings object (usually with ndi_source_name).
    pub input_settings: HashMap<String, Value>,
    /// Map of scene name to a list of (sourceName, isGroup, inputKind) tuples.
    pub scene_items: HashMap<String, Vec<(String, bool, String)>>,
}

pub struct FakeObsServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
    event_tx: mpsc::Sender<Value>,
    state: Arc<Mutex<FakeObsState>>,
}

impl FakeObsServer {
    pub async fn spawn() -> Self {
        Self::spawn_with_state(FakeObsState::default()).await
    }

    pub async fn spawn_with_state(initial: FakeObsState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (event_tx, event_rx) = mpsc::channel::<Value>(32);
        let state = Arc::new(Mutex::new(initial));
        let state_clone = state.clone();

        tokio::spawn(async move {
            run_loop(listener, shutdown_rx, event_rx, state_clone).await;
        });

        Self {
            addr,
            shutdown_tx,
            event_tx,
            state,
        }
    }

    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    pub async fn push_scene_change(&self, scene_name: &str) {
        let evt = json!({
            "op": 5,
            "d": {
                "eventType": "CurrentProgramSceneChanged",
                "eventData": { "sceneName": scene_name }
            }
        });
        let _ = self.event_tx.send(evt).await;
    }

    pub async fn update_state<F>(&self, f: F)
    where
        F: FnOnce(&mut FakeObsState),
    {
        let mut s = self.state.lock().await;
        f(&mut s);
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

async fn run_loop(
    listener: TcpListener,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut event_rx: mpsc::Receiver<Value>,
    state: Arc<Mutex<FakeObsState>>,
) {
    loop {
        tokio::select! {
            Ok((tcp, _)) = listener.accept() => {
                let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                handle_client(ws, &mut event_rx, &state).await;
            }
            _ = &mut shutdown_rx => return,
        }
    }
}

async fn handle_client(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    event_rx: &mut mpsc::Receiver<Value>,
    state: &Arc<Mutex<FakeObsState>>,
) {
    let (mut write, mut read) = ws.split();

    // Send Hello (op 0)
    let hello = json!({
        "op": 0,
        "d": {
            "obsWebSocketVersion": "5.6.3",
            "rpcVersion": 1
        }
    });
    write.send(Message::Text(hello.to_string().into())).await.unwrap();

    // Wait for Identify
    loop {
        tokio::select! {
            Some(Ok(msg)) = read.next() => {
                if let Message::Text(text) = msg {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        if val["op"] == 1 {
                            // Send Identified (op 2)
                            let identified = json!({
                                "op": 2,
                                "d": { "negotiatedRpcVersion": 1 }
                            });
                            write.send(Message::Text(identified.to_string().into())).await.unwrap();
                            break;
                        }
                    }
                }
            }
            else => return,
        }
    }

    // Main serve loop — respond to requests and push events.
    loop {
        tokio::select! {
            Some(Ok(msg)) = read.next() => {
                if let Message::Text(text) = msg {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        if val["op"] == 6 {
                            let response = handle_request(&val, state).await;
                            write.send(Message::Text(response.to_string().into())).await.unwrap();
                        }
                    }
                } else if let Message::Close(_) = msg {
                    return;
                }
            }
            Some(evt) = event_rx.recv() => {
                write.send(Message::Text(evt.to_string().into())).await.unwrap();
            }
            else => return,
        }
    }
}

async fn handle_request(req: &Value, state: &Arc<Mutex<FakeObsState>>) -> Value {
    let request_type = req["d"]["requestType"].as_str().unwrap_or("");
    let request_id = req["d"]["requestId"].as_str().unwrap_or("");

    let response_data = match request_type {
        "GetInputList" => {
            let s = state.lock().await;
            let inputs: Vec<Value> = s
                .inputs
                .iter()
                .map(|(name, kind)| {
                    json!({
                        "inputName": name,
                        "inputKind": kind,
                        "unversionedInputKind": kind,
                    })
                })
                .collect();
            json!({ "inputs": inputs })
        }
        "GetInputSettings" => {
            let input_name = req["d"]["requestData"]["inputName"].as_str().unwrap_or("");
            let s = state.lock().await;
            let settings = s
                .input_settings
                .get(input_name)
                .cloned()
                .unwrap_or_else(|| json!({}));
            json!({ "inputSettings": settings, "inputKind": "ndi_source" })
        }
        "GetSceneItemList" => {
            let scene_name = req["d"]["requestData"]["sceneName"].as_str().unwrap_or("");
            let s = state.lock().await;
            let items: Vec<Value> = s
                .scene_items
                .get(scene_name)
                .map(|list| {
                    list.iter()
                        .enumerate()
                        .map(|(i, (name, is_group, kind))| {
                            json!({
                                "sourceName": name,
                                "sceneItemId": i as i64 + 1,
                                "isGroup": is_group,
                                "inputKind": kind,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({ "sceneItems": items })
        }
        _ => json!({}),
    };

    json!({
        "op": 7,
        "d": {
            "requestType": request_type,
            "requestId": request_id,
            "requestStatus": { "result": true, "code": 100 },
            "responseData": response_data,
        }
    })
}

pub async fn read_next_text<S>(read: &mut S) -> Option<Value>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(Ok(msg)) = read.next().await {
        if let Message::Text(text) = msg {
            if let Ok(val) = serde_json::from_str::<Value>(&text) {
                return Some(val);
            }
        }
    }
    None
}
```

- [ ] **Step 4: Run the placeholder test in CI**

Push the commit, monitor `gh run list --branch dev --limit 3`, watch the run, confirm `fake_obs_server_accepts_identify` passes.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/tests/harness/mod.rs crates/sp-server/tests/fake_obs.rs crates/sp-server/Cargo.toml
git commit -m "test: add FakeObsServer integration harness for scene-detection tests"
```

---

## Phase 2: Bug fixes (scene detection, broadcasts, buttons, duration, cooldown)

### Task 3: Fix scene detection — populate `ndi_sources` from DB (issue #11)

**Files:**
- Modify: `crates/sp-server/src/obs/mod.rs` — accept a `pool: SqlitePool` + rebuild function; rebuild on reconnect.
- Modify: `crates/sp-server/src/obs/text.rs` — add `get_input_list_request`, `get_input_settings_request` builders.
- Create: `crates/sp-server/src/obs/ndi_discovery.rs` — `rebuild_ndi_source_map` that queries OBS inputs and matches against DB playlists.
- Modify: `crates/sp-server/src/lib.rs:268` — pass `pool` to `ObsClient::spawn` so the map can be built.
- Modify: `crates/sp-server/src/api/routes.rs` — trigger a rebuild broadcast after playlist CRUD.
- Create: `crates/sp-server/tests/scene_detection.rs` — integration test using FakeObsServer.

- [ ] **Step 1: Write failing integration test**

```rust
// crates/sp-server/tests/scene_detection.rs
mod harness;

use sp_server::db;
use std::collections::HashMap;

#[tokio::test]
async fn scene_change_to_sp_fast_dispatches_play_for_playlist_7() {
    // 1. Seed in-memory DB with playlists matching the production config.
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();

    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
         VALUES (7, 'ytfast', 'https://yt/fast', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 2. Configure FakeObsServer with an OBS input 'sp-fast_video' of kind ndi_source
    //    whose settings point to ndi_source_name=SP-fast, plus an 'sp-fast' scene
    //    that contains 'sp-fast_video' as a scene item.
    let mut state = harness::FakeObsState::default();
    state.inputs.insert("sp-fast_video".into(), "ndi_source".into());
    state.input_settings.insert(
        "sp-fast_video".into(),
        serde_json::json!({ "ndi_source_name": "SP-fast" }),
    );
    state.scene_items.insert(
        "sp-fast".into(),
        vec![("sp-fast_video".into(), false, "ndi_source".into())],
    );
    let fake_obs = harness::FakeObsServer::spawn_with_state(state).await;

    // 3. Construct and run a partial server wire-up: OBS client with the DB
    //    pool, a broadcast channel for ObsEvent, and assert that after a
    //    scene change event, the active_playlist_ids set contains {7}.
    let (obs_event_tx, mut obs_event_rx) =
        tokio::sync::broadcast::channel::<sp_server::obs::ObsEvent>(16);
    let obs_state = std::sync::Arc::new(tokio::sync::RwLock::new(
        sp_server::obs::ObsState::default(),
    ));
    let ndi_sources = std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    let _client = sp_server::obs::ObsClient::spawn(
        sp_server::obs::ObsConfig {
            url: fake_obs.url(),
            password: None,
        },
        pool.clone(),
        ndi_sources.clone(),
        obs_state.clone(),
        obs_event_tx.clone(),
        shutdown_rx,
    );

    // Allow time for connect + initial map rebuild.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the map has been populated from the DB + OBS input list.
    let map = ndi_sources.read().await;
    assert_eq!(
        map.get("sp-fast_video"),
        Some(&7),
        "ndi_sources should map 'sp-fast_video' OBS input name to playlist id 7"
    );
    drop(map);

    // Push a CurrentProgramSceneChanged event for sp-fast.
    fake_obs.push_scene_change("sp-fast").await;

    // Wait for the OBS client to process it and emit SceneChanged for playlist 7.
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        obs_event_rx.recv(),
    )
    .await
    .expect("should receive ObsEvent within 2s")
    .expect("broadcast channel still open");

    match event {
        sp_server::obs::ObsEvent::SceneChanged { playlist_id, on_program } => {
            assert_eq!(playlist_id, 7);
            assert!(on_program);
        }
        other => panic!("expected SceneChanged{{7, true}}, got {other:?}"),
    }

    let _ = shutdown_tx.send(());
    fake_obs.shutdown().await;
}
```

- [ ] **Step 2: Run the test, confirm RED**

Push commit, monitor CI, confirm the new test fails because `ObsClient::spawn` does not accept a `pool` argument, `ndi_sources` is never populated, and `ObsEvent::SceneChanged` is never emitted.

- [ ] **Step 3: Implement the OBS request builders**

Add to `crates/sp-server/src/obs/text.rs`:

```rust
/// Build a GetInputList request filtered to NDI inputs.
pub fn get_input_list_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetInputList",
            "requestId": request_id,
            "requestData": { "inputKind": "ndi_source" }
        }
    })
}

/// Build a GetInputSettings request for a specific input.
pub fn get_input_settings_request(request_id: &str, input_name: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetInputSettings",
            "requestId": request_id,
            "requestData": { "inputName": input_name }
        }
    })
}
```

- [ ] **Step 4: Implement the rebuild logic**

Create `crates/sp-server/src/obs/ndi_discovery.rs`:

```rust
//! NDI source discovery — queries OBS for NDI inputs and matches against
//! the DB's active playlists to build the scene-detection map.

use std::collections::HashMap;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use sqlx::{Row, SqlitePool};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::text::{get_input_list_request, get_input_settings_request};

/// Rebuild the NDI source map by querying OBS and matching against the DB.
///
/// Returns a HashMap keyed by the OBS **input name** (e.g. `sp-fast_video`)
/// whose value is the `playlist_id` for the playlist whose `ndi_output_name`
/// equals the NDI sender name exposed by that OBS input's settings.
pub async fn rebuild_ndi_source_map(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    pool: &SqlitePool,
) -> HashMap<String, i64> {
    let mut map = HashMap::new();

    // 1. Load active playlists {ndi_output_name → playlist_id} from DB.
    let rows = match sqlx::query(
        "SELECT id, ndi_output_name FROM playlists WHERE is_active = 1 AND ndi_output_name != ''",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("rebuild_ndi_source_map: failed to load playlists: {e}");
            return map;
        }
    };

    let mut by_ndi_name: HashMap<String, i64> = HashMap::new();
    for row in &rows {
        let id: i64 = row.get("id");
        let ndi: String = row.get("ndi_output_name");
        by_ndi_name.insert(ndi, id);
    }

    if by_ndi_name.is_empty() {
        debug!("rebuild_ndi_source_map: no active playlists with ndi_output_name");
        return map;
    }

    // 2. Query GetInputList for ndi_source inputs.
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_list_request(&req_id);
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("rebuild_ndi_source_map: send GetInputList failed: {e}");
        return map;
    }

    let response = match wait_for_response(read, &req_id).await {
        Some(r) => r,
        None => {
            warn!("rebuild_ndi_source_map: no GetInputList response");
            return map;
        }
    };

    let inputs = match response["d"]["responseData"]["inputs"].as_array() {
        Some(arr) => arr.clone(),
        None => return map,
    };

    // 3. For each NDI input, query its settings to learn the NDI sender name.
    for input in &inputs {
        let input_name = match input["inputName"].as_str() {
            Some(n) => n.to_string(),
            None => continue,
        };

        let settings_id = uuid::Uuid::new_v4().to_string();
        let settings_req = get_input_settings_request(&settings_id, &input_name);
        if let Err(e) = write.send(Message::Text(settings_req.to_string().into())).await {
            warn!("rebuild_ndi_source_map: send GetInputSettings failed for {input_name}: {e}");
            continue;
        }

        let settings_response = match wait_for_response(read, &settings_id).await {
            Some(r) => r,
            None => continue,
        };

        // NDI source plugin stores sender name under 'ndi_source_name' in input settings.
        let sender_name = match settings_response["d"]["responseData"]["inputSettings"]
            ["ndi_source_name"]
            .as_str()
        {
            Some(s) => s.to_string(),
            None => continue,
        };

        if let Some(&playlist_id) = by_ndi_name.get(&sender_name) {
            debug!(
                "rebuild_ndi_source_map: '{input_name}' → playlist {playlist_id} (NDI sender '{sender_name}')"
            );
            map.insert(input_name, playlist_id);
        }
    }

    info!(
        "rebuild_ndi_source_map: {} OBS inputs mapped to playlists",
        map.len()
    );
    map
}

/// Wait for a RequestResponse (op 7) matching the given request ID.
async fn wait_for_response(
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    request_id: &str,
) -> Option<serde_json::Value> {
    for _ in 0..100 {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let op = json["op"].as_u64().unwrap_or(u64::MAX);
                    if op == 7 && json["d"]["requestId"].as_str() == Some(request_id) {
                        return Some(json);
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                warn!("rebuild_ndi_source_map: WS read error: {e}");
                return None;
            }
        }
    }
    None
}
```

- [ ] **Step 5: Wire the rebuild into `ObsClient::spawn` and the reconnect loop**

Modify `crates/sp-server/src/obs/mod.rs`:
- Add `pool: SqlitePool` parameter to `ObsClient::spawn`.
- In `connect_and_run` (the per-connection function), call `rebuild_ndi_source_map(&mut write, &mut read, &pool)` immediately after Identified and write the result into `ndi_sources` (acquire write lock).
- Expose `pub mod ndi_discovery;`.

Pseudo-patch (full file edit required):

```rust
// in obs::mod.rs at top of module
pub mod ndi_discovery;

// in ObsClient::spawn signature
pub fn spawn(
    config: ObsConfig,
    pool: SqlitePool,                          // NEW
    ndi_sources: NdiSourceMap,
    shared_state: Arc<RwLock<ObsState>>,
    event_tx: broadcast::Sender<ObsEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Self {
    // ... existing ...
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ObsCommand>(64);
    let ndi_sources_clone = ndi_sources.clone();
    let pool_clone = pool.clone();
    let state_clone = state.clone();
    let loop_event_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            let result = connect_and_run(
                &config,
                &pool_clone,
                &ndi_sources_clone,
                &state_clone,
                &loop_event_tx,
                &mut cmd_rx,
            )
            .await;
            // ... reconnect logic unchanged
        }
    });
    // ...
}

// in connect_and_run function signature
async fn connect_and_run(
    config: &ObsConfig,
    pool: &SqlitePool,                        // NEW
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
    cmd_rx: &mut mpsc::Receiver<ObsCommand>,
) -> Result<(), anyhow::Error> {
    // ... existing connect + identify ...

    // After Identified, rebuild the NDI source map.
    let new_map = ndi_discovery::rebuild_ndi_source_map(&mut write, &mut read, pool).await;
    {
        let mut guard = ndi_sources.write().await;
        *guard = new_map;
    }

    // ... continue into the main event loop unchanged
}
```

- [ ] **Step 6: Update `lib.rs:268` to pass `pool`**

```rust
// before:
let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
let obs_client = obs::ObsClient::spawn(
    obs_config,
    ndi_sources,
    obs_state.clone(),
    obs_event_tx.clone(),
    shutdown_tx.subscribe(),
);

// after:
let ndi_sources: obs::NdiSourceMap = Arc::new(RwLock::new(HashMap::new()));
let obs_client = obs::ObsClient::spawn(
    obs_config,
    pool.clone(),
    ndi_sources.clone(),
    obs_state.clone(),
    obs_event_tx.clone(),
    shutdown_tx.subscribe(),
);
```

(Also keep `ndi_sources` alive in `AppState` so future playlist CRUD can signal a rebuild — see Step 7.)

- [ ] **Step 7: Trigger rebuild on playlist CRUD**

Add an `obs_rebuild_tx: broadcast::Sender<()>` to `AppState`. In `api::routes::create_playlist`, `update_playlist`, `delete_playlist`, call `let _ = state.obs_rebuild_tx.send(());` after a successful DB write.

In `obs::mod.rs::connect_and_run`, select on the rebuild channel inside the main event loop; on a rebuild signal, call `rebuild_ndi_source_map` again and update the map.

Unit test for the rebuild channel wiring (pure signal, no OBS WS required):

```rust
#[tokio::test]
async fn create_playlist_sends_rebuild_signal() {
    let state = test_state().await;
    let mut rebuild_rx = state.obs_rebuild_tx.subscribe();
    let app = app(state);

    app.oneshot(/* POST /api/v1/playlists with a valid body */).await.unwrap();

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), rebuild_rx.recv())
            .await
            .is_ok(),
        "create_playlist should publish a rebuild signal"
    );
}
```

- [ ] **Step 8: Run all tests, confirm GREEN**

Push commit, monitor CI, confirm:
- `scene_change_to_sp_fast_dispatches_play_for_playlist_7` → PASS
- `create_playlist_sends_rebuild_signal` → PASS
- All existing tests still PASS
- No `#[ignore]` or skip added anywhere

- [ ] **Step 9: Commit**

```bash
git add crates/sp-server/src/obs/ndi_discovery.rs \
        crates/sp-server/src/obs/mod.rs \
        crates/sp-server/src/obs/text.rs \
        crates/sp-server/src/lib.rs \
        crates/sp-server/src/api/routes.rs \
        crates/sp-server/tests/scene_detection.rs
git commit -m "fix: populate ndi_sources map from DB + OBS input settings (closes #11)"
```

---

### Task 4: Broadcast NowPlaying / PlaybackStateChanged from the engine (issue #9)

**Files:**
- Modify: `crates/sp-server/src/playback/mod.rs` — `PlaybackEngine::new` takes `event_tx: broadcast::Sender<ServerMsg>`; `handle_pipeline_event` publishes on `Started`/`Position`/`Ended`; `execute_action` publishes on state transitions.
- Modify: `crates/sp-server/src/lib.rs` — pass `event_tx` to `PlaybackEngine::new`.
- Modify: `crates/sp-server/src/db/models.rs` — add `get_video_duration_ms` helper if missing (or reuse `get_video_title_info` pattern).

- [ ] **Step 1: Write failing unit test**

```rust
// in crates/sp-server/src/playback/mod.rs tests module
#[tokio::test]
async fn pipeline_started_event_broadcasts_now_playing() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, duration_ms)
         VALUES (42, 1, 'abc', 'Test Song', 'Test Artist', 180000)",
    )
    .execute(&pool).await.unwrap();

    let (obs_tx, _) = tokio::sync::broadcast::channel::<crate::obs::ObsEvent>(16);
    let (resolume_tx, _) = tokio::sync::mpsc::channel(16);
    let (ws_tx, mut ws_rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);

    let mut engine = PlaybackEngine::new(pool, obs_tx, None, resolume_tx, ws_tx);
    engine.ensure_pipeline(1, "TestNDI");

    // Simulate a video having been selected (so current_video_id is set).
    if let Some(pp) = engine.pipelines.get_mut(&1) {
        pp.current_video_id = Some(42);
    }

    // Dispatch the pipeline Started event.
    engine
        .handle_pipeline_event(
            1,
            pipeline::PipelineEvent::Started { duration_ms: 180000 },
        )
        .await;

    // Expect ServerMsg::NowPlaying on the broadcast channel.
    let msg = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        ws_rx.recv(),
    )
    .await
    .expect("NowPlaying should be broadcast within 500ms")
    .expect("broadcast channel still open");

    match msg {
        sp_core::ws::ServerMsg::NowPlaying {
            playlist_id,
            video_id,
            song,
            artist,
            duration_ms,
            ..
        } => {
            assert_eq!(playlist_id, 1);
            assert_eq!(video_id, 42);
            assert_eq!(song, "Test Song");
            assert_eq!(artist, "Test Artist");
            assert_eq!(duration_ms, 180000);
        }
        other => panic!("expected NowPlaying, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the test, confirm RED**

Push commit, monitor CI, confirm compile error on `PlaybackEngine::new` signature change.

- [ ] **Step 3: Add `event_tx: broadcast::Sender<ServerMsg>` to `PlaybackEngine`**

Modify `crates/sp-server/src/playback/mod.rs`:

```rust
pub struct PlaybackEngine {
    pool: SqlitePool,
    pipelines: HashMap<i64, PlaylistPipeline>,
    event_rx: mpsc::UnboundedReceiver<(i64, PipelineEvent)>,
    event_tx: mpsc::UnboundedSender<(i64, PipelineEvent)>,
    #[cfg(windows)]
    ndi_backend: Option<pipeline::SharedNdiBackend>,
    obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
    #[allow(dead_code)]
    obs_event_tx: broadcast::Sender<ObsEvent>,
    resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
    /// WebSocket broadcast — informs the dashboard of playback state changes.
    ws_event_tx: broadcast::Sender<sp_core::ws::ServerMsg>,
}

impl PlaybackEngine {
    pub fn new(
        pool: SqlitePool,
        obs_event_tx: broadcast::Sender<ObsEvent>,
        obs_cmd_tx: Option<mpsc::Sender<crate::obs::ObsCommand>>,
        resolume_tx: mpsc::Sender<crate::resolume::ResolumeCommand>,
        ws_event_tx: broadcast::Sender<sp_core::ws::ServerMsg>,  // NEW
    ) -> Self {
        // ...
    }
}
```

Add a helper `fn broadcast_now_playing(&self, playlist_id: i64, video_id: i64, duration_ms: u64)` that fetches song+artist from the DB and sends `ServerMsg::NowPlaying`. Call it from the `PipelineEvent::Started` branch of `handle_pipeline_event`.

Also add a helper to send `PlaybackStateChanged` whenever `apply_event` transitions state (emit after `execute_action` runs).

- [ ] **Step 4: Update `lib.rs` to pass the existing `event_tx` to the engine**

```rust
// existing:
let engine = playback::PlaybackEngine::new(
    pool.clone(),
    obs_event_tx.clone(),
    Some(obs_cmd_tx.clone()),
    resolume_tx.clone(),
);
// becomes:
let engine = playback::PlaybackEngine::new(
    pool.clone(),
    obs_event_tx.clone(),
    Some(obs_cmd_tx.clone()),
    resolume_tx.clone(),
    event_tx.clone(),  // same broadcast already used by websocket handler
);
```

- [ ] **Step 5: Add position-update forwarding (throttled)**

On `PipelineEvent::Position { position_ms }`, broadcast `NowPlaying` with the updated position, but **only if ≥500ms has elapsed since the last broadcast for this playlist**. Store `last_position_broadcast: HashMap<i64, Instant>` on `PlaybackEngine`.

- [ ] **Step 6: Add a second test for position throttling**

```rust
#[tokio::test]
async fn position_updates_are_throttled_to_500ms() {
    // Setup engine, seed playlist + video.
    // Send 10 Position events in rapid succession.
    // Assert exactly one NowPlaying broadcast received.
    // Sleep 600ms.
    // Send another Position event.
    // Assert a second NowPlaying broadcast received.
}
```

- [ ] **Step 7: Verify all engine tests still pass**

Push commit, monitor CI. Pay attention to the existing `engine_construction`, `engine_ensure_pipeline_*`, and timer-cancellation tests — they all need to be updated to pass the new `ws_event_tx` parameter.

- [ ] **Step 8: Commit**

```bash
git add crates/sp-server/src/playback/mod.rs crates/sp-server/src/lib.rs
git commit -m "fix: broadcast NowPlaying/PlaybackStateChanged from playback engine (closes #9)"
```

---

### Task 5: Fix `MediaReader::duration_ms` to read presentation descriptor

**Files:**
- Modify: `crates/sp-decoder/src/reader.rs` — `open()` reads `MF_PD_DURATION` from the source's `IMFPresentationDescriptor` and stores it as the initial duration.

- [ ] **Step 1: Write failing decoder test**

```rust
// crates/sp-decoder/tests/duration.rs (new file, Windows-only)
#![cfg(windows)]

use sp_decoder::MediaReader;

#[test]
fn media_reader_reports_nonzero_duration_for_test_mp4() {
    // Use the existing small test mp4 shipped with the repo for decoder tests.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/test_10s.mp4");
    assert!(fixture.exists(), "fixture not found — add one");

    let reader = MediaReader::open(&fixture).expect("open should succeed");
    let duration = reader.duration_ms();

    // The test clip is 10 seconds ± source rounding.
    assert!(
        duration >= 9_000 && duration <= 11_000,
        "expected ~10s duration, got {duration}ms"
    );
}
```

If no fixture exists, generate one at test time with ffmpeg in a `build.rs` or add a tiny committed MP4 (< 100 KB).

- [ ] **Step 2: Run the test, confirm RED**

Push commit, wait for the Windows CI job, confirm the test fails because `duration_ms` returns `0`.

- [ ] **Step 3: Implement duration reading in `open()`**

In `MediaReader::open()` after creating the source reader, retrieve `IMFPresentationDescriptor` via `reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE, &MF_PD_DURATION)` which returns a `PROPVARIANT` containing a `VT_UI8` with the duration in 100-nanosecond units. Convert to milliseconds:

```rust
use windows::Win32::Media::MediaFoundation::{
    MF_PD_DURATION, MF_SOURCE_READER_MEDIASOURCE,
};

let mut duration_ms: u64 = 0;
unsafe {
    if let Ok(pv) = reader.GetPresentationAttribute(
        MF_SOURCE_READER_MEDIASOURCE.0 as u32,
        &MF_PD_DURATION,
    ) {
        // PROPVARIANT UI8 field holds 100-ns ticks.
        let ticks_100ns: u64 = pv.Anonymous.Anonymous.Anonymous.uhVal;
        duration_ms = ticks_100ns / 10_000;
    }
}

// then:
Ok(Self {
    reader,
    duration_ms,           // now nonzero at open time
    // ... other fields
})
```

Mark the function `#[cfg_attr(test, mutants::skip)]` consistent with other `MediaReader` methods.

- [ ] **Step 4: Run the test, confirm GREEN**

Push commit, wait for Windows CI job, confirm the test passes. Verify `cargo fmt --all --check` passes locally.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-decoder/src/reader.rs crates/sp-decoder/tests/duration.rs crates/sp-decoder/tests/fixtures/
git commit -m "fix(sp-decoder): read MF_PD_DURATION at open for accurate duration reporting"
```

---

### Task 6: Fix dashboard Play/Pause/Skip/Prev/Sync/Mode buttons (issue #8)

**Files:**
- Modify: `sp-ui/src/components/playback_controls.rs` — dispatch each `ClientMsg` variant to the correct path-based REST endpoint.
- Modify: `crates/sp-server/src/api/mod.rs` — add `POST /api/v1/playback/{id}/previous` route.
- Modify: `crates/sp-server/src/api/routes.rs` — add `previous()` handler.
- Modify: `crates/sp-server/src/lib.rs` — add `EngineCommand::Previous` variant and route it through `PlaybackEngine`.
- Modify: `crates/sp-server/src/playback/mod.rs` — handle `Previous` (for now, treat as Skip; we can revisit true history later).

- [ ] **Step 1: Write failing Playwright test (runs against real server in CI)**

Add to `e2e/frontend.spec.ts`:

```ts
test("clicking Play dispatches a 2xx backend request (real server)", async ({ page, request }) => {
  // Pre-create a playlist via REST so the UI has something to click.
  const created = await request.post("/api/v1/playlists", {
    data: {
      name: "PlayTestPL",
      youtube_url: "https://youtube.com/playlist?list=PLtest",
      ndi_output_name: "SP-play-test"
    }
  });
  expect(created.status()).toBe(201);
  const pl = await created.json();
  const pid = pl.id as number;

  await page.goto("/");
  await expect(page.locator("text=PlayTestPL")).toBeVisible({ timeout: 10000 });

  // Intercept outgoing network to confirm the correct URL was hit.
  const responsePromise = page.waitForResponse(
    (r) => r.url().includes(`/api/v1/playback/${pid}/play`),
    { timeout: 5000 }
  );

  await page.getByRole("heading", { name: "PlayTestPL" })
    .locator("xpath=ancestor::div[contains(@class,'playlist-card')]")
    .getByRole("button", { name: "Play" })
    .click();

  const resp = await responsePromise;
  expect(resp.status()).toBeGreaterThanOrEqual(200);
  expect(resp.status()).toBeLessThan(300);
});
```

This test must run against a real `sp-server` instance, not the mock. Update `e2e/playwright.config.ts` or add a new project that targets `http://127.0.0.1:8920` and expects the real server to be started before the test (either by CI launching it, or by the Tauri build output running as a desktop app in test mode).

- [ ] **Step 2: Run the test in CI, confirm RED**

Push commit. The test fails because (a) no real server is running in the E2E job, or (b) the button posts to `/api/v1/control` and gets 405. Either failure is proof the test is wired correctly.

- [ ] **Step 3: Add the real-server E2E job to CI**

Modify `.github/workflows/ci.yml` — add a job `e2e-real-server` that:
1. Checks out the repo.
2. Downloads the WASM dist artifact from `build-wasm`.
3. Builds sp-server for Linux (no Windows-only features, no NDI).
4. Launches `sp-server` with an ephemeral SQLite DB on port 8920.
5. Runs `cd e2e && npx playwright test frontend.spec.ts`.
6. Uploads traces + screenshots on failure.

Do NOT mark this job `continue-on-error`. Do NOT skip it on any condition.

- [ ] **Step 4: Rewrite `playback_controls.rs::send_cmd`**

```rust
let send_cmd = move |msg: ClientMsg| {
    let pid_str = pid.to_string();
    leptos::task::spawn_local(async move {
        match msg {
            ClientMsg::Play { .. } => {
                let _ = api::post_empty(&format!("/api/v1/playback/{pid_str}/play")).await;
            }
            ClientMsg::Pause { .. } => {
                let _ = api::post_empty(&format!("/api/v1/playback/{pid_str}/pause")).await;
            }
            ClientMsg::Skip { .. } => {
                let _ = api::post_empty(&format!("/api/v1/playback/{pid_str}/skip")).await;
            }
            ClientMsg::Previous { .. } => {
                let _ = api::post_empty(&format!("/api/v1/playback/{pid_str}/previous")).await;
            }
            ClientMsg::SetMode { mode, .. } => {
                #[derive(serde::Serialize)]
                struct Body { mode: String }
                let _ = api::put_json::<Body, serde_json::Value>(
                    &format!("/api/v1/playback/{pid_str}/mode"),
                    &Body { mode: mode.as_str().to_string() },
                ).await;
            }
            ClientMsg::SyncPlaylist { .. } => {
                let _ = api::post_empty(&format!("/api/v1/playlists/{pid_str}/sync")).await;
            }
            ClientMsg::Ping => {}
        }
    });
};
```

Add `post_empty` helper to `sp-ui/src/api.rs` if it does not exist (a POST with no body that returns `()` on 2xx).

- [ ] **Step 5: Add `POST /api/v1/playback/{id}/previous` endpoint**

In `api/mod.rs`:
```rust
.route(
    "/api/v1/playback/{playlist_id}/previous",
    axum::routing::post(routes::previous),
)
```

In `api/routes.rs`:
```rust
pub async fn previous(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let _ = state
        .engine_tx
        .send(EngineCommand::Previous { playlist_id })
        .await;
    StatusCode::NO_CONTENT
}
```

In `lib.rs`, add `EngineCommand::Previous { playlist_id: i64 }` variant and dispatch it through `PlaybackEngine::handle_command` — for now, treat it the same as `Skip` (emit `PlayEvent::Skip`). Add a TODO to implement real history later.

Add a unit test:
```rust
#[tokio::test]
async fn previous_endpoint_returns_204() {
    let state = test_state().await;
    let app = app(state);
    let resp = app.oneshot(
        Request::builder().method("POST")
            .uri("/api/v1/playback/1/previous")
            .body(Body::empty()).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}
```

- [ ] **Step 6: Run all tests, confirm GREEN**

Push commit. CI must show:
- New Playwright test PASSES
- New `previous_endpoint_returns_204` PASSES
- All existing tests PASS
- Zero console errors from Playwright
- No skipped tests

- [ ] **Step 7: Commit**

```bash
git add sp-ui/src/components/playback_controls.rs \
        sp-ui/src/api.rs \
        crates/sp-server/src/api/mod.rs \
        crates/sp-server/src/api/routes.rs \
        crates/sp-server/src/lib.rs \
        crates/sp-server/src/playback/mod.rs \
        e2e/frontend.spec.ts \
        .github/workflows/ci.yml
git commit -m "fix: dashboard buttons use path-based REST; add Previous endpoint (closes #8)"
```

---

### Task 7: Add Gemini rate-limit cooldown to the reprocess worker (issue #12)

**Files:**
- Modify: `crates/sp-server/src/reprocess/mod.rs` — track per-video `next_retry_at` with exponential backoff and a global `gemini_cooldown_until` timestamp.

- [ ] **Step 1: Write failing unit test**

```rust
#[tokio::test(start_paused = true)]
async fn gemini_rate_limit_triggers_global_cooldown() {
    let pool = setup().await;
    let tmp = tempfile::tempdir().unwrap();

    // Seed two videos both needing reprocessing.
    let gf = "foo_bar_abc1234567_normalized_gf.mp4";
    tokio::fs::write(tmp.path().join(gf), b"x").await.unwrap();
    insert_gf_video(&pool, "abc1234567", tmp.path().join(gf).to_str().unwrap()).await;
    let gf2 = "foo_bar_xyz7654321_normalized_gf.mp4";
    tokio::fs::write(tmp.path().join(gf2), b"x").await.unwrap();
    insert_gf_video(&pool, "xyz7654321", tmp.path().join(gf2).to_str().unwrap()).await;

    let providers: Arc<Vec<Box<dyn MetadataProvider>>> = Arc::new(vec![Box::new(RateLimitProvider)]);
    let mut worker = ReprocessWorker::new(pool.clone(), providers, tmp.path().to_path_buf());

    // First call: hits rate limit on the first video, aborts batch.
    let attempts = worker.process_all_with_cooldown_check().await.unwrap();
    assert_eq!(attempts, 1, "first rate-limit must abort the batch after video 1");

    // Immediately retrying — still in cooldown, zero attempts.
    let attempts_during_cooldown = worker.process_all_with_cooldown_check().await.unwrap();
    assert_eq!(attempts_during_cooldown, 0, "cooldown must block further attempts");

    // Advance virtual time by 6 minutes.
    tokio::time::advance(std::time::Duration::from_secs(6 * 60)).await;

    // Cooldown expired — worker attempts again.
    let attempts_after = worker.process_all_with_cooldown_check().await.unwrap();
    assert_eq!(attempts_after, 1, "after cooldown, batch should resume and attempt first video");
}

struct RateLimitProvider;
#[async_trait::async_trait]
impl MetadataProvider for RateLimitProvider {
    async fn extract(&self, _: &str, _: &str) -> Result<VideoMetadata, MetadataError> {
        Err(MetadataError::RateLimited)
    }
    fn name(&self) -> &str { "rate-limited" }
}
```

Assumes `MetadataError::RateLimited` exists; add it if not and propagate from `gemini.rs`.

- [ ] **Step 2: Run the test, confirm RED**

Push commit, monitor CI, confirm the test compiles but fails because no cooldown exists.

- [ ] **Step 3: Implement cooldown state**

```rust
pub struct ReprocessWorker {
    pool: SqlitePool,
    providers: Arc<Vec<Box<dyn MetadataProvider>>>,
    cache_dir: PathBuf,
    gemini_cooldown_until: Option<std::time::Instant>,
    per_video_backoff: std::collections::HashMap<i64, std::time::Instant>,
}

impl ReprocessWorker {
    const COOLDOWN_DURATION: std::time::Duration = std::time::Duration::from_secs(5 * 60);
    const BACKOFF_STAGES: &'static [std::time::Duration] = &[
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(5 * 60),
        std::time::Duration::from_secs(15 * 60),
        std::time::Duration::from_secs(60 * 60),
        std::time::Duration::from_secs(6 * 60 * 60),
        std::time::Duration::from_secs(24 * 60 * 60),
    ];

    fn in_cooldown(&self) -> bool {
        self.gemini_cooldown_until
            .map(|t| std::time::Instant::now() < t)
            .unwrap_or(false)
    }

    async fn reprocess_one(&mut self, row: &ReprocessRow) -> Result<bool, anyhow::Error> {
        if self.in_cooldown() {
            return Ok(false);
        }
        if let Some(retry_at) = self.per_video_backoff.get(&row.id) {
            if std::time::Instant::now() < *retry_at {
                return Ok(false);
            }
        }

        match crate::metadata::get_metadata(&self.providers, &row.youtube_id, &row.title).await {
            Ok(meta) if !meta.gemini_failed => {
                self.per_video_backoff.remove(&row.id);
                // ... existing rename + DB update ...
                Ok(true)
            }
            Err(MetadataError::RateLimited) | Ok(_) => {
                // Trip global cooldown and per-video backoff.
                self.gemini_cooldown_until =
                    Some(std::time::Instant::now() + Self::COOLDOWN_DURATION);
                let current_idx = /* look up current backoff stage for this video */ 0;
                let next = Self::BACKOFF_STAGES
                    .get(current_idx + 1)
                    .copied()
                    .unwrap_or_else(|| *Self::BACKOFF_STAGES.last().unwrap());
                self.per_video_backoff
                    .insert(row.id, std::time::Instant::now() + next);
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }
}
```

Requires extending the state to track per-video backoff stage (store a `HashMap<i64, usize>` for stage index).

- [ ] **Step 4: Run the test, confirm GREEN**

Push commit, monitor CI, confirm:
- `gemini_rate_limit_triggers_global_cooldown` PASSES
- Existing reprocess tests still PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sp-server/src/reprocess/mod.rs crates/sp-server/src/metadata/mod.rs
git commit -m "fix: add Gemini rate-limit cooldown + per-video backoff (closes #12)"
```

---

## Phase 3: Post-deploy E2E that actually tests the features

### Task 8: Real post-deploy Playwright that drives OBS scenes

**Why:** The existing `E2E Tests (win-resolume)` job only opens the dashboard. It does not switch OBS scenes, does not click the Play button, and never verified that any of the four bugs above would have worked in production. This must be fixed as part of this PR.

**Files:**
- Modify: `.github/workflows/ci.yml` — replace the `E2E Tests (win-resolume)` job's script with a real Playwright suite that uses `obs-websocket-js` to drive OBS.
- Create: `e2e/post-deploy.spec.ts` — Playwright test suite for the deployed stack.
- Create: `e2e/obs-driver.ts` — small wrapper around `obs-websocket-js` for scene switching.

- [ ] **Step 1: Write the failing post-deploy spec**

```ts
// e2e/post-deploy.spec.ts
import { test, expect } from "@playwright/test";
import { ObsDriver } from "./obs-driver";

const DASHBOARD = process.env.SONGPLAYER_URL || "http://10.77.9.201:8920";
const OBS_URL = process.env.OBS_WS_URL || "ws://10.77.9.201:4455";

test.describe("SongPlayer post-deploy real-OBS feature verification", () => {
  let obs: ObsDriver;

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_URL);
  });

  test.afterAll(async () => {
    await obs?.disconnect();
  });

  test("scene switch to sp-fast triggers playback within 3s", async ({ page }) => {
    await obs.switchScene("Break"); // a non-sp scene, to reset
    await page.goto(DASHBOARD);

    // Wait for WS to connect.
    await expect(page.locator(".status-indicator.ws-connected")).toBeVisible({ timeout: 5000 });

    // Switch OBS to sp-fast.
    await obs.switchScene("sp-fast");

    // Within 3s, the ytfast playlist card should transition from "Nothing playing"
    // to a song title + elapsed time.
    const fastCard = page.locator("[data-playlist-id='7']");
    await expect(fastCard.locator(".np-song")).toBeVisible({ timeout: 3000 });
    const songText = await fastCard.locator(".np-song").textContent();
    expect(songText?.trim().length).toBeGreaterThan(0);

    // The status line should read "Playing".
    await expect(fastCard.locator(".np-time")).toContainText("Playing");
  });

  test("scene switch away from sp-fast stops playback within 3s", async ({ page }) => {
    await obs.switchScene("sp-fast");
    await page.goto(DASHBOARD);
    const fastCard = page.locator("[data-playlist-id='7']");
    await expect(fastCard.locator(".np-song")).toBeVisible({ timeout: 5000 });

    await obs.switchScene("Break");

    // Card should return to "Nothing playing" within 3s.
    await expect(fastCard.locator(".np-idle")).toBeVisible({ timeout: 3000 });
  });

  test("clicking Play button on a non-active playlist starts playback", async ({ page }) => {
    await obs.switchScene("Break");
    await page.goto(DASHBOARD);

    const warmupCard = page.locator("[data-playlist-id='2']");
    await warmupCard.getByRole("button", { name: "Play" }).click();

    // Dashboard updates with NowPlaying within 2s.
    await expect(warmupCard.locator(".np-song")).toBeVisible({ timeout: 2000 });
  });

  test("zero console errors throughout the suite", async ({ page }) => {
    const errors: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error" || m.type() === "warning") errors.push(m.text());
    });

    await page.goto(DASHBOARD);
    await page.waitForTimeout(3000);

    const allowed = [/favicon/, /WebSocket connection/];
    const real = errors.filter((e) => !allowed.some((r) => r.test(e)));
    expect(real).toEqual([]);
  });
});
```

- [ ] **Step 2: Write the OBS driver wrapper**

```ts
// e2e/obs-driver.ts
import OBSWebSocket from "obs-websocket-js";

export class ObsDriver {
  private constructor(private obs: OBSWebSocket) {}

  static async connect(url: string, password?: string): Promise<ObsDriver> {
    const obs = new OBSWebSocket();
    await obs.connect(url, password);
    return new ObsDriver(obs);
  }

  async switchScene(sceneName: string): Promise<void> {
    await this.obs.call("SetCurrentProgramScene", { sceneName });
    // Let OBS + SongPlayer propagate the change.
    await new Promise((r) => setTimeout(r, 250));
  }

  async disconnect(): Promise<void> {
    await this.obs.disconnect();
  }
}
```

Add `obs-websocket-js` to `e2e/package.json`.

- [ ] **Step 3: Add `data-playlist-id` attribute to `PlaylistCard`**

In `sp-ui/src/components/playlist_card.rs`, add `data-playlist-id={pid}` to the root div so Playwright can select cards by playlist ID.

- [ ] **Step 4: Update the CI `E2E Tests (win-resolume)` job**

Replace the current script with:

```yaml
  e2e-post-deploy:
    name: E2E Tests (win-resolume real OBS)
    needs: deploy-win-resolume
    runs-on: [self-hosted, Windows, X64, resolume]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
      - name: Install Playwright
        run: cd e2e && npm ci && npx playwright install chromium
      - name: Run real-OBS post-deploy tests
        env:
          SONGPLAYER_URL: http://127.0.0.1:8920
          OBS_WS_URL: ws://127.0.0.1:4455
        run: cd e2e && npx playwright test post-deploy.spec.ts
      - name: Upload traces on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: playwright-traces
          path: e2e/test-results/
```

Do NOT mark this job `continue-on-error`. Do NOT add any `if: always()` skips.

- [ ] **Step 5: Commit and push, verify CI runs the new suite**

```bash
git add e2e/post-deploy.spec.ts e2e/obs-driver.ts e2e/package.json e2e/package-lock.json \
        sp-ui/src/components/playlist_card.rs \
        .github/workflows/ci.yml
git commit -m "test(e2e): post-deploy Playwright suite drives real OBS + asserts dashboard state"
```

---

## Phase 4: Release

### Task 9: Final verification and PR

- [ ] **Step 1: Bump VERSION to release value**

Change `VERSION` from `0.9.0-dev.2` to `0.9.0`. Run `./scripts/sync-version.sh`. Run `cargo update --workspace --offline`. Run `cargo fmt --all --check`.

- [ ] **Step 2: Commit release bump**

```bash
git add VERSION Cargo.lock Cargo.toml crates/*/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json sp-ui/Cargo.toml
git commit -m "chore: set version to 0.9.0 for release"
```

- [ ] **Step 3: Push dev and wait for CI to complete green**

```bash
git push origin dev
```

Monitor with `gh run view`. ALL jobs must be green — especially `e2e-post-deploy` (real-OBS). Do NOT proceed until every job is a green checkmark.

- [ ] **Step 4: Open PR dev → main**

```bash
gh pr create --base main --head dev --title "Fix all SongPlayer wiring bugs (#8, #9, #11, #12) + add feature-level E2E" --body "$(cat <<'EOF'
## Summary

Fixes every bug that prevents SongPlayer from working end-to-end in production, plus adds the integration and post-deploy E2E tests that would have caught each bug under green CI.

- Closes #8 (dashboard Play/Pause/Skip/Prev/Sync/Mode buttons no longer return 405)
- Closes #9 (dashboard now-playing row updates from ServerMsg::NowPlaying broadcast)
- Closes #11 (scene detection populates ndi_sources from DB + OBS input settings)
- Closes #12 (reprocess worker honors Gemini rate-limit cooldown)
- Fixes latent duration_ms=0 bug in sp-decoder by reading MF_PD_DURATION at open
- Adds post-deploy Playwright that drives real OBS scene switches and asserts dashboard/engine state

## Test plan

- [x] `cargo fmt --all --check` clean
- [x] Unit + integration tests green (scene_detection, NowPlaying broadcast, Gemini cooldown, duration, previous endpoint)
- [x] Frontend Playwright against real server green (Play button hits /api/v1/playback/{id}/play, not /api/v1/control)
- [x] Post-deploy real-OBS Playwright green (scene switch triggers playback, dashboard card updates, zero console errors)
- [x] Version 0.9.0 (no -dev)
- [x] No skip/ignore/continue-on-error added anywhere

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Verify PR is mergeable**

```bash
gh api repos/zbynekdrlik/songplayer/pulls/$(gh pr view --json number -q .number) --jq '{mergeable, mergeable_state}'
```

Must report `{mergeable: true, mergeable_state: "clean"}`. If "behind", sync. If "dirty", resolve.

- [ ] **Step 6: Report PR URL to user and WAIT for explicit merge instruction**

Do NOT merge. Do NOT "go ahead and merge" based on CI being green. Wait for user to say "merge" or equivalent.

---

## Verification

After the user merges and CI completes on `main`:

1. **Main CI:** all jobs green including `e2e-post-deploy`.
2. **Deploy:** new 0.9.0 installer deployed to win-resolume via CI.
3. **Live feature check:** Claude opens the dashboard at `http://10.77.9.201:8920`, switches OBS to `sp-fast` via `obs-resolume` MCP, watches the ytfast card transition from "Nothing playing" to a song title within 3 seconds, clicks the Play button on ytwarmup, watches warmup transition, switches OBS away from all sp-* scenes, watches all cards return to "Nothing playing". No console errors.
4. **User confirms audio reaches OBS for sp-fast via their physical monitor.**

If any of these fail, the PR is not done — revert or follow-up.
