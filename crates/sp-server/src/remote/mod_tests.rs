//! #213 `remote/mod.rs`: settings, telemetry, the link to cg OBS and the
//! settings task's listener lifecycle (bound over REAL ports; every wait is
//! bounded). Wired via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::db::models::set_setting;
use crate::obs::remote_call::RemoteCall;
use crate::obs::{ObsCommand, ObsEvent};
use crate::playback::program_bus::ProgramBus;

const TIMEOUT: Duration = Duration::from_secs(10);
/// The obs-websocket spec's example password (the only password in tests).
const SPEC_PASSWORD: &str = "supersecretpassword";

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

fn settings(enabled: bool, port: u16, password: Option<&str>) -> RemoteSettings {
    RemoteSettings {
        enabled,
        port,
        password: password.map(str::to_string),
    }
}

// ---- settings --------------------------------------------------------------

#[test]
fn parse_port_takes_a_non_zero_u16_else_the_default() {
    assert_eq!(parse_port(Some("4460")), 4460);
    assert_eq!(parse_port(Some(" 4461 ")), 4461);
    assert_eq!(parse_port(Some("1")), 1);
    assert_eq!(parse_port(Some("65535")), 65535);
    assert_eq!(parse_port(None), 4456);
    assert_eq!(parse_port(Some("")), 4456);
    assert_eq!(parse_port(Some("0")), 4456);
    assert_eq!(parse_port(Some("65536")), 4456);
    assert_eq!(parse_port(Some("abc")), 4456);
}

#[tokio::test]
async fn load_remote_settings_defaults_to_off_on_4456_without_a_password() {
    let pool = pool().await;
    assert_eq!(
        load_remote_settings(&pool).await.unwrap(),
        settings(false, 4456, None)
    );
    assert_eq!(RemoteSettings::disabled(), settings(false, 4456, None));
}

#[tokio::test]
async fn load_remote_settings_reads_the_stored_keys() {
    let pool = pool().await;
    set_setting(&pool, "remote_ws_enabled", "true")
        .await
        .unwrap();
    set_setting(&pool, "remote_ws_port", "4460").await.unwrap();
    set_setting(&pool, "remote_ws_password", SPEC_PASSWORD)
        .await
        .unwrap();
    assert_eq!(
        load_remote_settings(&pool).await.unwrap(),
        settings(true, 4460, Some(SPEC_PASSWORD))
    );
    // Only "true" enables; a blank password means no auth.
    set_setting(&pool, "remote_ws_enabled", "yes")
        .await
        .unwrap();
    set_setting(&pool, "remote_ws_password", "   ")
        .await
        .unwrap();
    assert_eq!(
        load_remote_settings(&pool).await.unwrap(),
        settings(false, 4460, None)
    );
}

#[test]
fn the_listener_plan() {
    let on = settings(true, 4456, None);
    let off = settings(false, 4456, None);
    let other_port = settings(true, 4457, None);
    let with_password = settings(true, 4456, Some(SPEC_PASSWORD));
    // Nothing bound.
    assert_eq!(listener_plan(None, &off), ListenerPlan::Keep);
    assert_eq!(listener_plan(None, &on), ListenerPlan::Start);
    // Bound with the wanted settings.
    assert_eq!(listener_plan(Some(&on), &on), ListenerPlan::Keep);
    // A change rebinds; disabling stops.
    assert_eq!(listener_plan(Some(&on), &other_port), ListenerPlan::Start);
    assert_eq!(
        listener_plan(Some(&on), &with_password),
        ListenerPlan::Start
    );
    assert_eq!(listener_plan(Some(&on), &off), ListenerPlan::Stop);
}

#[test]
fn only_a_new_error_is_logged() {
    assert!(is_new_error(None, Some("taken")));
    assert!(is_new_error(Some("a"), Some("b")));
    assert!(!is_new_error(Some("taken"), Some("taken")));
    assert!(!is_new_error(Some("taken"), None));
    assert!(!is_new_error(None, None));
}

// ---- telemetry -------------------------------------------------------------

#[test]
fn the_status_reports_the_stored_settings_and_the_live_counters() {
    let shared = Arc::new(RemoteShared::default());
    let st = shared.status(&settings(true, 4460, Some(SPEC_PASSWORD)));
    assert!(st.enabled);
    assert_eq!(st.port, 4460);
    assert!(st.auth);
    assert!(!st.listening);
    assert_eq!(st.error, None);
    assert_eq!(st.clients, 0);
    assert_eq!(st.requests, 0);
    assert_eq!(st.last_request, None);
    assert_eq!(st.last_remote_cut, None);
    assert!(st.unsupported_requests.is_empty());
    assert!(!shared.status(&settings(false, 4456, None)).auth);

    let a = shared.client_connected();
    let b = shared.client_connected();
    assert_eq!(shared.status(&RemoteSettings::disabled()).clients, 2);
    drop(a);
    assert_eq!(shared.status(&RemoteSettings::disabled()).clients, 1);
    drop(b);
    assert_eq!(shared.status(&RemoteSettings::disabled()).clients, 0);

    shared.record_request("GetVersion");
    shared.record_request("GetSceneList");
    assert!(shared.note_unsupported("GetStats"));
    assert!(!shared.note_unsupported("GetStats"));
    assert!(shared.note_unsupported("GetHotkeyList"));
    shared.set_listening(true);
    assert!(shared.set_error(Some("taken".to_string())));
    assert!(!shared.set_error(Some("taken".to_string())));
    let st = shared.status(&RemoteSettings::disabled());
    assert_eq!(st.requests, 2);
    let last = st.last_request.unwrap();
    assert_eq!(last.request_type, "GetSceneList");
    assert!(last.at_ms > 1_700_000_000_000, "{}", last.at_ms);
    assert_eq!(st.unsupported_requests, vec!["GetHotkeyList", "GetStats"]);
    assert!(st.listening);
    assert_eq!(st.error.as_deref(), Some("taken"));
    assert!(!shared.set_error(None));
    assert_eq!(shared.status(&RemoteSettings::disabled()).error, None);

    let cut = RemoteCut {
        scene: "sp-fast".to_string(),
        action: "playlist",
        source: Some(7),
        reason: None,
        cut_boundary_100ns: Some(42),
        at_ms: 1,
    };
    shared.record_cut(cut.clone());
    assert_eq!(
        shared.status(&RemoteSettings::disabled()).last_remote_cut,
        Some(cut)
    );
}

#[test]
fn the_status_serializes_the_api_field_names() {
    let shared = RemoteShared::default();
    let v = serde_json::to_value(shared.status(&settings(true, 4456, None))).unwrap();
    assert_eq!(
        v,
        json!({
            "enabled": true,
            "port": 4456,
            "auth": false,
            "listening": false,
            "error": null,
            "clients": 0,
            "requests": 0,
            "last_request": null,
            "last_remote_cut": null,
            "unsupported_requests": [],
        })
    );
}

// ---- the link to cg OBS ----------------------------------------------------

#[tokio::test]
async fn without_an_obs_client_every_call_is_none() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let up = Upstream::new(None, events);
    assert_eq!(up.request("GetSceneList", None).await, None);
    assert_eq!(up.scene_playlists("sp-fast").await, None);
}

#[tokio::test]
async fn a_call_is_answered_through_the_obs_command_channel() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let (tx, mut rx) = mpsc::channel::<ObsCommand>(4);
    let up = Upstream::new(Some(tx), events);
    tokio::spawn(async move {
        while let Some(ObsCommand::Remote(call)) = rx.recv().await {
            match call {
                RemoteCall::Request {
                    request_type,
                    request_data,
                    reply,
                } => {
                    let _ = reply.send(Some(json!({
                        "echo": request_type,
                        "data": request_data,
                    })));
                }
                RemoteCall::ScenePlaylists { scene, reply } => {
                    let ids: HashSet<i64> = if scene == "sp-fast" {
                        [7].into()
                    } else {
                        HashSet::new()
                    };
                    let _ = reply.send(ids);
                }
            }
        }
    });
    let d = up
        .request("GetSceneItemList", Some(json!({ "sceneName": "x" })))
        .await;
    assert_eq!(
        d,
        Some(json!({ "echo": "GetSceneItemList", "data": { "sceneName": "x" } }))
    );
    assert_eq!(
        up.scene_playlists("sp-fast").await,
        Some(HashSet::from([7]))
    );
    assert_eq!(up.scene_playlists("Slido").await, Some(HashSet::new()));
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_call_times_out_and_is_left_marked_abandoned() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let (tx, mut rx) = mpsc::channel::<ObsCommand>(4);
    let up = Upstream::new(Some(tx), events);
    let started = tokio::time::Instant::now();
    assert_eq!(up.request("SetCurrentProgramScene", None).await, None);
    assert_eq!(started.elapsed(), UPSTREAM_TIMEOUT);
    match rx.try_recv() {
        Ok(ObsCommand::Remote(RemoteCall::Request {
            request_type,
            reply,
            ..
        })) => {
            assert_eq!(request_type, "SetCurrentProgramScene");
            // The OBS side skips it: a late switch never happens.
            assert!(reply.is_closed());
        }
        other => panic!("expected the queued request, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_full_obs_queue_is_not_ready_at_once_never_a_blocked_caller() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let (tx, _rx) = mpsc::channel::<ObsCommand>(1);
    let up = Upstream::new(Some(tx), events);
    let (reply, _keep) = tokio::sync::oneshot::channel();
    up.cmd_tx
        .as_ref()
        .unwrap()
        .try_send(ObsCommand::Remote(RemoteCall::ScenePlaylists {
            scene: "filler".to_string(),
            reply,
        }))
        .unwrap();
    let started = tokio::time::Instant::now();
    assert_eq!(up.request("GetSceneList", None).await, None);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

// ---- the settings task -----------------------------------------------------

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The Hello a fresh client on `port` gets, `None` when nothing listens.
async fn hello_on(port: u16) -> Option<Value> {
    let url = format!("ws://127.0.0.1:{port}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.ok()?;
    let msg = tokio::time::timeout(TIMEOUT, ws.next()).await.ok()??.ok()?;
    match msg {
        Message::Text(text) => serde_json::from_str(&text).ok(),
        _ => None,
    }
}

async fn wait_for_hello(port: u16, what: &str, want_auth: bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(hello) = hello_on(port).await
            && hello["d"].get("authentication").is_some() == want_auth
        {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn the_settings_task_binds_rebinds_retries_and_stops_the_listener() {
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
    let listening = || bus.remote().status(&RemoteSettings::disabled()).listening;
    let error = || bus.remote().status(&RemoteSettings::disabled()).error;

    let port = free_port();
    set_setting(&pool, "remote_ws_port", &port.to_string())
        .await
        .unwrap();
    set_setting(&pool, "remote_ws_enabled", "true")
        .await
        .unwrap();
    let task = tokio::spawn(run_remote_config_task(
        pool.clone(),
        Arc::clone(&bus),
        Upstream::new(None, events),
        shutdown_rx,
        Duration::from_millis(20),
    ));
    wait_for("the listener binds", listening).await;
    wait_for_hello(port, "a Hello without auth", false).await;

    // A password change on the SAME port: the old listener must let the port
    // go before the rebind, and new clients get the challenge.
    set_setting(&pool, "remote_ws_password", SPEC_PASSWORD)
        .await
        .unwrap();
    wait_for_hello(port, "a Hello with auth", true).await;
    assert!(listening());
    assert_eq!(error(), None);

    // Disabled: nothing listens any more.
    set_setting(&pool, "remote_ws_enabled", "false")
        .await
        .unwrap();
    wait_for("the listener stops", || !listening()).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while hello_on(port).await.is_some() {
        assert!(tokio::time::Instant::now() < deadline, "still served");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A taken port: the error shows and the bind is retried until it is free.
    let taken = free_port();
    let blocker = std::net::TcpListener::bind(("0.0.0.0", taken)).unwrap();
    set_setting(&pool, "remote_ws_port", &taken.to_string())
        .await
        .unwrap();
    set_setting(&pool, "remote_ws_enabled", "true")
        .await
        .unwrap();
    wait_for("the bind error shows", || {
        error().is_some_and(|e| e.starts_with(&format!("binding port {taken} failed")))
    })
    .await;
    assert!(!listening());
    drop(blocker);
    wait_for("the retried bind succeeds", listening).await;
    assert_eq!(error(), None);
    wait_for_hello(taken, "served on the freed port", true).await;

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("the settings task stops on shutdown")
        .unwrap();
    assert!(!listening());
}
