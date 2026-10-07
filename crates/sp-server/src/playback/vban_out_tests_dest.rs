//! #233: a VBAN output per destination — its rate, format and delay on the
//! wire and on the schedule; the queue holds the delay; a migrated FOH entry
//! sends exactly the 0.72.0 datagrams.

use super::tests::{FakeClock, RecordingSink, active_config};
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_migrate::entries_from_vban;
use crate::playback::vban_packet::tests::parse_packet;
use crate::playback::vban_packet::tests_legacy::{legacy_encode_block, oracle_blocks};
use crate::playback::vban_packet::{VBAN_SEND_LATENCY_100NS, VbanFormat, stream_name_bytes};
use sp_core::audio_outputs::{RateChoice, VbanDest, VbanSampleFormat};
use std::sync::Arc;

const D: i64 = 17_900_000_000_000_000;
const L: i64 = VBAN_SEND_LATENCY_100NS;

fn block(due: i64, samples: Option<Vec<f32>>) -> ProgramBlock {
    ProgramBlock {
        due_100ns: due,
        samples: samples.map(Into::into),
        substituted: false,
    }
}

/// Every datagram `out`'s sender puts on the wire for `blocks`, with its
/// send instant, on a fake clock starting at `D`.
fn send(out: &VbanOut, blocks: &[ProgramBlock]) -> Vec<(i64, Vec<u8>)> {
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::for_out(out);
    for b in blocks {
        sender.send_block(out, b, &mut sink, &mut clock);
    }
    sink.sent.into_iter().map(|(t, _, p)| (t, p)).collect()
}

fn dest(host: &str, stream: &str) -> VbanDest {
    VbanDest {
        host: host.into(),
        port: 6980,
        stream_name: stream.into(),
        format: VbanSampleFormat::Int24,
    }
}

#[test]
fn a_migrated_foh_entry_sends_the_0_72_datagrams() {
    let entry = entries_from_vban(true, "sp-program", "fohabl.lan:6980")
        .entries
        .remove(0);
    let out = VbanOut::for_entry(&entry, 96_000).expect("a FOH output");
    assert_eq!(
        out.format(),
        VbanFormat::PROGRAM,
        "fixed 48 kHz INT24 on a 96 kHz network"
    );
    assert_eq!(out.delay_100ns(), 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let blocks: Vec<ProgramBlock> = oracle_blocks()
        .into_iter()
        .enumerate()
        .map(|(i, s)| block(D + i as i64 * 333_333, s))
        .collect();
    let sent = send(&out, &blocks);
    let name = stream_name_bytes("sp-program");
    let mut counter = 0u32;
    let want: Vec<[u8; 1228]> = blocks
        .iter()
        .flat_map(|b| legacy_encode_block(&mut counter, &name, b.samples.as_deref()))
        .collect();
    assert_eq!(sent.len(), want.len());
    for (i, ((_, got), want)) in sent.iter().zip(&want).enumerate() {
        assert_eq!(&got[..], &want[..], "datagram {i}");
    }
    let firsts: Vec<i64> = sent.iter().step_by(8).map(|(t, _)| *t).collect();
    assert_eq!(firsts[0], D + L, "the #210 schedule");
    assert_eq!(firsts[1], D + 333_333 + L);
    assert_eq!(out.status().blocks_sent, 5);
}

#[test]
fn a_96k_destination_sends_16_packets_a_slot_with_index_4() {
    let format = VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap();
    let out = VbanOut::for_destination(format, 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = send(&out, &[block(D, Some(vec![0.25; 3200]))]);
    assert_eq!(sent.len(), 16);
    let times: Vec<i64> = sent.iter().map(|(t, _)| *t - D - L).collect();
    assert_eq!(&times[..4], &[0, 20_833, 41_666, 62_500]);
    assert_eq!(times[15], 312_499);
    for (k, (_, p)) in sent.iter().enumerate() {
        let parsed = parse_packet(p);
        assert_eq!(
            (parsed.format_sr, parsed.nbs, parsed.counter),
            (4, 199, k as u32)
        );
        assert_eq!(p.len(), 1228);
    }
    let st = out.status();
    assert_eq!((st.blocks_sent, st.packets_sent), (1, 16));
}

#[test]
fn a_96k_destination_carries_the_converted_tone() {
    // One second of a 1 kHz tone through the 96 kHz output: the packets
    // carry it at 96 kHz (the converter between the queue and the encoder).
    let format = VbanFormat::new(96_000, VbanSampleFormat::Int24).unwrap();
    let out = VbanOut::for_destination(format, 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let blocks: Vec<ProgramBlock> = (0..30)
        .map(|b| {
            let samples: Vec<f32> = (0..1600)
                .flat_map(|i| {
                    let n = (b * 1600 + i) as f32;
                    let x = (n * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.5;
                    [x, x]
                })
                .collect();
            block(D + b as i64 * 333_333, Some(samples))
        })
        .collect();
    let sent = send(&out, &blocks);
    assert_eq!(sent.len(), 30 * 16);
    let left: Vec<i32> = sent[3 * 16..]
        .iter()
        .flat_map(|(_, p)| parse_packet(p).samples.into_iter().step_by(2))
        .collect();
    assert_eq!(left.len(), 27 * 3_200);
    let rising = left.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    assert!(
        (898..=902).contains(&rising),
        "{rising} rising zero crossings in 0.9 s"
    );
}

#[test]
fn a_float32_destination_sends_10_packets_of_160_frames() {
    let format = VbanFormat::new(48_000, VbanSampleFormat::Float32).unwrap();
    let out = VbanOut::for_destination(format, 0);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = send(&out, &[block(D, Some(vec![0.25; 3200]))]);
    assert_eq!(sent.len(), 10);
    assert!(sent.iter().all(|(_, p)| p.len() == 1308 && p[7] == 0x04));
    assert_eq!(&sent[0].1[28..32], &0.25f32.to_le_bytes(), "48 kHz: no converter");
    assert_eq!(sent[1].0 - sent[0].0, 33_333, "1/300 s apart");
}

#[test]
fn a_delay_moves_every_packet_by_the_delay() {
    let out = VbanOut::for_destination(VbanFormat::PROGRAM, 2_500_000);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = send(&out, &[block(D, Some(vec![0.25; 3200]))]);
    assert_eq!(sent[0].0, D + L + 2_500_000);
    assert_eq!(sent[1].0, D + L + 2_500_000 + 41_666);
}

#[test]
fn the_longest_delay_is_waited_for_whole() {
    // 2 s of delay: the first packet is 2 s + L after its boundary, past the
    // 4-slot cap a sender without a delay keeps (a clock mismatch).
    let out = VbanOut::for_destination(VbanFormat::PROGRAM, 20_000_000);
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let mut clock = FakeClock::at(D);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::for_out(&out);
    sender.send_block(&out, &block(D, None), &mut sink, &mut clock);
    assert_eq!(sink.sent[0].0, D + L + 20_000_000);
    assert_eq!(clock.sleeps[0], L + 20_000_000, "one wait");
    // A block whose packets lie further out than that is not waited for:
    // the cap is the delay + 4 slots.
    let mut far = FakeClock::at(D);
    let mut sink = RecordingSink::on(&far);
    sender.send_block(&out, &block(D + 10_000_000_000, None), &mut sink, &mut far);
    assert_eq!(far.sleeps[0], VBAN_MAX_WAIT_100NS + 20_000_000);
    assert_eq!(plan_wait_up_to(0, 1_000, 999), 999);
    assert_eq!(plan_wait_up_to(0, 1_000, 1_001), 1_000);
    assert_eq!(plan_wait_up_to(1_001, 1_000, 5), 0);
}

#[test]
fn the_queue_holds_the_delay() {
    assert_eq!(
        SLOT_100NS as i64,
        sp_core::genlock::UNITS_PER_SECOND / sp_core::genlock::GENLOCK_GRID_FPS
    );
    assert_eq!(queue_bound(0), VBAN_QUEUE_BOUND);
    assert_eq!(queue_bound(1), VBAN_QUEUE_BOUND + 1);
    assert_eq!(queue_bound(333_333), VBAN_QUEUE_BOUND + 1);
    assert_eq!(queue_bound(333_334), VBAN_QUEUE_BOUND + 2);
    assert_eq!(queue_bound(-5), VBAN_QUEUE_BOUND);
    let out = VbanOut::for_destination(VbanFormat::PROGRAM, 20_000_000);
    assert_eq!(out.bound(), VBAN_QUEUE_BOUND + 61);
    for i in 0..out.bound() as i64 {
        out.push(ProgramBlock::silence(D + i));
    }
    assert_eq!(
        out.status().blocks_dropped,
        0,
        "2 s of delay is all queued"
    );
    out.push(ProgramBlock::silence(D + 999));
    assert_eq!(out.status().blocks_dropped, 1);
    assert_eq!(VbanOut::new().bound(), VBAN_QUEUE_BOUND);
}

#[test]
fn an_entry_at_the_network_rate_follows_the_network() {
    let mut entry = entries_from_vban(true, "", "h:1").entries.remove(0);
    entry.rate = RateChoice::Network;
    entry.delay_ms = 40;
    let out = VbanOut::for_entry(&entry, 96_000).unwrap();
    assert_eq!(out.format().rate_hz(), 96_000);
    assert_eq!(out.delay_100ns(), 400_000);
    assert_eq!(out.bound(), queue_bound(400_000));
    entry.vban.as_mut().unwrap().format = VbanSampleFormat::Int16;
    assert_eq!(
        VbanOut::for_entry(&entry, 44_100).unwrap().format(),
        VbanFormat::new(44_100, VbanSampleFormat::Int16).unwrap()
    );
    entry.vban = None;
    assert_eq!(
        VbanOut::for_entry(&entry, 48_000).err().as_deref(),
        Some("not a VBAN entry")
    );
}

#[tokio::test]
async fn resolve_dest_builds_one_target_and_disabled_sends_nothing() {
    let d = dest("127.0.0.1", "foh-test");
    let cfg = resolve_dest(d.clone(), true, Vec::new()).await;
    assert!(cfg.is_active());
    assert_eq!(cfg.stream_name, "foh-test");
    assert_eq!(cfg.targets.len(), 1);
    assert_eq!(cfg.targets[0].spec, "127.0.0.1:6980");
    assert_eq!(cfg.targets[0].addr, Some("127.0.0.1:6980".parse().unwrap()));
    assert_eq!(target_spec(&d), "127.0.0.1:6980");
    let off = resolve_dest(d, false, cfg.targets.clone()).await;
    assert!(!off.is_active(), "disabled sends nothing");
}

#[test]
fn the_config_carries_the_wire_name_and_the_status_its_target() {
    let cfg = VbanConfig::for_dest(
        &dest("fohabl.lan", "é-a-very-long-stream"),
        true,
        vec![VbanTarget {
            spec: "fohabl.lan:6980".into(),
            addr: Some("10.77.7.30:6980".parse().unwrap()),
            error: None,
        }],
    );
    assert_eq!(cfg.stream_name, "_-a-very-long-st", "what goes on the wire");
    assert_eq!(&cfg.name_bytes, b"_-a-very-long-st");
    let out = VbanOut::new();
    out.set_config(cfg);
    let st = out.status();
    assert!(st.enabled);
    assert_eq!(st.stream_name, "_-a-very-long-st");
    assert_eq!(st.targets[0].target, "fohabl.lan:6980");
    assert_eq!(st.targets[0].addr.as_deref(), Some("10.77.7.30:6980"));
    let def = VbanConfig::default();
    assert!(!def.enabled);
    assert_eq!(def.stream_name, "sp-program");
    assert!(def.targets.is_empty());
}

#[test]
fn a_block_shares_its_samples_between_outputs() {
    let frame = sp_ndi::AudioFrame {
        data: vec![0.5; 3200],
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    };
    let b = ProgramBlock::copied(D, &[frame]);
    let c = b.clone();
    assert!(Arc::ptr_eq(
        b.samples.as_ref().unwrap(),
        c.samples.as_ref().unwrap()
    ));
}
