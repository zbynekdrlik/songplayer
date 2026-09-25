//! Unit tests for the #148 v3 post-seek trim in [`SymphoniaAudioReader`].
//!
//! The public seek behaviour is proven end-to-end in
//! `tests/symphonia_audio.rs` on generated ramp fixtures. symphonia's FLAC
//! demuxer always lands on the packet CONTAINING the target, so a real seek
//! never needs a trim longer than one packet; the multi-packet path is
//! exercised here by arming the trim directly on a real decoder.

use super::*;
// The parent's private `use` aliases are not guaranteed through the glob.
use crate::stream::AudioStream;
use symphonia::core::units::TimeBase;

/// Decode a ramp sample (24-bit `n`, decoded as `n / 2^23`) to its frame index.
fn ramp_index(sample: f32) -> i64 {
    (f64::from(sample) * 8_388_608.0).round() as i64
}

fn open_ramp_4096() -> SymphoniaAudioReader {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ramp_4096.flac");
    SymphoniaAudioReader::open(&path).expect("ramp fixture must open")
}

#[test]
fn ts_to_ms_converts_stream_timestamps() {
    let tb = TimeBase::new(1, 48_000);
    assert_eq!(ts_to_ms(tb, 0), 0);
    assert_eq!(ts_to_ms(tb, 48_000), 1_000);
    // Whole seconds AND a fraction: 1.25 s.
    assert_eq!(ts_to_ms(tb, 60_000), 1_250);
    // Fraction only: 4096 / 48 000 s = 85.33 ms, truncated.
    assert_eq!(ts_to_ms(tb, 4_096), 85);
}

#[test]
fn seek_start_trims_to_the_target_inside_a_block() {
    let tb = TimeBase::new(1, 48_000);
    // seek(1000) on 4096-frame blocks: target 48 000, block starts at 45 056.
    assert_eq!(seek_start(48_000, 45_056, 1_000, tb), (2_944, 1_000));
}

#[test]
fn seek_start_on_a_block_boundary_trims_nothing_and_keeps_the_requested_label() {
    // 44.1 kHz: seek(1) targets frame 44 (1 ms = 44.1 frames, floored), whose
    // exact media time truncates to 0 ms. On a boundary (actual == required) the
    // label stays the REQUESTED 1 ms — never re-derived from the frame.
    let tb = TimeBase::new(1, 44_100);
    assert_eq!(seek_start(44, 44, 1, tb), (0, 1));
}

#[test]
fn seek_start_after_an_overshoot_labels_the_real_first_sample() {
    // The demuxer landed AFTER the target (actual 4096 > required 100): there is
    // nothing before the target to drop, and the first sample is at 85 ms.
    let tb = TimeBase::new(1, 48_000);
    assert_eq!(seek_start(100, 4_096, 2, tb), (0, 85));
}

#[test]
fn trim_leading_frames_spans_several_packets() {
    // Stereo packets of 4 frames each; frame f = [f, -f]. Trim 10 frames.
    let packet = |start: i32| -> Vec<f32> {
        (start..start + 4)
            .flat_map(|f| [f as f32, -(f as f32)])
            .collect()
    };
    let mut skip = 10;

    let mut p0 = packet(0);
    trim_leading_frames(&mut p0, 2, &mut skip);
    assert!(p0.is_empty(), "packet 0 is wholly pre-target");
    assert_eq!(skip, 6);

    let mut p1 = packet(4);
    trim_leading_frames(&mut p1, 2, &mut skip);
    assert!(p1.is_empty(), "packet 1 is wholly pre-target");
    assert_eq!(skip, 2);

    let mut p2 = packet(8);
    trim_leading_frames(&mut p2, 2, &mut skip);
    assert_eq!(p2, vec![10.0, -10.0, 11.0, -11.0], "frames 8,9 dropped");
    assert_eq!(skip, 0);

    let mut p3 = packet(12);
    trim_leading_frames(&mut p3, 2, &mut skip);
    assert_eq!(p3, packet(12), "no trim once the skip is spent");
    assert_eq!(skip, 0);
}

#[test]
fn trim_leading_frames_exactly_one_packet_and_mono() {
    let mut skip = 3;
    let mut mono = vec![0.0, 1.0, 2.0];
    trim_leading_frames(&mut mono, 1, &mut skip);
    assert!(mono.is_empty());
    assert_eq!(skip, 0);

    let mut skip = 1;
    let mut mono = vec![0.0, 1.0, 2.0];
    trim_leading_frames(&mut mono, 1, &mut skip);
    assert_eq!(mono, vec![1.0, 2.0]);
    assert_eq!(skip, 0);
}

/// The decode loop honours a trim that spans more than one real FLAC packet:
/// armed for 10 000 frames from the start of the 4096-block ramp, it drops two
/// whole packets plus 1808 frames of the third, emits frame 10 000 first, labels
/// that chunk with the armed time, and never emits an empty chunk.
#[test]
fn decode_drops_a_trim_spanning_several_packets() {
    let mut reader = open_ramp_4096();
    reader.skip_frames = 10_000;
    reader.pending_seek_ts_ms = Some(208);

    let first = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("a chunk after the trim");
    assert_eq!(first.timestamp_ms, 208, "armed label on the first chunk");
    assert_eq!(
        first.data.len(),
        (3 * 4096 - 10_000) * 2,
        "rest of packet 2"
    );
    assert_eq!(
        ramp_index(first.data[0]),
        10_000,
        "first frame is the target"
    );
    assert_eq!(ramp_index(first.data[1]), -10_000);
    assert_eq!(reader.skip_frames, 0);

    // The next packet is untouched and keeps its own block-start label.
    let next = reader
        .next_samples()
        .expect("decode should succeed")
        .expect("next packet");
    assert_eq!(next.data.len(), 4096 * 2);
    assert_eq!(ramp_index(next.data[0]), 12_288);
    assert_eq!(next.timestamp_ms, 256);
}
