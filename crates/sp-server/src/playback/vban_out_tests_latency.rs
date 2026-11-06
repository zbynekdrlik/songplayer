//! #289: `audio_low_latency` on a VBAN output — the exact send instants at
//! both settings (L = two slots off, 15 ms on), the wait cap's 4 × L
//! relation, a block handed over after its first packet's 15 ms, and a
//! converting destination's delay (padded to `rate / 60` off, the
//! converter's own on).

use super::tests::{FakeClock, RecordingSink, active_config};
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::audio_out_migrate::entries_from_vban;
use crate::playback::vban_rate::{
    VbanRateConverter, fft_delay_frames, low_latency_delay_frames, prime_frames,
};
use sp_core::audio_outputs::VbanSampleFormat;

const D: i64 = 17_900_000_000_000_000;

fn block(due: i64, samples: Option<Vec<f32>>) -> ProgramBlock {
    ProgramBlock {
        due_100ns: due,
        samples: samples.map(Into::into),
        substituted: false,
    }
}

/// Every packet's send instant for `blocks`, on a fake clock at `start`.
fn instants(out: &VbanOut, blocks: &[ProgramBlock], start: i64) -> Vec<i64> {
    let mut clock = FakeClock::at(start);
    let mut sink = RecordingSink::on(&clock);
    let mut sender = VbanSender::for_out(out);
    for b in blocks {
        sender.send_block(out, b, &mut sink, &mut clock);
    }
    sink.sent.into_iter().map(|(t, _, _)| t).collect()
}

fn foh(low_latency: bool) -> VbanOut {
    let entry = entries_from_vban(true, "sp-program", "fohabl.lan:6980")
        .entries
        .remove(0);
    let out = VbanOut::for_entry_at(&entry, 96_000, low_latency).expect("a FOH output");
    out.set_config(active_config(&["10.0.0.1:6980"]));
    out
}

/// k · 1e7 / 240, floored: the 48 kHz INT24 packet offsets.
const OFFSETS_48K: [i64; 8] = [
    0, 41_666, 83_333, 125_000, 166_666, 208_333, 250_000, 291_666,
];

#[test]
fn the_send_latency_is_two_slots_off_and_15_ms_on() {
    assert_eq!(base_send_latency_100ns(false), 666_666);
    assert_eq!(base_send_latency_100ns(true), 150_000);
    assert_eq!(send_latency_in(0, false), 666_666);
    assert_eq!(send_latency_in(70_000, true), 220_000);
    assert_eq!(send_latency_in(70_000, false), 736_666);
    // The wait cap keeps its 4 × L relation (plus the delay).
    assert_eq!(max_wait_in(0, false), VBAN_MAX_WAIT_100NS);
    assert_eq!(max_wait_in(30_000, false), VBAN_MAX_WAIT_100NS + 30_000);
    assert_eq!(max_wait_in(0, true), 600_000);
    assert_eq!(max_wait_in(30_000, true), 630_000);
    // The planned wait of the last packet of a block taken at its boundary
    // stays under the cap: L + the last offset.
    assert!(150_000 + 291_666 < max_wait_in(0, true));
}

#[test]
fn foh_sends_at_two_slots_off_and_at_15_ms_on() {
    for (low, l) in [(false, 666_666i64), (true, 150_000)] {
        let out = foh(low);
        assert_eq!(out.low_latency(), low);
        let blocks: Vec<ProgramBlock> = (0..3)
            .map(|i| block(D + i * 333_333, Some(vec![0.1; 3200])))
            .collect();
        let sent = instants(&out, &blocks, D);
        let want: Vec<i64> = (0..3)
            .flat_map(|i| OFFSETS_48K.map(|o| D + i * 333_333 + l + o))
            .collect();
        assert_eq!(sent, want, "low_latency {low}");
        assert_eq!(out.status().late_sends, 0, "low_latency {low}");
    }
}

#[test]
fn for_entry_is_the_default_latency() {
    let entry = entries_from_vban(true, "sp-program", "fohabl.lan:6980")
        .entries
        .remove(0);
    let out = VbanOut::for_entry(&entry, 48_000).unwrap();
    assert!(!out.low_latency());
    assert!(!VbanOut::new().low_latency());
}

#[test]
fn a_block_handed_over_20_ms_late_sends_its_past_due_packets_at_once_when_on() {
    // On: the first packet is due 15 ms after the boundary; a block taken
    // 20 ms after it sends packets 0 and 1 (due 15.0 / 19.2 ms) at once —
    // packet 0 over 2 ms late, packet 1 0.8 ms late — then waits for the
    // rest on schedule.
    let out = foh(true);
    let sent = instants(&out, &[block(D, Some(vec![0.1; 3200]))], D + 200_000);
    let mut want = vec![D + 200_000, D + 200_000];
    want.extend(OFFSETS_48K[2..].iter().map(|o| D + 150_000 + o));
    assert_eq!(sent, want);
    assert_eq!(out.status().late_sends, 1);
    // Off, the same block is 46.7 ms early: every packet on schedule.
    let out = foh(false);
    let sent = instants(&out, &[block(D, Some(vec![0.1; 3200]))], D + 200_000);
    assert_eq!(sent, OFFSETS_48K.map(|o| D + 666_666 + o).to_vec());
    assert_eq!(out.status().late_sends, 0);
}

#[test]
fn a_96k_destination_sends_16_packets_from_15_ms_when_on() {
    let entry = OutputEntry {
        delay_ms: 3,
        ..sp_core::audio_outputs::OutputEntry::vban(
            "out-2",
            "FOH 96",
            VbanDest {
                host: "h".into(),
                port: 6980,
                stream_name: "sp".into(),
                format: VbanSampleFormat::Int24,
            },
        )
    };
    let out = VbanOut::for_entry_at(&entry, 96_000, true).unwrap();
    out.set_config(active_config(&["10.0.0.1:6980"]));
    let sent = instants(&out, &[block(D, Some(vec![0.25; 3200]))], D);
    assert_eq!(sent.len(), 16);
    // L + the 3 ms delay, then k · 1e7 / 480 floored.
    assert_eq!(sent[0], D + 150_000 + 30_000);
    assert_eq!(sent[1], D + 180_000 + 20_833);
    assert_eq!(sent[15], D + 180_000 + 312_500);
}

#[test]
fn the_converter_is_padded_to_rate_over_60_only_when_off() {
    for rate in [44_100u32, 88_200, 96_000, 192_000] {
        let off = VbanRateConverter::for_latency(rate, false);
        assert_eq!(off.delay_frames(), fft_delay_frames(rate), "{rate}");
        assert_eq!(
            VbanRateConverter::new(rate).delay_frames(),
            fft_delay_frames(rate)
        );
        assert_eq!(prime_frames(rate, 100, true), 0);
        assert_eq!(prime_frames(rate, 100, false), fft_delay_frames(rate) - 100);
    }
    // On: the converter's own delay, plus the first block's top-up (the
    // converter's first call gives rate / 24 000 frames under a block,
    // dstack v0.6.20).
    for (rate, own, after) in [
        (44_100u32, 117usize, 119usize),
        (88_200, 235, 239),
        (96_000, 256, 260),
        (192_000, 512, 520),
    ] {
        let mut on = VbanRateConverter::for_latency(rate, true);
        assert_eq!(on.delay_frames(), own, "{rate}");
        for _ in 0..30 {
            let out = on.convert(Some(&[0.2f32; 3200][..])).expect("never short");
            assert_eq!(out.len(), rate as usize / 30 * 2, "{rate}");
        }
        assert_eq!(on.delay_frames(), after, "{rate}: one top-up only");
        assert_eq!(low_latency_delay_frames(rate), after, "{rate}");
    }
    assert_eq!(low_latency_delay_frames(48_000), 0);
    let mut pass = VbanRateConverter::for_latency(48_000, true);
    let block = vec![0.3f32; 3200];
    let out = pass.convert(Some(&block[..])).unwrap();
    assert!(
        std::ptr::eq(out.as_ptr(), block.as_ptr()),
        "48 kHz untouched"
    );
}

/// The index of the largest |left| sample of the converted stream.
fn peak_frame(c: &mut VbanRateConverter, blocks: usize) -> usize {
    let mut stream = Vec::new();
    for b in 0..blocks {
        let mut input = vec![0.0f32; 3200];
        if b == 0 {
            input[0] = 1.0;
            input[1] = 1.0;
        }
        stream.extend_from_slice(c.convert(Some(&input[..])).unwrap());
    }
    stream
        .chunks_exact(2)
        .enumerate()
        .max_by(|a, b| a.1[0].abs().total_cmp(&b.1[0].abs()))
        .unwrap()
        .0
}

#[test]
fn an_impulse_leaves_the_converter_after_its_reported_delay() {
    // 96 kHz: input frame 0 sits at output frame 0 of an ideal 2× stream.
    let mut off = VbanRateConverter::for_latency(96_000, false);
    let at = peak_frame(&mut off, 3);
    assert!(at.abs_diff(off.delay_frames()) <= 1, "off: peak {at}");
    assert_eq!(off.delay_frames(), 1_600);
    let mut on = VbanRateConverter::for_latency(96_000, true);
    let at = peak_frame(&mut on, 3);
    assert!(at.abs_diff(on.delay_frames()) <= 1, "on: peak {at}");
    assert_eq!(on.delay_frames(), 260);
}

#[test]
fn the_status_latency_follows_the_switch() {
    use crate::playback::audio_out::vban_latency_ms_at;
    assert_eq!(vban_latency_ms_at(0, 48_000, false), 66.6666);
    assert_eq!(vban_latency_ms_at(0, 48_000, true), 15.0);
    assert_eq!(
        vban_latency_ms_at(5, 96_000, false),
        66.6666 + 5.0 + 1600.0 / 96.0
    );
    assert_eq!(
        vban_latency_ms_at(5, 96_000, true),
        15.0 + 5.0 + 260.0 / 96.0
    );
}
