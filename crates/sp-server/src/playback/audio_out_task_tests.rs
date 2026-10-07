//! #233: the outputs task's plan (keep an unchanged entry, build a new or
//! changed one, stop a removed or changed one; a network-rate change rebuilds
//! only the entries that follow the network) and `apply` on real outputs:
//! an unchanged output keeps its queue, a kept one re-resolves on #210's
//! 60 s cadence, a build error is named on its entry; the task migrates at
//! its start and stops every output at shutdown.

use super::*;
use crate::playback::audio_out::{OutputSink, RunningOutput, STATE_DISABLED, STATE_WAITING};
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_config::OutputsSettings;
use crate::playback::vban_out::{VbanOut, VbanTake};
use sp_core::audio_outputs::{OutputEntry, RateChoice, VbanDest, VbanSampleFormat};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn entry(id: &str, rate: RateChoice) -> OutputEntry {
    let mut e = OutputEntry::vban(
        id,
        id,
        VbanDest {
            host: "127.0.0.1".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: VbanSampleFormat::Int24,
        },
    );
    e.rate = rate;
    e
}

fn ran(e: &OutputEntry, built_rate: u32) -> RunningOutput {
    RunningOutput {
        entry: e.clone(),
        built_rate,
        sink: None,
        error: None,
    }
}

const FIXED: RateChoice = RateChoice::Fixed(48_000);
const NET: RateChoice = RateChoice::Network;

#[test]
fn an_unchanged_list_keeps_everything() {
    let (a, b) = (entry("out-1", FIXED), entry("out-2", NET));
    let p = plan(&[ran(&a, 48_000), ran(&b, 96_000)], &[a, b], 96_000);
    assert_eq!(
        p,
        Plan {
            steps: vec![Step::Keep(0), Step::Keep(1)],
            stop: vec![]
        }
    );
}

#[test]
fn an_added_entry_is_built_and_the_rest_kept_in_settings_order() {
    let (a, b, c) = (
        entry("out-1", FIXED),
        entry("out-2", FIXED),
        entry("out-3", FIXED),
    );
    let p = plan(&[ran(&a, 48_000), ran(&b, 48_000)], &[c, b, a], 96_000);
    assert_eq!(p.steps, vec![Step::Build, Step::Keep(1), Step::Keep(0)]);
    assert!(p.stop.is_empty());
}

#[test]
fn a_changed_entry_is_rebuilt_and_its_old_output_stopped() {
    let a = entry("out-1", FIXED);
    let mut a2 = a.clone();
    a2.delay_ms = 10;
    let p = plan(&[ran(&a, 48_000)], &[a2], 48_000);
    assert_eq!(
        p,
        Plan {
            steps: vec![Step::Build],
            stop: vec![0]
        }
    );
    let mut off = a.clone();
    off.enabled = false;
    assert_eq!(
        plan(&[ran(&a, 48_000)], &[off], 48_000).stop,
        vec![0],
        "a toggle rebuilds"
    );
}

#[test]
fn a_removed_entry_is_stopped() {
    let (a, b) = (entry("out-1", FIXED), entry("out-2", FIXED));
    let p = plan(
        &[ran(&a, 48_000), ran(&b, 48_000)],
        std::slice::from_ref(&b),
        48_000,
    );
    assert_eq!(
        p,
        Plan {
            steps: vec![Step::Keep(1)],
            stop: vec![0]
        }
    );
    assert_eq!(
        plan(&[ran(&a, 48_000)], &[], 48_000),
        Plan {
            steps: vec![],
            stop: vec![0]
        }
    );
}

#[test]
fn a_network_rate_change_rebuilds_only_the_entries_that_follow_it() {
    let (foh, net) = (entry("out-1", FIXED), entry("out-2", NET));
    let running = [ran(&foh, 48_000), ran(&net, 48_000)];
    let p = plan(&running, &[foh.clone(), net.clone()], 96_000);
    assert_eq!(
        p,
        Plan {
            steps: vec![Step::Keep(0), Step::Build],
            stop: vec![1]
        }
    );
    assert_eq!(build_rate(&net, 96_000), 96_000);
    assert_eq!(build_rate(&foh, 96_000), 48_000);
}

fn settings(entries: Vec<OutputEntry>, network_rate: u32) -> OutputsSettings {
    OutputsSettings {
        entries,
        network_rate,
        problems: vec!["p".into()],
    }
}

fn vban(o: &RunningOutput) -> Arc<VbanOut> {
    match &o.sink {
        Some(OutputSink::Vban(out)) => out.clone(),
        None => panic!("{} has no output", o.entry.id),
    }
}

#[tokio::test]
async fn apply_keeps_an_unchanged_output_when_another_is_added() {
    let outputs = AudioOutputs::new();
    let mut resolved = HashMap::new();
    let (a, b) = (entry("out-1", FIXED), entry("out-2", NET));
    apply(
        &outputs,
        settings(vec![a.clone(), b.clone()], 96_000),
        &mut resolved,
    )
    .await;
    let first = outputs.running();
    let sink_a = vban(&first[0]);
    assert!(sink_a.config().is_active(), "resolved at build");
    assert_eq!(
        sink_a.format().rate_hz(),
        48_000,
        "out-1 is fixed at 48 kHz"
    );
    assert_eq!(vban(&first[1]).format().rate_hz(), 96_000, "out-2 follows");
    assert_eq!((first[0].built_rate, first[1].built_rate), (48_000, 96_000));
    sink_a.push(ProgramBlock::silence(1));
    let c = entry("out-3", FIXED);
    apply(
        &outputs,
        settings(vec![a.clone(), b.clone(), c], 96_000),
        &mut resolved,
    )
    .await;
    let second = outputs.running();
    assert_eq!(second.len(), 3);
    assert!(
        Arc::ptr_eq(&sink_a, &vban(&second[0])),
        "out-1 kept: its thread, queue, counter"
    );
    assert!(
        Arc::ptr_eq(&vban(&first[1]), &vban(&second[1])),
        "out-2 kept"
    );
    assert_eq!(sink_a.queued(), 1, "its queue kept");
    assert_eq!(outputs.network_rate(), 96_000);
    assert_eq!(outputs.problems(), vec!["p".to_string()]);
    assert_eq!(resolved.len(), 3);

    let mut a2 = a.clone();
    a2.delay_ms = 10;
    apply(&outputs, settings(vec![a2, b], 96_000), &mut resolved).await;
    let third = outputs.running();
    assert!(
        !Arc::ptr_eq(&sink_a, &vban(&third[0])),
        "a changed entry is rebuilt"
    );
    assert_eq!(vban(&third[0]).delay_100ns(), 100_000);
    assert_eq!(
        sink_a.take_timeout(Duration::ZERO),
        VbanTake::Block(ProgramBlock::silence(1))
    );
    assert_eq!(
        sink_a.take_timeout(Duration::ZERO),
        VbanTake::Stopped,
        "the old one stopped"
    );
    let c_out = vban(&second[2]);
    assert_eq!(
        c_out.take_timeout(Duration::ZERO),
        VbanTake::Stopped,
        "the removed one stopped"
    );
    assert!(
        !resolved.contains_key("out-3"),
        "a removed entry's resolve is forgotten"
    );
    assert!(
        resolved.contains_key("out-1"),
        "the rebuilt one resolved again"
    );
}

#[tokio::test]
async fn a_kept_output_re_resolves_on_the_60_s_cadence() {
    let outputs = AudioOutputs::new();
    let mut resolved = HashMap::new();
    let a = entry("out-1", FIXED);
    apply(&outputs, settings(vec![a.clone()], 48_000), &mut resolved).await;
    let out = vban(&outputs.running()[0]);
    // What the next apply must leave alone while the resolve is recent.
    let mut sentinel = out.config().as_ref().clone();
    sentinel.targets[0].addr = None;
    sentinel.targets[0].error = Some("sentinel".into());
    out.set_config(sentinel.clone());
    apply(&outputs, settings(vec![a.clone()], 48_000), &mut resolved).await;
    assert_eq!(*out.config(), sentinel, "resolved 0 s ago: not again");
    // As far as the cadence knows, never resolved: due now.
    resolved.remove("out-1");
    apply(&outputs, settings(vec![a], 48_000), &mut resolved).await;
    let cfg = out.config();
    assert!(cfg.is_active(), "re-resolved");
    assert_eq!(cfg.targets[0].error, None);
    assert!(resolved.contains_key("out-1"));
}

#[tokio::test]
async fn a_disabled_entry_runs_nothing_but_is_listed() {
    let outputs = AudioOutputs::new();
    let mut off = entry("out-1", FIXED);
    off.enabled = false;
    let mut resolved = HashMap::new();
    apply(&outputs, settings(vec![off], 48_000), &mut resolved).await;
    let list = outputs.running();
    assert_eq!(list.len(), 1);
    assert!(list[0].sink.is_none());
    assert_eq!(outputs.status()[0].state, STATE_DISABLED);
    assert!(resolved.is_empty(), "nothing resolved for it");
}

#[tokio::test]
async fn an_entry_vban_cannot_carry_is_named_on_its_line() {
    // Only a hand-made network rate reaches this (the stored one is always a
    // supported rate): the entry runs nothing and says why.
    let outputs = AudioOutputs::new();
    let net = entry("out-1", NET);
    apply(&outputs, settings(vec![net], 32_000), &mut HashMap::new()).await;
    let list = outputs.running();
    assert!(list[0].sink.is_none());
    assert_eq!(list[0].error.as_deref(), Some("VBAN carries no 32000 Hz"));
    let st = &outputs.status()[0];
    assert_eq!(
        (st.state, st.reason.as_deref()),
        (STATE_WAITING, Some("VBAN carries no 32000 Hz"))
    );
}

#[tokio::test]
async fn the_task_migrates_at_its_start_and_stops_every_output_at_shutdown() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (k, v) in [
        ("vban_enabled", "true"),
        ("vban_stream_name", "sp-program"),
        ("vban_targets", "127.0.0.1:6980,127.0.0.1:6981"),
    ] {
        crate::db::models::set_setting(&pool, k, v).await.unwrap();
    }
    let outputs = Arc::new(AudioOutputs::new());
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(run_outputs_task(pool.clone(), outputs.clone(), rx));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while outputs.running().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let list = outputs.running();
    assert_eq!(list.len(), 2, "both migrated targets run");
    assert_eq!(
        (list[0].entry.id.as_str(), list[1].entry.id.as_str()),
        ("out-1", "out-2")
    );
    assert_eq!(vban(&list[0]).format().rate_hz(), 48_000);
    assert!(
        crate::db::models::get_setting(&pool, "audio_outputs")
            .await
            .unwrap()
            .is_some(),
        "the list was written"
    );
    let _ = shutdown.send(());
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the task ends at shutdown")
        .expect("the task did not panic");
    assert_eq!(
        vban(&list[1]).take_timeout(Duration::ZERO),
        VbanTake::Stopped
    );
}
