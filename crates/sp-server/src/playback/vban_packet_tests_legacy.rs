//! #233 lane 1: the #210 encoder exactly as it shipped in 0.72.0
//! (`vban_packet.rs` at c504b51f, constants inlined), kept as a TEST ORACLE.
//! FOH's packets must stay byte for byte what they were before the output
//! list; this file never calls the production encoder to build its bytes.

use super::VbanEncoder;
use super::VbanFormat;
use super::stream_name_bytes;

fn legacy_f32_to_int24(x: f32) -> i32 {
    let clamped = f64::from(x).clamp(-1.0, 1.0);
    (clamped * f64::from(8_388_607)).round() as i32
}

fn legacy_header(out: &mut [u8], name: &[u8; 16], counter: u32) {
    out[0..4].copy_from_slice(b"VBAN");
    out[4] = 3;
    out[5] = 199;
    out[6] = 1;
    out[7] = 0x02;
    out[8..24].copy_from_slice(name);
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

/// The 8 packets 0.72.0 sent for one block, advancing `counter` by 8.
pub(crate) fn legacy_encode_block(
    counter: &mut u32,
    name: &[u8; 16],
    samples: Option<&[f32]>,
) -> Vec<[u8; 1228]> {
    let samples = samples.filter(|s| s.len() == 3200);
    (0..8)
        .map(|k| {
            let mut packet = [0u8; 1228];
            legacy_header(&mut packet[..28], name, *counter);
            *counter = counter.wrapping_add(1);
            let payload = &mut packet[28..];
            match samples {
                Some(s) => {
                    let chunk = &s[k * 400..k * 400 + 400];
                    for (dst, &x) in payload.chunks_exact_mut(3).zip(chunk) {
                        let b = legacy_f32_to_int24(x).to_le_bytes();
                        dst.copy_from_slice(&[b[0], b[1], b[2]]);
                    }
                }
                None => payload.fill(0),
            }
            packet
        })
        .collect()
}

/// Blocks that exercise every INT24 branch: a ramp, clamps, NaN, tiny values,
/// a sine, a wrong length, silence.
pub(crate) fn oracle_blocks() -> Vec<Option<Vec<f32>>> {
    let ramp: Vec<f32> = (0..3200)
        .map(|i| (i % 2000) as f32 / 1000.0 - 1.0)
        .collect();
    let mut hot = vec![0.5f32; 3200];
    hot[0] = 1.2;
    hot[1] = -1.2;
    hot[2] = f32::NAN;
    hot[3] = 1e-9;
    hot[4] = -1e-9;
    hot[5] = 1.0;
    hot[6] = -1.0;
    let sine: Vec<f32> = (0..3200)
        .map(|i| (((i / 2) as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.9)
        .collect();
    vec![
        Some(ramp),
        Some(hot),
        Some(sine),
        Some(vec![0.25; 3199]),
        None,
    ]
}

#[test]
fn the_program_format_is_byte_for_byte_the_0_72_encoder() {
    let name = stream_name_bytes("sp-program");
    assert_eq!(&name, b"sp-program\0\0\0\0\0\0");
    for start in [0u32, u32::MAX - 3] {
        let mut legacy_counter = start;
        let mut enc = VbanEncoder::starting_at(start);
        for block in oracle_blocks() {
            let want: Vec<u8> =
                legacy_encode_block(&mut legacy_counter, &name, block.as_deref()).concat();
            let mut got = vec![0u8; 8 * 1228];
            enc.encode_into(VbanFormat::PROGRAM, &name, block.as_deref(), &mut got);
            assert_eq!(got, want, "start {start}");
        }
        assert_eq!(enc.next_counter(), legacy_counter);
    }
}

#[test]
fn encode_block_is_the_program_format() {
    let name = stream_name_bytes("sp-program");
    let mut a = VbanEncoder::default();
    let mut b = VbanEncoder::default();
    for block in oracle_blocks() {
        let mut packets = super::empty_block_packets();
        a.encode_block(&name, block.as_deref(), &mut packets);
        let mut flat = vec![0u8; 8 * 1228];
        b.encode_into(VbanFormat::PROGRAM, &name, block.as_deref(), &mut flat);
        assert_eq!(packets.as_flattened(), &flat[..]);
    }
}
