//! #213 `remote/mod.rs`: settings, telemetry, the link to cg OBS and the
//! settings task's listener lifecycle (bound over REAL ports; every wait is
//! bounded). Wired via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`.

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
use crate::playback::program_on_air::OnAir;

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
    let st = shared.status(
        &settings(true, 4460, Some(SPEC_PASSWORD)),
        &OnAir::default(),
    );
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
    assert!(
        !shared
            .status(&settings(false, 4456, None), &OnAir::default())
            .auth
    );

    let a = shared.client_connected();
    let b = shared.client_connected();
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .clients,
        2
    );
    drop(a);
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .clients,
        1
    );
    drop(b);
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .clients,
        0
    );

    shared.record_request("GetVersion");
    shared.record_request("GetSceneList");
    assert!(shared.note_unsupported("GetStats"));
    assert!(!shared.note_unsupported("GetStats"));
    assert!(shared.note_unsupported("GetHotkeyList"));
    shared.set_listening(true);
    assert!(shared.set_error(Some("taken".to_string())));
    assert!(!shared.set_error(Some("taken".to_string())));
    let st = shared.status(&RemoteSettings::disabled(), &OnAir::default());
    assert_eq!(st.requests, 2);
    let last = st.last_request.unwrap();
    assert_eq!(last.request_type, "GetSceneList");
    assert!(last.at_ms > 1_700_000_000_000, "{}", last.at_ms);
    assert_eq!(st.unsupported_requests, vec!["GetHotkeyList", "GetStats"]);
    assert!(st.listening);
    assert_eq!(st.error.as_deref(), Some("taken"));
    assert!(!shared.set_error(None));
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .error,
        None
    );

    let cut = RemoteCut {
        scene: "sp-fast".to_string(),
        action: "playlist",
        source: Some(7),
        reason: None,
        cut_boundary_100ns: Some(42),
        at_ms: 1,
        via: Some("transition"),
        cg_forward: Some("pending".to_string()),
    };
    shared.record_cut(cut.clone());
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .last_remote_cut,
        Some(cut)
    );
}

fn cut_of(scene: &str) -> RemoteCut {
    RemoteCut {
        scene: scene.to_string(),
        action: "playlist",
        source: Some(7),
        reason: None,
        cut_boundary_100ns: None,
        at_ms: 1,
        via: Some("program"),
        cg_forward: Some("pending".to_string()),
    }
}

/// #221: the mirror's answer lands after the press was answered; it may only
/// update ITS cut, never a later press's.
#[test]
fn a_late_mirror_answer_updates_only_its_own_cut() {
    let shared = RemoteShared::default();
    let last_forward = |shared: &RemoteShared| {
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .last_remote_cut
            .and_then(|c| c.cg_forward)
    };
    assert!(
        !shared.set_cg_forward(0, "ok".to_string()),
        "no cut recorded yet"
    );
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .last_remote_cut,
        None
    );
    let first = shared.record_cut(cut_of("sp-fast"));
    assert!(shared.set_cg_forward(first, "error 600".to_string()));
    assert_eq!(last_forward(&shared).as_deref(), Some("error 600"));
    let second = shared.record_cut(cut_of("sp-slow"));
    assert_ne!(first, second);
    assert!(
        !shared.set_cg_forward(first, "ok".to_string()),
        "a later press replaced it"
    );
    assert_eq!(last_forward(&shared).as_deref(), Some("pending"));
    assert!(shared.set_cg_forward(second, "not_ready".to_string()));
    let last = shared
        .status(&RemoteSettings::disabled(), &OnAir::default())
        .last_remote_cut
        .unwrap();
    assert_eq!(
        (last.scene.as_str(), last.cg_forward.as_deref()),
        ("sp-slow", Some("not_ready"))
    );
}

/// #221: a transition duration is kept for the telemetry, never applied.
#[test]
fn a_transition_duration_is_recorded_as_not_applied() {
    let shared = RemoteShared::default();
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .last_transition_duration,
        None
    );
    shared.record_transition_duration(2000);
    shared.record_transition_duration(750);
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .last_transition_duration,
        Some(TransitionDuration {
            ms: 750,
            applied: false
        })
    );
}

#[test]
fn debug_never_prints_the_password() {
    let shown = format!("{:?}", settings(true, 4460, Some(SPEC_PASSWORD)));
    assert!(!shown.contains(SPEC_PASSWORD), "{shown}");
    assert_eq!(
        shown,
        r#"RemoteSettings { enabled: true, port: 4460, password: Some("<set>") }"#
    );
    assert_eq!(
        format!("{:?}", settings(false, 4456, None)),
        "RemoteSettings { enabled: false, port: 4456, password: None }"
    );
}

#[test]
fn client_chosen_request_types_stay_bounded() {
    let shared = RemoteShared::default();
    let long = "G".repeat(100);
    assert!(shared.note_unsupported(&long));
    shared.record_request(&long);
    let st = shared.status(&RemoteSettings::disabled(), &OnAir::default());
    assert_eq!(st.unsupported_requests, vec!["G".repeat(64)]);
    assert_eq!(st.last_request.unwrap().request_type, "G".repeat(64));
    for i in 1..64 {
        assert!(shared.note_unsupported(&format!("R{i:02}")), "{i}");
    }
    assert_eq!(
        shared
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .unsupported_requests
            .len(),
        64
    );
    // Full: a 65th type is neither stored nor logged.
    assert!(!shared.note_unsupported("R99"));
    let listed = shared
        .status(&RemoteSettings::disabled(), &OnAir::default())
        .unsupported_requests;
    assert_eq!(listed.len(), 64);
    assert!(!listed.contains(&"R99".to_string()));
}

#[test]
fn the_status_serializes_the_api_field_names() {
    let shared = RemoteShared::default();
    let v = serde_json::to_value(shared.status(&settings(true, 4456, None), &OnAir::default()))
        .unwrap();
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
            "last_transition_duration": null,
            "program_scene": null,
        })
    );
}

/// #221 review round 3: a production facade waits for a switch's
/// `SceneTransitionEnded` at most the production bound (only `for_test` sets
/// a longer one).
#[tokio::test]
async fn a_production_facade_bounds_the_ended_wait_by_the_production_value() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let facade = Facade::new(
        pool().await,
        Arc::new(ProgramBus::new()),
        Upstream::new(None, events),
        None,
    );
    assert_eq!(
        facade.transition_end_max,
        studio_events::TRANSITION_END_MAX_WAIT
    );
}

/// #221 L3: `program_scene` names what SP-program has on air with the one
/// resolver: the scene it was cut for, "OBS manuál" for the NDI input with
/// no scene, `null` while nothing is on air.
#[test]
fn the_status_names_the_program_scene_with_the_resolver() {
    let shared = RemoteShared::default();
    let settings = RemoteSettings::disabled();
    let on_air = |source: i64, scene: Option<&str>| OnAir::default().next(source, scene);
    let named = |on_air: &OnAir| shared.status(&settings, on_air).program_scene;
    assert_eq!(named(&OnAir::default()), None);
    assert_eq!(
        named(&on_air(7, Some("sp-fast"))).as_deref(),
        Some("sp-fast")
    );
    assert_eq!(named(&on_air(-1, Some("Slido"))).as_deref(), Some("Slido"));
    assert_eq!(named(&on_air(-1, None)).as_deref(), Some("OBS manuál"));
    assert_eq!(
        named(&on_air(7, None)),
        None,
        "a playlist with no catalog scene"
    );
}

// ---- the link to cg OBS ----------------------------------------------------

#[tokio::test]
async fn without_an_obs_client_every_call_is_none() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let up = Upstream::new(None, events);
    assert_eq!(up.request("GetSceneList", None).await, None);
}

#[tokio::test]
async fn a_call_is_answered_through_the_obs_command_channel() {
    let (events, _) = broadcast::channel::<ObsEvent>(4);
    let (tx, mut rx) = mpsc::channel::<ObsCommand>(4);
    let up = Upstream::new(Some(tx), events);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let ObsCommand::Remote(RemoteCall::Request {
                request_type,
                request_data,
                reply,
                ..
            }) = cmd
            else {
                continue;
            };
            let _ = reply.send(Some(json!({
                "echo": request_type,
                "data": request_data,
            })));
        }
    });
    let d = up
        .request("GetSceneItemList", Some(json!({ "sceneName": "x" })))
        .await;
    assert_eq!(
        d,
        Some(json!({ "echo": "GetSceneItemList", "data": { "sceneName": "x" } }))
    );
    let d = up.request("GetSceneList", None).await;
    assert_eq!(d, Some(json!({ "echo": "GetSceneList", "data": null })));
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
            deadline,
            reply,
            ..
        })) => {
            assert_eq!(request_type, "SetCurrentProgramScene");
            // #221: the OBS side sees when its requester stops waiting.
            assert_eq!(deadline, started + UPSTREAM_TIMEOUT);
            // The OBS side skips it: a late switch never happens.
            assert!(reply.is_closed());
        }
        other => panic!("expected the queued request, got {other:?}"),
    }
}

/// #221: a mirror's waiter outwaits the forwarder's worst case — a switch
/// in flight, then the mirror's own answer, each at most the OBS client's
/// answer timeout.
#[test]
fn a_mirror_waits_for_two_answers_of_cg_obs_longer() {
    assert_eq!(
        MIRROR_EXTRA_WAIT,
        crate::obs::dispatcher::DEFAULT_RESPONSE_TIMEOUT * 2
    );
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
        .try_send(ObsCommand::Remote(RemoteCall::Request {
            request_type: "filler".to_string(),
            request_data: None,
            supersedes: false,
            deadline: tokio::time::Instant::now(),
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
    let listening = || {
        bus.remote()
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .listening
    };
    let error = || {
        bus.remote()
            .status(&RemoteSettings::disabled(), &OnAir::default())
            .error
    };

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
    // Disabled while the bind failed: nothing to stop, the error goes away.
    set_setting(&pool, "remote_ws_enabled", "false")
        .await
        .unwrap();
    wait_for("the stale error is dropped", || error().is_none()).await;
    set_setting(&pool, "remote_ws_enabled", "true")
        .await
        .unwrap();
    wait_for("the bind error shows again", || error().is_some()).await;
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
