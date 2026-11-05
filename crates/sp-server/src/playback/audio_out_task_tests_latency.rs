//! #289: the outputs task applies `audio_low_latency` live — a pass that
//! reads a changed switch rebuilds every VBAN output with it (never a
//! restart), keeps an ASIO output, and an unchanged switch keeps everything.

use super::super::*;
use super::{FIXED, NET, dvs, entry, no_thread, settings, vban};
use crate::playback::audio_out::OutputSink;
use sp_core::audio_latency::SETTING_AUDIO_LOW_LATENCY;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn a_vban_output_built_with_the_other_switch_is_rebuilt() {
    let a = entry("out-1", FIXED);
    let built = |low: bool| crate::playback::audio_out::RunningOutput {
        entry: a.clone(),
        built_rate: 48_000,
        sink: Some(OutputSink::Vban(Arc::new(
            VbanOut::for_entry_at(&a, 48_000, low).unwrap(),
        ))),
        error: None,
    };
    for low in [false, true] {
        let running = [built(low)];
        let wanted = std::slice::from_ref(&a);
        assert!(built_at(&running[0], low));
        assert!(!built_at(&running[0], !low));
        assert_eq!(
            plan_at(&running, wanted, 48_000, low).steps,
            vec![Step::Keep(0)]
        );
        let other = plan_at(&running, wanted, 48_000, !low);
        assert_eq!((other.steps, other.stop), (vec![Step::Build], vec![0]));
    }
    assert_eq!(
        plan(&[built(false)], std::slice::from_ref(&a), 48_000).steps,
        vec![Step::Keep(0)],
        "plan = the switch off"
    );
}

#[test]
fn an_asio_or_unbuilt_output_is_not_built_by_the_switch() {
    let d = dvs("out-3", "Dante Virtual Soundcard (x64)");
    let ran = crate::playback::audio_out::RunningOutput {
        entry: d.clone(),
        built_rate: 0,
        sink: None,
        error: None,
    };
    assert!(built_at(&ran, true) && built_at(&ran, false));
    assert_eq!(
        plan_at(&[ran], &[d], 96_000, true).steps,
        vec![Step::Keep(0)]
    );
}

#[tokio::test]
async fn apply_rebuilds_every_vban_output_when_the_switch_flips() {
    let outputs = AudioOutputs::new();
    let mut resolved = HashMap::new();
    let list = || settings(vec![entry("out-1", FIXED), entry("out-2", NET)], 96_000);
    apply(&outputs, list(), &mut resolved, &no_thread).await;
    let off = outputs.running();
    assert!(!vban(&off[0]).low_latency() && !vban(&off[1]).low_latency());
    apply(&outputs, list(), &mut resolved, &no_thread).await;
    assert!(
        Arc::ptr_eq(&vban(&off[0]), &vban(&outputs.running()[0])),
        "an unchanged switch keeps the output"
    );
    outputs.set_low_latency(true);
    apply(&outputs, list(), &mut resolved, &no_thread).await;
    let on = outputs.running();
    for i in 0..2 {
        assert!(!Arc::ptr_eq(&vban(&off[i]), &vban(&on[i])), "rebuilt {i}");
        assert!(vban(&on[i]).low_latency(), "{i}");
        assert_eq!(
            vban(&off[i]).take_timeout(std::time::Duration::ZERO),
            crate::playback::vban_out::VbanTake::Stopped,
            "the old output {i} is discarded"
        );
    }
    let status = outputs.status();
    assert_eq!(status[0].latency_ms, 15.0, "48 kHz: L alone");
    assert_eq!(status[1].latency_ms, 15.0 + 260.0 / 96.0, "96 kHz unpadded");
}

#[tokio::test]
async fn a_pass_reads_the_switch_from_the_settings() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let one =
        r#"[{"id":"out-1","name":"a","type":"vban","vban":{"host":"127.0.0.1","port":6980}}]"#;
    crate::db::models::set_setting(&pool, "audio_outputs", one)
        .await
        .unwrap();
    let outputs = AudioOutputs::new();
    let mut state = TaskState::default();
    tick(&pool, &outputs, &mut state, &no_thread).await;
    assert!(!outputs.low_latency(), "unset = off");
    assert!(!vban(&outputs.running()[0]).low_latency());
    for (stored, on) in [("true", true), ("junk", false), ("TRUE", true), ("", false)] {
        crate::db::models::set_setting(&pool, SETTING_AUDIO_LOW_LATENCY, stored)
            .await
            .unwrap();
        tick(&pool, &outputs, &mut state, &no_thread).await;
        assert_eq!(outputs.low_latency(), on, "{stored:?}");
        assert_eq!(vban(&outputs.running()[0]).low_latency(), on, "{stored:?}");
    }
}
