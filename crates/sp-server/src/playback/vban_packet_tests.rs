//! #210 VBAN encoder: the header bytes against the VB-Audio spec (revision 13),
//! the bit-exact INT24 conversion, the 1600-frame block → 8 × 200-frame split,
//! the frame counter and the packet schedule. `parse_packet` is shared with
//! `vban_out_tests.rs` and `api/program_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "vban_packet_tests.rs"] pub(crate) mod tests;`.

use super::*;

/// One decoded VBAN packet.
#[derive(Debug)]
pub(crate) struct Parsed {
    pub format_sr: u8,
    pub nbs: u8,
    pub nbc: u8,
    pub format_bit: u8,
    pub name: [u8; VBAN_STREAM_NAME_LEN],
    pub counter: u32,
    /// The payload's 24-bit samples, sign-extended.
    pub samples: Vec<i32>,
}

/// Decode one packet as a VBAN receiver would (independent of the encoder).
pub(crate) fn parse_packet(p: &[u8]) -> Parsed {
    assert_eq!(&p[0..4], b"VBAN", "magic");
    assert_eq!((p.len() - 28) % 3, 0, "whole INT24 samples");
    let mut name = [0u8; 16];
    name.copy_from_slice(&p[8..24]);
    Parsed {
        format_sr: p[4],
        nbs: p[5],
        nbc: p[6],
        format_bit: p[7],
        name,
        counter: u32::from_le_bytes([p[24], p[25], p[26], p[27]]),
        samples: p[28..]
            .chunks_exact(3)
            .map(|b| {
                let sign = if b[2] & 0x80 != 0 { 0xFF } else { 0x00 };
                i32::from_le_bytes([b[0], b[1], b[2], sign])
            })
            .collect(),
    }
}

/// A block whose every sample converts to a distinct-ish INT24 value, over
/// the whole `[-1, 1)` range.
pub(crate) fn ramp_block() -> Vec<f32> {
    (0..VBAN_BLOCK_SAMPLES)
        .map(|i| (i % 2000) as f32 / 1000.0 - 1.0)
        .collect()
}

#[test]
fn the_packet_geometry_is_48k_stereo_200_frames_int24() {
    assert_eq!(VBAN_HEADER_LEN, 28);
    assert_eq!(VBAN_BLOCK_FRAMES, 1600);
    assert_eq!(VBAN_BLOCK_SAMPLES, 3200);
    assert_eq!(VBAN_SAMPLES_PER_PACKET, 400);
    assert_eq!(VBAN_PAYLOAD_LEN, 1200);
    assert_eq!(VBAN_PACKET_LEN, 1228);
    assert_eq!(VBAN_PACKETS_PER_SECOND, 240);
    assert_eq!(VBAN_SEND_LATENCY_100NS, 333_333, "L = one grid slot");
}

#[test]
fn the_header_is_the_vban_spec_layout_little_endian() {
    let mut h = [0xAAu8; VBAN_HEADER_LEN];
    write_header(&mut h, &stream_name_bytes("sp-program"), 0x0102_0304);
    let expected: [u8; 28] = [
        b'V', b'B', b'A', b'N', //
        0x03, // SR index 3 = 48 kHz, sub protocol AUDIO (0x00)
        199,  // 200 samples − 1
        1,    // 2 channels − 1
        0x02, // INT24, reserved bit 0, codec PCM (0x00)
        b's', b'p', b'-', b'p', b'r', b'o', b'g', b'r', b'a', b'm', 0, 0, 0, 0, 0, 0, //
        0x04, 0x03, 0x02, 0x01, // nuFrame, little-endian
    ];
    assert_eq!(h, expected);
}

#[test]
fn the_stream_name_is_16_ascii_bytes_zero_padded() {
    assert_eq!(&stream_name_bytes("sp-program"), b"sp-program\0\0\0\0\0\0");
    assert_eq!(
        &stream_name_bytes("a-very-long-stream-name"),
        b"a-very-long-stre",
        "truncated to 16"
    );
    assert_eq!(
        &stream_name_bytes("é\tx"),
        b"__x\0\0\0\0\0\0\0\0\0\0\0\0\0",
        "non-ASCII and control characters become _"
    );
    assert_eq!(stream_name_bytes(""), [0u8; 16]);
}

#[test]
fn int24_conversion_is_bit_exact_and_clamps() {
    assert_eq!(f32_to_int24(0.0), 0);
    assert_eq!(f32_to_int24(1.0), 8_388_607, "+1.0 = +full scale");
    assert_eq!(f32_to_int24(-1.0), -8_388_607, "-1.0 = -full scale");
    assert_eq!(f32_to_int24(1.5), 8_388_607, "> 1.0 clamps");
    assert_eq!(f32_to_int24(-2.0), -8_388_607, "< -1.0 clamps");
    assert_eq!(f32_to_int24(f32::INFINITY), 8_388_607);
    assert_eq!(f32_to_int24(f32::NEG_INFINITY), -8_388_607);
    assert_eq!(f32_to_int24(f32::NAN), 0, "NaN is silence");
    assert_eq!(f32_to_int24(0.5), 4_194_304, "4194303.5 rounds away from 0");
    assert_eq!(f32_to_int24(-0.5), -4_194_304);
    assert_eq!(f32_to_int24(0.25), 2_097_152);
    assert_eq!(f32_to_int24(-0.25), -2_097_152);
    assert_eq!(f32_to_int24(1.0 / 8_388_607.0), 1, "one LSB");

    assert_eq!(int24_le(8_388_607), [0xFF, 0xFF, 0x7F]);
    assert_eq!(int24_le(-8_388_607), [0x01, 0x00, 0x80]);
    assert_eq!(int24_le(-1), [0xFF, 0xFF, 0xFF]);
    assert_eq!(int24_le(0x12_3456), [0x56, 0x34, 0x12]);
    assert_eq!(int24_le(f32_to_int24(1.0)), [0xFF, 0xFF, 0x7F]);
    assert_eq!(int24_le(f32_to_int24(-1.0)), [0x01, 0x00, 0x80]);
}

#[test]
fn a_block_splits_into_8_packets_whose_pcm_concatenates_to_the_block() {
    let block = ramp_block();
    let mut enc = VbanEncoder::default();
    let mut out = empty_block_packets();
    enc.encode_block(&stream_name_bytes("sp-program"), Some(&block), &mut out);
    assert_eq!(out.len(), 8);
    let mut pcm = Vec::new();
    for (k, packet) in out.iter().enumerate() {
        assert_eq!(packet.len(), 1228);
        let p = parse_packet(packet);
        assert_eq!((p.format_sr, p.nbs, p.nbc, p.format_bit), (3, 199, 1, 0x02));
        assert_eq!(&p.name, b"sp-program\0\0\0\0\0\0");
        assert_eq!(p.counter, k as u32);
        assert_eq!(p.samples.len(), 400, "200 stereo frames");
        pcm.extend(p.samples);
    }
    let expected: Vec<i32> = block.iter().map(|&x| f32_to_int24(x)).collect();
    assert_eq!(pcm, expected, "8 × 400 samples, in order, bit-exact");
    assert_eq!(pcm[0], -8_388_607, "the ramp starts at -1.0");
    assert_eq!(pcm[1999], 8_380_218, "0.999 → 8380218");
    assert_eq!(enc.next_counter(), 8);
}

#[test]
fn silence_and_a_wrong_length_block_encode_zero_pcm_and_keep_counting() {
    let name = stream_name_bytes("sp-program");
    let mut enc = VbanEncoder::default();
    let mut out = empty_block_packets();
    enc.encode_block(&name, Some(&ramp_block()), &mut out);
    enc.encode_block(&name, None, &mut out);
    for (k, packet) in out.iter().enumerate() {
        let p = parse_packet(packet);
        assert_eq!(p.counter, 8 + k as u32);
        assert!(p.samples.iter().all(|&s| s == 0), "silence");
    }
    let short = vec![0.3f32; VBAN_BLOCK_SAMPLES - 2];
    enc.encode_block(&name, Some(&short), &mut out);
    for (k, packet) in out.iter().enumerate() {
        let p = parse_packet(packet);
        assert_eq!(p.counter, 16 + k as u32);
        assert!(p.samples.iter().all(|&s| s == 0), "not a block → silence");
    }
    assert_eq!(enc.next_counter(), 24);
}

#[test]
fn the_frame_counter_wraps_at_u32_max() {
    let mut enc = VbanEncoder::starting_at(u32::MAX - 3);
    let mut out = empty_block_packets();
    enc.encode_block(&stream_name_bytes("x"), None, &mut out);
    let counters: Vec<u32> = out.iter().map(|p| parse_packet(p).counter).collect();
    assert_eq!(
        counters,
        vec![
            u32::MAX - 3,
            u32::MAX - 2,
            u32::MAX - 1,
            u32::MAX,
            0,
            1,
            2,
            3
        ]
    );
    assert_eq!(enc.next_counter(), 4);
}

#[test]
fn packet_k_is_due_at_the_boundary_plus_l_plus_k_240ths_in_100ns() {
    let offsets: Vec<i64> = (0..=8).map(packet_offset_100ns).collect();
    assert_eq!(
        offsets,
        vec![
            0, 41_666, 83_333, 125_000, 166_666, 208_333, 250_000, 291_666, 333_333
        ],
        "k·4.1667 ms floored; packet 8 would be the next slot's packet 0"
    );
    let due = 17_900_000_000_000_000i64;
    for (k, off) in offsets.iter().take(8).enumerate() {
        assert_eq!(
            packet_send_at_100ns(due, VBAN_SEND_LATENCY_100NS, k),
            due + 333_333 + off
        );
    }
    assert_eq!(packet_send_at_100ns(1_000, 7, 3), 1_000 + 7 + 125_000);
}
