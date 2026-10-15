//! #243 follow-up: `POST /api/v1/program/spout/quiesce`. A deploy asks it
//! right before it kills SongPlayer: the `program-max` thread (the only
//! thread that calls Spout) drops both Spout senders and makes no Spout call
//! while the quiesce holds (a minute from the last ask), so the kill can
//! never leave one of Spout's locks abandoned (PP, 12.10.2026: a
//! `taskkill /F` mid-lock left Spout's sender list to Arena for good).
//! Through the real axum router, over a real `run_max_loop` on the fake
//! GPU; the deploy polls `GET /api/v1/program` until `max.state` reads
//! `quiesced`.
//! Wired via `#[cfg(test)] #[path = "program_tests_quiesce.rs"] mod tests_quiesce;`.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::http::StatusCode;

use super::tests::call;
use crate::api::routes::tests::test_state;
use crate::playback::program_max::MaxOut;
use crate::playback::program_max_worker::tests::{FakeGpu, picture, plain, spawn_loop, wait_until};

/// The `program-max` loop of a test: stopped and joined when dropped, so a
/// failing test never leaves its thread waiting.
struct Loop {
    max: Arc<MaxOut>,
    thread: Option<JoinHandle<()>>,
}

impl Loop {
    fn spawn(max: &Arc<MaxOut>, gpu: &FakeGpu) -> Self {
        Self {
            max: max.clone(),
            thread: Some(spawn_loop(max, gpu)),
        }
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        self.max.stop();
        // Bounded, and never a panic here (this may run while a failed
        // assertion unwinds): wait for the end, then join.
        for _ in 0..10_000 {
            if self.finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Some(thread) = self.thread.take()
            && thread.is_finished()
        {
            let _ = thread.join();
        }
    }
}

/// `GET /api/v1/program`'s `max.state` and `max.fhd.state` once `max.state`
/// reads `quiesced`, or as they are when 20 s passed (the deploy waits 5 s).
async fn states_once_quiesced(state: &crate::AppState) -> (serde_json::Value, serde_json::Value) {
    let mut last = (serde_json::Value::Null, serde_json::Value::Null);
    for _ in 0..4_000 {
        let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
        last = (
            json["max"]["state"].clone(),
            json["max"]["fhd"]["state"].clone(),
        );
        if last.0 == "quiesced" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    last
}

/// Both senders went out (MAX twice, SP-program once); the quiesce drops both (each before its
/// compositor), `max.state` and `max.fhd.state` read `quiesced`, and while
/// it holds (a minute) the thread lives on with nothing offered, built or
/// sent.
#[tokio::test]
async fn a_quiesce_drops_both_spout_senders_and_ends_every_spout_call() {
    let state = test_state().await;
    let max = state.program_bus.max().clone();
    max.set_enabled(true);
    max.set_fhd_enabled(true);
    let gpu = FakeGpu::default();
    let running = Loop::spawn(&max, &gpu);
    wait_until("the loop takes jobs", || max.accepting());
    let a = picture(4, 2);
    max.offer_with(|| plain(1, &a));
    // MAX goes first, whole: the SP-program output is built after MAX's
    // send and goes out from the next boundary.
    wait_until("MAX went out, SP-program built", || {
        max.status().submitted == 1 && gpu.log().fhd_senders_built == 1
    });
    max.offer_with(|| plain(2, &a));
    wait_until("MAX and SP-program went out", || {
        let status = max.status();
        status.submitted == 2 && status.fhd.submitted == 1
    });

    let (status, json) = call(state.clone(), "POST", "/api/v1/program/spout/quiesce", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "the route takes it: {json}");
    let (max_state, fhd_state) = states_once_quiesced(&state).await;
    assert_eq!(max_state, "quiesced", "what the deploy polls for");
    assert_eq!(fhd_state, "quiesced", "the SP-program sender too");
    assert_eq!(
        gpu.log().drops,
        ["sender", "fhd sender", "compositor"],
        "both Spout senders dropped, then their one compositor (#251 step 2)"
    );
    assert!(!running.finished(), "the thread lives on, quiesced");
    assert!(
        !max.offer_with(|| plain(3, &a)),
        "a quiesced MAX takes no job"
    );
    {
        // A block, not `drop(log)`: clippy's await_holding_lock does not
        // see an explicit drop before the await below.
        let log = gpu.log();
        assert_eq!(
            (log.compositors_built, log.senders_built, log.sent),
            (1, 1, 2),
            "MAX: nothing built or sent after the quiesce"
        );
        assert_eq!(
            (log.targets_added.len(), log.fhd_senders_built, log.fhd_sent),
            (1, 1, 1),
            "SP-program: nothing built or sent after the quiesce"
        );
    }
    let (_, json) = call(state, "GET", "/api/v1/program", None).await;
    assert_eq!(
        json["max"]["state"], "quiesced",
        "it stays quiesced while the quiesce holds"
    );
}

/// A box whose MAX is switched off (no sender, the thread waits) is
/// quiesced at once too: the deploy's poll never waits its 5 s for nothing.
#[tokio::test]
async fn a_quiesce_with_max_switched_off_reads_quiesced_too() {
    let state = test_state().await;
    let max = state.program_bus.max().clone();
    let gpu = FakeGpu::default();
    // Whether the thread attached before the ask or after it, its first
    // step is the quiesce.
    let _running = Loop::spawn(&max, &gpu);
    let (_, json) = call(state.clone(), "GET", "/api/v1/program", None).await;
    assert_eq!(json["max"]["state"], "off", "MAX is off before");

    let (status, _) = call(state.clone(), "POST", "/api/v1/program/spout/quiesce", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (max_state, _) = states_once_quiesced(&state).await;
    assert_eq!(max_state, "quiesced", "quiesced wins over off");
    assert_eq!(gpu.log().compositors_built, 0, "nothing was ever built");
}
