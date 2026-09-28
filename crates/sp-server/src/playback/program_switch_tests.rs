//! #221 L2: the pure parts of the switch path and the mirror's waiter. The
//! path itself runs end to end over real sockets in `remote/session_tests.rs`
//! and `remote/session_tests_studio.rs`.
//! Wired via `#[cfg(test)] #[path = "program_switch_tests.rs"] mod tests;`.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use super::*;
use crate::playback::program_bus::ProgramBus;
use crate::remote::{RemoteSettings, RemoteShared, Upstream};

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
        .status(&RemoteSettings::disabled())
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
