//! The VBAN audio packet encoder (#210, B2 of EPIC #174). Pure, no I/O.
//!
//! The program's audio is one 1600-frame block of 48 kHz stereo interleaved
//! f32 per 30 fps grid boundary (`sp_ndi::AudioFrame`, the same samples
//! `SP-program` carries). [`VbanEncoder::encode_block`] turns one block into
//! [`VBAN_PACKETS_PER_BLOCK`] packets of [`VBAN_FRAMES_PER_PACKET`] frames each,
//! as PCM INT24, and [`packet_send_at_100ns`] says when each packet goes out, so
//! the sender (`vban_out.rs`) sends one packet every 1/240 s, never in bursts.
//!
//! Wire format, per the VB-Audio "VBAN Protocol Specifications" (revision 13,
//! SEP 2025), little-endian throughout. The 28-byte header is:
//!
//! | bytes | field | value here |
//! |---|---|---|
//! | 0..4 | `'V' 'B' 'A' 'N'` | magic |
//! | 4 | `format_SR` (bits 0–4 SR index, 5–7 sub protocol) | 3 = 48 kHz, AUDIO (0x00) |
//! | 5 | `format_nbs` = samples − 1 | 199 |
//! | 6 | `format_nbc` = channels − 1 | 1 |
//! | 7 | `format_bit` (bits 0–2 data type, bit 3 = 0, 4–7 codec) | 0x02 = INT24, PCM (0x00) |
//! | 8..24 | `streamname[16]`, ASCII, zero padded | e.g. `sp-program` |
//! | 24..28 | `nuFrame`, u32 | +1 per packet |
//!
//! The payload is interleaved `L R L R …` 24-bit two's-complement samples, 3
//! bytes each: 200 × 2 × 3 = 1200 bytes, within the spec's 1436-byte data
//! maximum.

use sp_core::genlock::{GENLOCK_GRID_FPS, UNITS_PER_SECOND};

/// The four magic bytes every VBAN packet starts with.
pub const VBAN_MAGIC: [u8; 4] = *b"VBAN";
/// The VBAN header length.
pub const VBAN_HEADER_LEN: usize = 28;
/// The stream-name field length.
pub const VBAN_STREAM_NAME_LEN: usize = 16;
/// `format_SR`: SR index 3 (48000 Hz in the spec's SR list), sub protocol
/// AUDIO (bits 5–7 = 0).
pub const VBAN_FORMAT_SR_48K_AUDIO: u8 = 3;
/// `format_bit`: data type INT24 (index 2 in bits 0–2), codec PCM (bits 4–7 =
/// 0), reserved bit 3 = 0.
pub const VBAN_FORMAT_BIT_INT24_PCM: u8 = 0x02;
/// The spec's data maximum per packet.
pub const VBAN_DATA_MAX: usize = 1436;

/// The program audio's sample rate.
pub const VBAN_SAMPLE_RATE_HZ: i64 = 48_000;
/// Stereo.
pub const VBAN_CHANNELS: usize = 2;
/// Frames (one sample per channel) per packet.
pub const VBAN_FRAMES_PER_PACKET: usize = 200;
/// Packets per program block (one grid boundary).
pub const VBAN_PACKETS_PER_BLOCK: usize = 8;
/// Frames per program block: 1600 = 48000 / 30.
pub const VBAN_BLOCK_FRAMES: usize = VBAN_FRAMES_PER_PACKET * VBAN_PACKETS_PER_BLOCK;
/// Interleaved f32 samples per program block: 3200.
pub const VBAN_BLOCK_SAMPLES: usize = VBAN_BLOCK_FRAMES * VBAN_CHANNELS;
/// Interleaved samples per packet: 400.
pub const VBAN_SAMPLES_PER_PACKET: usize = VBAN_FRAMES_PER_PACKET * VBAN_CHANNELS;
/// Bytes per INT24 sample.
pub const VBAN_BYTES_PER_SAMPLE: usize = 3;
/// Payload bytes per packet: 1200.
pub const VBAN_PAYLOAD_LEN: usize = VBAN_SAMPLES_PER_PACKET * VBAN_BYTES_PER_SAMPLE;
/// Whole packet: 1228 bytes.
pub const VBAN_PACKET_LEN: usize = VBAN_HEADER_LEN + VBAN_PAYLOAD_LEN;
/// Packets per second: 240 (one every 4.1667 ms).
pub const VBAN_PACKETS_PER_SECOND: i64 = VBAN_SAMPLE_RATE_HZ / VBAN_FRAMES_PER_PACKET as i64;

/// Full scale of the INT24 conversion: `±1.0 → ±8388607` (symmetric).
pub const INT24_FULL_SCALE: i32 = 8_388_607;

/// The fixed send latency L (100 ns): one grid slot after the boundary, so a
/// block is always queued before its first packet is due.
pub const VBAN_SEND_LATENCY_100NS: i64 = UNITS_PER_SECOND / GENLOCK_GRID_FPS;

// A 1600-frame block is exactly one grid slot, and a packet fits the spec.
const _: () = assert!(VBAN_BLOCK_FRAMES as i64 * GENLOCK_GRID_FPS == VBAN_SAMPLE_RATE_HZ);
const _: () = assert!(VBAN_PAYLOAD_LEN <= VBAN_DATA_MAX);

/// One packet on the wire.
pub type VbanPacket = [u8; VBAN_PACKET_LEN];

/// The 8 packets of one block.
pub type VbanBlockPackets = [VbanPacket; VBAN_PACKETS_PER_BLOCK];

/// The stream-name field for `name`: its first 16 characters as ASCII (any
/// non-ASCII or control character becomes `_`), zero padded.
pub fn stream_name_bytes(name: &str) -> [u8; VBAN_STREAM_NAME_LEN] {
    let mut out = [0u8; VBAN_STREAM_NAME_LEN];
    for (dst, c) in out.iter_mut().zip(name.chars()) {
        *dst = if c.is_ascii() && !c.is_ascii_control() {
            c as u8
        } else {
            b'_'
        };
    }
    out
}

/// One f32 sample as a 24-bit integer: clamped to `[-1.0, 1.0]`, scaled by
/// [`INT24_FULL_SCALE`], rounded half away from zero. NaN is silence (0, via
/// Rust's saturating float→int cast).
pub fn f32_to_int24(x: f32) -> i32 {
    let clamped = f64::from(x).clamp(-1.0, 1.0);
    (clamped * f64::from(INT24_FULL_SCALE)).round() as i32
}

/// The 3 little-endian bytes of a 24-bit two's-complement sample.
pub fn int24_le(v: i32) -> [u8; 3] {
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

/// Write the 28-byte header for one packet into `out[..VBAN_HEADER_LEN]`.
pub fn write_header(out: &mut [u8], name: &[u8; VBAN_STREAM_NAME_LEN], counter: u32) {
    out[0..4].copy_from_slice(&VBAN_MAGIC);
    out[4] = VBAN_FORMAT_SR_48K_AUDIO;
    out[5] = (VBAN_FRAMES_PER_PACKET - 1) as u8;
    out[6] = (VBAN_CHANNELS - 1) as u8;
    out[7] = VBAN_FORMAT_BIT_INT24_PCM;
    out[8..24].copy_from_slice(name);
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

/// Offset of packet `k` from its block's first packet (100 ns): `k / 240 s`,
/// floored — 0, 41 666, 83 333, 125 000, …, 291 666.
pub fn packet_offset_100ns(k: usize) -> i64 {
    k as i64 * UNITS_PER_SECOND / VBAN_PACKETS_PER_SECOND
}

/// When packet `k` of the block for the boundary `due_100ns` is sent:
/// `due + latency + k / 240 s`, on the program's wall domain.
pub fn packet_send_at_100ns(due_100ns: i64, latency_100ns: i64, k: usize) -> i64 {
    due_100ns + latency_100ns + packet_offset_100ns(k)
}

/// The VBAN encoder of ONE stream: it owns the frame counter, which grows by
/// exactly 1 per packet, whatever the block carries (a source, a cut, the
/// standby silence), and wraps at `u32::MAX`.
#[derive(Debug, Default)]
pub struct VbanEncoder {
    counter: u32,
}

impl VbanEncoder {
    /// The `nuFrame` the next packet carries.
    pub fn next_counter(&self) -> u32 {
        self.counter
    }

    /// Encode one block into `out`: packet `k` carries interleaved samples
    /// `k·400 .. (k+1)·400` of `samples`, or silence when `samples` is `None`
    /// or not exactly [`VBAN_BLOCK_SAMPLES`] long.
    pub fn encode_block(
        &mut self,
        name: &[u8; VBAN_STREAM_NAME_LEN],
        samples: Option<&[f32]>,
        out: &mut VbanBlockPackets,
    ) {
        let samples = samples.filter(|s| s.len() == VBAN_BLOCK_SAMPLES);
        for (k, packet) in out.iter_mut().enumerate() {
            write_header(&mut packet[..VBAN_HEADER_LEN], name, self.counter);
            self.counter = self.counter.wrapping_add(1);
            let payload = &mut packet[VBAN_HEADER_LEN..];
            match samples {
                Some(s) => {
                    let first = k * VBAN_SAMPLES_PER_PACKET;
                    let chunk = &s[first..first + VBAN_SAMPLES_PER_PACKET];
                    for (dst, &x) in payload.chunks_exact_mut(VBAN_BYTES_PER_SAMPLE).zip(chunk) {
                        dst.copy_from_slice(&int24_le(f32_to_int24(x)));
                    }
                }
                None => payload.fill(0),
            }
        }
    }

    /// Test-only: start the counter at `counter` (the wrap test).
    #[cfg(test)]
    pub fn starting_at(counter: u32) -> Self {
        Self { counter }
    }
}

/// A block's packets, zeroed (the encoder's reusable output buffer).
pub fn empty_block_packets() -> Box<VbanBlockPackets> {
    Box::new([[0u8; VBAN_PACKET_LEN]; VBAN_PACKETS_PER_BLOCK])
}

#[cfg(test)]
#[path = "vban_packet_tests.rs"]
pub(crate) mod tests;
