//! #221 L2: the pure parts of the switch path. The path itself runs end to
//! end over real sockets in `remote/session_tests.rs` and
//! `remote/session_tests_studio.rs` (a press), and through the real router
//! in `api/program_tests_switch.rs` (a dashboard cut).
//! Wired via `#[cfg(test)] #[path = "program_switch_tests.rs"] mod tests;`.

use serde_json::json;

use super::*;
use crate::playback::program_bus::ProgramBus;

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
fn a_switch_names_what_triggered_it_and_why_it_kept_the_program() {
    assert_eq!(Via::Program.label(), "program");
    assert_eq!(Via::Transition.label(), "transition");
    assert_eq!(Via::Dashboard.label(), "dashboard");
    assert_eq!(
        [NOT_SWITCHED, INPUT_INACTIVE, CATALOG_FAILED, PERSIST_FAILED],
        [
            "not_switched",
            "input_inactive",
            "catalog_failed",
            "persist_failed"
        ]
    );
}

#[test]
fn the_records_of_a_cut_and_of_a_kept_program() {
    let status = ProgramBus::new().status();
    let to_input = cut_done("Slido", Via::Transition, -1, &status, Some("ok".into()));
    assert_eq!(
        (to_input.action, to_input.source, to_input.reason),
        ("input", Some(-1), None)
    );
    assert_eq!(to_input.via, "transition");
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
    assert_eq!(to_playlist.via, "program");
    assert_eq!(to_playlist.cg_forward, None, "cg OBS is told nothing");
    let long = "S".repeat(100);
    let held = kept(&long, Via::Dashboard, PERSIST_FAILED, None);
    assert_eq!(
        (held.action, held.source, held.reason, held.via),
        ("keep", None, Some("persist_failed"), "dashboard")
    );
    assert_eq!(
        held.scene,
        "S".repeat(64),
        "a client-chosen name is clipped"
    );
    assert!(held.at_ms > 1_700_000_000_000, "{}", held.at_ms);
}
