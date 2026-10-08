//! #233: a VBAN destination's wire format — the SR index per rate, the
//! format bit per sample type, the packet geometry (the largest divisor of
//! `rate/30` within 256 frames and the 1436-byte payload), the encoders, the
//! schedule. Pins derived with a scratch model (every rate × format).

use super::tests::{parse_packet, ramp_block};
use super::*;
use sp_core::audio_outputs::VbanSampleFormat::{self, Float32, Int16, Int24};

struct Geometry {
    rate: u32,
    sample: VbanSampleFormat,
    frames: usize,
    packets: usize,
    len: usize,
}

const fn g(
    rate: u32,
    sample: VbanSampleFormat,
    frames: usize,
    packets: usize,
    len: usize,
) -> Geometry {
    Geometry {
        rate,
        sample,
        frames,
        packets,
        len,
    }
}

const TABLE: [Geometry; 15] = [
    g(44_100, Int16, 245, 6, 1008),
    g(44_100, Int24, 210, 7, 1288),
    g(44_100, Float32, 147, 10, 1204),
    g(48_000, Int16, 200, 8, 828),
    g(48_000, Int24, 200, 8, 1228),
    g(48_000, Float32, 160, 10, 1308),
    g(88_200, Int16, 245, 12, 1008),
    g(88_200, Int24, 210, 14, 1288),
    g(88_200, Float32, 147, 20, 1204),
    g(96_000, Int16, 200, 16, 828),
    g(96_000, Int24, 200, 16, 1228),
    g(96_000, Float32, 160, 20, 1308),
    g(192_000, Int16, 256, 25, 1052),
    g(192_000, Int24, 200, 32, 1228),
    g(192_000, Float32, 160, 40, 1308),
];

#[test]
fn every_rate_and_format_has_its_packet_geometry() {
    for t in TABLE {
        let f = VbanFormat::new(t.rate, t.sample).unwrap();
        assert_eq!((f.rate_hz(), f.sample()), (t.rate, t.sample));
        assert_eq!(f.block_frames() as u32 * 30, t.rate);
        assert_eq!(f.packet_frames(), t.frames, "{} {:?}", t.rate, t.sample);
        assert_eq!(
            f.packets_per_block(),
            t.packets,
            "{} {:?}",
            t.rate,
            t.sample
        );
        assert_eq!(f.packet_len(), t.len, "{} {:?}", t.rate, t.sample);
        assert!(f.packet_len() - VBAN_HEADER_LEN <= VBAN_DATA_MAX);
        assert!(f.packet_frames() <= VBAN_MAX_FRAMES_PER_PACKET);
    }
    assert_eq!(VbanFormat::new(48_000, Int24).unwrap(), VbanFormat::PROGRAM);
    assert_eq!(VbanFormat::PROGRAM.packet_len(), VBAN_PACKET_LEN);
    assert_eq!(
        VbanFormat::PROGRAM.packets_per_block(),
        VBAN_PACKETS_PER_BLOCK
    );
}

#[test]
fn the_sr_index_and_format_bit_follow_the_spec() {
    assert_eq!(sr_index_of(48_000), Some(3));
    assert_eq!(sr_index_of(96_000), Some(4));
    assert_eq!(sr_index_of(192_000), Some(5));
    assert_eq!(sr_index_of(44_100), Some(16));
    assert_eq!(sr_index_of(88_200), Some(17));
    assert_eq!(sr_index_of(32_000), None);
    assert_eq!(
        VbanFormat::new(32_000, Int24).unwrap_err(),
        "VBAN carries no 32000 Hz"
    );
    assert_eq!(VbanFormat::new(96_000, Int24).unwrap().sr_index(), 4);
    assert_eq!(format_bit_of(Int16), 0x01);
    assert_eq!(format_bit_of(Int24), 0x02);
    assert_eq!(format_bit_of(Float32), 0x04);
    assert_eq!(VbanFormat::new(48_000, Float32).unwrap().format_bit(), 0x04);
    assert_eq!(
        (bytes_of(Int16), bytes_of(Int24), bytes_of(Float32)),
        (2, 3, 4)
    );
    assert_eq!(
        VbanFormat::new(48_000, Int16).unwrap().bytes_per_sample(),
        2
    );
}

#[test]
fn the_divisor_and_payload_caps_at_their_edges() {
    assert_eq!(max_packet_frames(2), 256, "1436/4 = 359, capped at 256");
    assert_eq!(max_packet_frames(3), 239);
    assert_eq!(max_packet_frames(4), 179);
    assert_eq!(largest_divisor_at_most(1600, 239), 200);
    assert_eq!(largest_divisor_at_most(1600, 200), 200);
    assert_eq!(largest_divisor_at_most(1600, 199), 160);
    assert_eq!(largest_divisor_at_most(6400, 256), 256);
    assert_eq!(largest_divisor_at_most(7, 3), 1);
    assert_eq!(largest_divisor_at_most(6, 9), 6);
    assert_eq!(largest_divisor_at_most(0, 5), 1);
}

#[test]
fn headers_carry_each_destinations_rate_and_format() {
    let name = stream_name_bytes("x");
    let header = |rate, sample| {
        let mut h = [0u8; VBAN_HEADER_LEN];
        write_header_as(VbanFormat::new(rate, sample).unwrap(), &mut h, &name, 5);
        h
    };
    let h = header(96_000, Int24);
    assert_eq!(&h[0..4], b"VBAN");
    assert_eq!((h[4], h[5], h[6], h[7]), (4, 199, 1, 0x02));
    let h = header(44_100, Int24);
    assert_eq!((h[4], h[5]), (16, 209));
    let h = header(88_200, Int16);
    assert_eq!((h[4], h[5], h[7]), (17, 244, 0x01));
    let h = header(192_000, Int16);
    assert_eq!((h[4], h[5], h[7]), (5, 255, 0x01));
    let h = header(48_000, Float32);
    assert_eq!((h[4], h[5], h[7]), (3, 159, 0x04));
    assert_eq!(&h[8..24], &name);
    assert_eq!(&h[24..28], &5u32.to_le_bytes());
}

#[test]
fn int16_and_float32_samples_are_exact() {
    assert_eq!(f32_to_int16(1.0), 32_767);
    assert_eq!(f32_to_int16(-1.0), -32_767);
    assert_eq!(f32_to_int16(0.5), 16_384, "16383.5 rounds away from zero");
    assert_eq!(f32_to_int16(-0.25), -8_192);
    assert_eq!(f32_to_int16(2.0), 32_767);
    assert_eq!(f32_to_int16(-2.0), -32_767);
    assert_eq!(f32_to_int16(f32::NAN), 0);
    assert_eq!(clean_f32(1.5), 1.0);
    assert_eq!(clean_f32(-1.5), -1.0);
    assert_eq!(clean_f32(-0.25), -0.25);
    assert_eq!(clean_f32(f32::NAN), 0.0);
    assert_eq!(clean_f32(f32::INFINITY), 0.0);
    assert_eq!(clean_f32(f32::NEG_INFINITY), 0.0);
}

#[test]
fn a_96k_block_is_16_packets_of_200_frames_in_order() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let block: Vec<f32> = ramp_block()
        .iter()
        .chain(ramp_block().iter())
        .copied()
        .collect();
    let mut out = empty_packets(f);
    assert_eq!(out.len(), 16 * 1228);
    let mut enc = VbanEncoder::default();
    enc.encode_into(f, &stream_name_bytes("sp-e2e-96k"), Some(&block), &mut out);
    for (k, packet) in out.chunks_exact(1228).enumerate() {
        let p = parse_packet(packet);
        assert_eq!((p.format_sr, p.nbs, p.counter), (4, 199, k as u32));
        let want: Vec<i32> = block[k * 400..k * 400 + 400]
            .iter()
            .map(|&x| f32_to_int24(x))
            .collect();
        assert_eq!(p.samples, want);
    }
    assert_eq!(enc.next_counter(), 16);
}

#[test]
fn float32_and_int16_payloads_are_little_endian_and_interleaved() {
    let f = VbanFormat::new(48_000, Float32).unwrap();
    let mut out = empty_packets(f);
    let block: Vec<f32> = (0..3200).map(|i| i as f32 / 4000.0).collect();
    VbanEncoder::default().encode_into(f, &stream_name_bytes("x"), Some(&block), &mut out);
    assert_eq!(out.len(), 10 * 1308);
    assert_eq!(&out[28..32], &0.0f32.to_le_bytes());
    assert_eq!(&out[32..36], &(1.0f32 / 4000.0).to_le_bytes());
    // packet 1 starts with sample 320 (160 frames × 2)
    assert_eq!(
        &out[1308 + 28..1308 + 32],
        &(320.0f32 / 4000.0).to_le_bytes()
    );
    let f = VbanFormat::new(48_000, Int16).unwrap();
    let mut out = empty_packets(f);
    VbanEncoder::default().encode_into(f, &stream_name_bytes("x"), Some(&[0.5; 3200]), &mut out);
    assert_eq!(out.len(), 8 * 828);
    assert_eq!(&out[28..30], &16_384i16.to_le_bytes());
    assert_eq!(
        &out[826..828],
        &16_384i16.to_le_bytes(),
        "the last sample of packet 0"
    );
}

#[test]
fn a_wrong_length_block_is_silence_at_any_rate() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let mut out = vec![0xAAu8; f.packet_len() * f.packets_per_block()];
    let mut enc = VbanEncoder::default();
    enc.encode_into(f, &stream_name_bytes("x"), Some(&[0.5; 3200]), &mut out);
    assert!(
        out.chunks_exact(1228)
            .all(|p| p[28..].iter().all(|&b| b == 0))
    );
    assert!(out.chunks_exact(1228).all(|p| &p[0..4] == b"VBAN"));
    enc.encode_into(f, &stream_name_bytes("x"), None, &mut out);
    assert_eq!(enc.next_counter(), 32, "silence still counts its packets");
}

#[test]
fn packets_are_spread_evenly_over_the_slot() {
    let f = VbanFormat::new(96_000, Int24).unwrap();
    let offsets: Vec<i64> = (0..=16).map(|k| packet_offset_in(f, k)).collect();
    assert_eq!(&offsets[..4], &[0, 20_833, 41_666, 62_500]);
    assert_eq!(offsets[16], 333_333, "one grid slot");
    for k in 0..=8 {
        assert_eq!(
            packet_offset_in(VbanFormat::PROGRAM, k),
            packet_offset_100ns(k)
        );
    }
    assert_eq!(packet_send_at_in(f, 1_000, 7, 2), 1_000 + 7 + 41_666);
    assert_eq!(
        packet_send_at_100ns(1_000, 7, 2),
        packet_send_at_in(VbanFormat::PROGRAM, 1_000, 7, 2)
    );
}
