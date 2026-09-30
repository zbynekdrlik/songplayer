//! #221 L2: the pure parts of the switch path and the mirror's waiter. The
//! path itself runs end to end over real sockets in `remote/session_tests.rs`
//! and `remote/session_tests_studio.rs`.
//! Wired via `#[cfg(test)] #[path = "program_switch_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sp_core::config::PROGRAM_INPUT_ID;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use super::*;
use crate::obs::ObsCommand;
use crate::obs::remote_call::RemoteCall;
use crate::playback::legacy_cg::LegacyCg;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_on_air::OnAir;
use crate::remote::{MIRROR_EXTRA_WAIT, RemoteSettings, RemoteShared, UPSTREAM_TIMEOUT, Upstream};

#[test]
fn cg_obs_answers_read_as_cg_forward() {
    assert_eq!(cg_forward_label(None), "not_ready");
    let ok = json!({ "requestStatus": { "result": true, "code": 100 } });
    assert_eq!(cg_forward_label(Some(&ok)), "ok");
    let refused = json!({ "requestStatus": {
        "result": false,
        "code": 600,
        "comment": "No source was found by the name of `Nope`.",
    }});
    assert_eq!(cg_forward_label(Some(&refused)), "error 600");
    assert_eq!(
        cg_forward_label(Some(&json!({}))),
        "error 205",
        "an answer without a request status"
    );
}

#[test]
fn a_switch_names_what_triggered_it() {
    assert_eq!(Via::Program.label(), "program");
    assert_eq!(Via::Transition.label(), "transition");
    assert_eq!(Via::Dashboard.label(), "dashboard");
}

#[test]
fn the_records_of_a_cut_and_of_a_kept_program() {
    let status = ProgramBus::new().status();
    let to_input = cut_done("Slido", Via::Transition, -1, &status, Some("ok".into()));
    assert_eq!(
        (to_input.action, to_input.source, to_input.reason),
        ("input", Some(-1), None)
    );
    assert_eq!(to_input.via, Some("transition"));
    assert_eq!(to_input.cg_forward.as_deref(), Some("ok"));
    let to_playlist = cut_done("SP-fast", Via::Program, 7, &status, None);
    assert_eq!(
        (
            to_playlist.action,
            to_playlist.source,
            to_playlist.scene.as_str()
        ),
        ("playlist", Some(7), "SP-fast")
    );
    assert_eq!(to_playlist.via, Some("program"));
    let long = "S".repeat(100);
    let held = kept(&long, Via::Program, PERSIST_FAILED, None);
    assert_eq!(
        (held.action, held.source, held.reason),
        ("keep", None, Some("persist_failed"))
    );
    assert_eq!(
        held.scene,
        "S".repeat(64),
        "a client-chosen name is clipped"
    );
    assert!(held.at_ms > 1_700_000_000_000, "{}", held.at_ms);
}

/// A recorded playlist cut whose mirror answer is still due.
fn pending_cut(shared: &RemoteShared) -> u64 {
    let status = ProgramBus::new().status();
    let cut = cut_done("sp-fast", Via::Program, 7, &status, None);
    let id = shared.record_cut(cut);
    shared.set_cg_forward(id, CG_PENDING.to_string());
    id
}

fn last_forward(shared: &RemoteShared) -> Option<String> {
    shared
        .status(&RemoteSettings::disabled(), &OnAir::default())
        .last_remote_cut
        .and_then(|cut| cut.cg_forward)
}

#[tokio::test]
async fn the_mirrors_answer_is_recorded_on_its_cut() {
    let (events, _) = broadcast::channel(4);
    let upstream = Upstream::new(None, events);
    let shared = Arc::new(RemoteShared::default());

    let id = pending_cut(&shared);
    let (tx, rx) = oneshot::channel();
    tx.send(Some(
        json!({ "requestStatus": { "result": true, "code": 100 } }),
    ))
    .unwrap();
    record_mirror(
        upstream.clone(),
        rx,
        Arc::clone(&shared),
        id,
        "sp-fast".into(),
    )
    .await;
    assert_eq!(last_forward(&shared).as_deref(), Some("ok"));

    // cg OBS refused it.
    let id = pending_cut(&shared);
    let (tx, rx) = oneshot::channel();
    let refused = json!({ "requestStatus": { "result": false, "code": 600 } });
    tx.send(Some(refused)).unwrap();
    record_mirror(
        upstream.clone(),
        rx,
        Arc::clone(&shared),
        id,
        "sp-fast".into(),
    )
    .await;
    assert_eq!(last_forward(&shared).as_deref(), Some("error 600"));

    // The call was dropped (cg OBS went away): not ready.
    let id = pending_cut(&shared);
    let (tx, rx) = oneshot::channel::<Option<Value>>();
    drop(tx);
    record_mirror(
        upstream.clone(),
        rx,
        Arc::clone(&shared),
        id,
        "sp-fast".into(),
    )
    .await;
    assert_eq!(last_forward(&shared).as_deref(), Some("not_ready"));

    // A later press replaced the cut: its answer changes nothing.
    let old = pending_cut(&shared);
    let _newer = pending_cut(&shared);
    let (tx, rx) = oneshot::channel();
    tx.send(Some(
        json!({ "requestStatus": { "result": true, "code": 100 } }),
    ))
    .unwrap();
    record_mirror(upstream, rx, Arc::clone(&shared), old, "sp-fast".into()).await;
    assert_eq!(last_forward(&shared).as_deref(), Some("pending"));
}

/// #221 review round 5: the OBS client writes a mirror however late (its cut
/// already happened), and may first wait for a switch in flight, so the
/// mirror's waiter waits longer than the upstream timeout: an answer after it
/// is still recorded. The wait is still bounded.
#[tokio::test(start_paused = true)]
async fn a_mirror_answered_after_the_upstream_timeout_is_still_recorded() {
    let (events, _) = broadcast::channel(4);
    let upstream = Upstream::new(None, events);
    let shared = Arc::new(RemoteShared::default());

    let id = pending_cut(&shared);
    let (tx, rx) = oneshot::channel();
    let shared_waiter = Arc::clone(&shared);
    let waiter = tokio::spawn(record_mirror(
        upstream.clone(),
        rx,
        shared_waiter,
        id,
        "sp-fast".into(),
    ));
    tokio::time::sleep(UPSTREAM_TIMEOUT + Duration::from_millis(500)).await;
    let _ = tx.send(Some(
        json!({ "requestStatus": { "result": true, "code": 100 } }),
    ));
    waiter.await.unwrap();
    assert_eq!(last_forward(&shared).as_deref(), Some("ok"));

    // Never answered: not ready, once the whole mirror wait is over.
    let id = pending_cut(&shared);
    let (_tx, rx) = oneshot::channel::<Option<Value>>();
    let started = Instant::now();
    record_mirror(upstream, rx, Arc::clone(&shared), id, "sp-fast".into()).await;
    assert_eq!(started.elapsed(), UPSTREAM_TIMEOUT + MIRROR_EXTRA_WAIT);
    assert_eq!(last_forward(&shared).as_deref(), Some("not_ready"));
}

/// #221 L4a: the mirror's waiter says whether cg OBS accepted it — only an
/// OK answer, never a refusal or no answer.
#[tokio::test]
async fn the_mirrors_waiter_says_whether_cg_obs_accepted() {
    let (events, _) = broadcast::channel(4);
    let upstream = Upstream::new(None, events);
    let shared = Arc::new(RemoteShared::default());
    let ok = json!({ "requestStatus": { "result": true, "code": 100 } });
    let refused = json!({ "requestStatus": { "result": false, "code": 600 } });
    for (answer, accepted) in [(Some(ok), true), (Some(refused), false), (None, false)] {
        let id = pending_cut(&shared);
        let (tx, rx) = oneshot::channel();
        tx.send(answer.clone()).unwrap();
        let shared = Arc::clone(&shared);
        let got = record_mirror(upstream.clone(), rx, shared, id, "sp-fast".into()).await;
        assert_eq!(got, accepted, "{answer:?}");
    }
}

/// #221 L4a: an accepted mirror is recorded in `legacy_cg` by its ticket; a
/// refused one changes nothing, and a late answer to an older mirror never
/// overwrites a newer one's.
#[tokio::test]
async fn an_accepted_mirror_is_recorded_as_shown_by_its_ticket() {
    let legacy = Arc::new(LegacyCg::default());
    let refused = legacy.ticket();
    confirm_mirror(async { false }, Arc::clone(&legacy), refused, 7).await;
    assert_eq!(legacy.shown_now(), None);
    let accepted = legacy.ticket();
    confirm_mirror(async { true }, Arc::clone(&legacy), accepted, 7).await;
    assert_eq!(legacy.shown_now(), Some(7));
    let older = legacy.ticket();
    let newer = legacy.ticket();
    confirm_mirror(async { true }, Arc::clone(&legacy), newer, 3).await;
    confirm_mirror(async { true }, Arc::clone(&legacy), older, 7).await;
    assert_eq!(legacy.shown_now(), Some(3));
}

fn ok() -> Option<Value> {
    Some(json!({ "requestStatus": { "result": true, "code": 100 } }))
}

/// A link to a cg OBS whose command queue the test reads.
fn cg_queue() -> (Upstream, mpsc::Receiver<ObsCommand>) {
    let (tx, rx) = mpsc::channel(4);
    (Upstream::new(Some(tx), broadcast::channel(4).0), rx)
}

/// The scene switch on cg OBS's queue, and the reply to answer it with.
fn queued_switch(rx: &mut mpsc::Receiver<ObsCommand>) -> (Value, oneshot::Sender<Option<Value>>) {
    let Ok(ObsCommand::Remote(RemoteCall::Request {
        request_type,
        request_data,
        supersedes,
        reply,
        ..
    })) = rx.try_recv()
    else {
        panic!("the re-mirror must be on cg OBS's command queue");
    };
    assert_eq!(request_type, "SetCurrentProgramScene");
    assert!(supersedes, "a mirror: its program is already on air");
    (request_data.expect("a scene name"), reply)
}

/// #221 L4b, main-session decision 1 (comment 5884501960): at startup cg OBS
/// is told to show the restored playlist's catalog scene, through the
/// ticketed mirror: one switch per call (and `start_program`, which is
/// `mutants::skip` orchestration, calls it once); its OK is recorded in
/// `legacy_cg` by the ticket taken when it was sent, so a late OK never
/// overwrites a newer command's answer.
#[tokio::test]
async fn the_startup_re_mirror_sends_the_restored_playlist_scene_through_the_ticketed_mirror() {
    let bus = ProgramBus::new();
    bus.select_initial(7, Some("sp-fast"));
    bus.legacy_cg().restored(7);
    let (upstream, mut rx) = cg_queue();
    let mut shown = bus.legacy_cg().shown(); // has seen the restored value

    assert!(
        remirror_on_air(&bus, &upstream).await,
        "the mirror is queued"
    );
    let (data, reply) = queued_switch(&mut rx);
    assert_eq!(data, json!({ "sceneName": "sp-fast" }));
    assert!(rx.try_recv().is_err(), "one switch per call");
    reply.send(ok()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), shown.changed())
        .await
        .expect("cg OBS's OK is recorded")
        .unwrap();
    assert_eq!(bus.legacy_cg().shown_now(), Some(7));

    // Its ticket is taken when it is sent: a newer command's answer stays.
    assert!(remirror_on_air(&bus, &upstream).await);
    let (_, late) = queued_switch(&mut rx);
    let newer = bus.legacy_cg().ticket();
    assert!(bus.legacy_cg().confirmed(newer, Some(3)));
    late.send(ok()).unwrap();
    // A "must not change" window, only in the safe direction: correct code
    // never changes `shown` here, whatever the runner's speed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        bus.legacy_cg().shown_now(),
        Some(3),
        "the late OK is dropped"
    );
}

/// Nothing is re-mirrored when no playlist scene is on program: nothing
/// restored, the NDI input "OBS manuál" (cg OBS keeps its manual scene, even
/// with a scene name), or a playlist whose catalog names no scene; nor when
/// cg OBS is not reachable.
#[tokio::test]
async fn no_playlist_scene_on_program_nothing_is_re_mirrored() {
    let cases = [
        (None, None),
        (Some(PROGRAM_INPUT_ID), None),
        (Some(PROGRAM_INPUT_ID), Some("Slido")),
        (Some(5), None),
    ];
    for (source, scene) in cases {
        let bus = ProgramBus::new();
        if let Some(source) = source {
            bus.select_initial(source, scene);
        }
        let (upstream, mut rx) = cg_queue();
        let queued = remirror_on_air(&bus, &upstream).await;
        assert!(!queued, "{source:?} {scene:?}");
        assert!(rx.try_recv().is_err(), "{source:?} {scene:?}: nothing sent");
    }
    let bus = ProgramBus::new();
    bus.select_initial(7, Some("sp-fast"));
    assert!(
        !remirror_on_air(&bus, &Upstream::unlinked()).await,
        "no OBS client: nothing queued"
    );
}

/// The re-mirror's waiter says whether cg OBS accepted it: only an OK, never
/// a refusal or no answer.
#[tokio::test]
async fn the_startup_answer_says_whether_cg_obs_accepted() {
    let refused = json!({ "requestStatus": { "result": false, "code": 600 } });
    for (answer, accepted) in [(ok(), true), (Some(refused), false), (None, false)] {
        let (tx, rx) = oneshot::channel();
        tx.send(answer.clone()).unwrap();
        let got = startup_answer(Upstream::unlinked(), rx, "sp-fast".into()).await;
        assert_eq!(got, accepted, "{answer:?}");
    }
}
