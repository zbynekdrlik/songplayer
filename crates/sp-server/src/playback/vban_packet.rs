//! The VBAN audio packet encoder (#210, B2 of EPIC #174). Pure, no I/O.
//!
//! The program's audio is one 1600-frame block of 48 kHz stereo interleaved
//! f32 per 30 fps grid boundary (`sp_ndi::AudioFrame`, the same samples
//! `SP-program` carries). #233: [`VbanEncoder::encode_into`] turns one block
//! (converted to its destination's rate first, `vban_rate.rs`) into that
//! destination's packets ([`VbanFormat`]: 8 packets of 200 frames as PCM INT24
//! at 48 kHz, FOH's), and [`packet_send_at_in`] says when each one goes out,
//! so the sender (`vban_out.rs`) sends an on-time packet every
//! `1 / (30 · packets)` s, 1/240 s at 48 kHz INT24 (a late block's past-due
//! packets go out back-to-back and count as late sends). #210's 48 kHz INT24
//! entry points (`encode_block`, `write_header`, `packet_offset_100ns`,
//! `packet_send_at_100ns`, `empty_block_packets`) are test-only wrappers over
//! the `PROGRAM` format, so #210's own tests still run unchanged against the
//! 0.72.0 bytes.
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
//!
//! #233: every destination has its own format ([`VbanFormat`]): the SR index
//! of its rate (48 kHz = 3, 96 = 4, 192 = 5, 44.1 = 16, 88.2 = 17), its sample
//! type (INT16 0x01, INT24 0x02, FLOAT32 0x04) and its packet geometry: the
//! largest divisor of `rate/30` frames within the spec's 256 frames and
//! 1436-byte payload, so one boundary's block is whole packets, sent evenly
//! over the slot. [`VbanFormat::PROGRAM`] (48 kHz INT24, 8 × 200) is #210's
//! format, byte for byte (`vban_packet_tests_legacy.rs`, a copy of the 0.72.0
//! encoder).

use sp_core::audio_outputs::VbanSampleFormat;
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

/// #233: the spec's frames-per-packet maximum (`format_nbs` = samples − 1, a byte).
pub const VBAN_MAX_FRAMES_PER_PACKET: usize = 256;
/// #233: `format_bit` INT16 (data type index 1), codec PCM.
pub const VBAN_FORMAT_BIT_INT16_PCM: u8 = 0x01;
/// #233: `format_bit` FLOAT32 (data type index 4), codec PCM.
pub const VBAN_FORMAT_BIT_FLOAT32_PCM: u8 = 0x04;
/// #233: full scale of the INT16 conversion: `±1.0 → ±32767` (symmetric, like INT24).
pub const INT16_FULL_SCALE: i16 = 32_767;

/// The fixed send latency L (100 ns): two grid slots after the boundary, so a
/// block is always queued before its first packet is due. A program block
/// reaches VBAN only after its source's submit and the `SP-program` NDI
/// submit (each up to ~20 ms p99); one slot (33 ms) left 0.5 % of the
/// packets late and VB-Matrix at FOH counted overload/underrun bursts
/// (26.9.2026 box capture).
pub const VBAN_SEND_LATENCY_100NS: i64 = 2 * UNITS_PER_SECOND / GENLOCK_GRID_FPS;

// A 1600-frame block is exactly one grid slot, and a packet fits the spec.
const _: () = assert!(VBAN_BLOCK_FRAMES as i64 * GENLOCK_GRID_FPS == VBAN_SAMPLE_RATE_HZ);
const _: () = assert!(VBAN_PAYLOAD_LEN <= VBAN_DATA_MAX);

/// One packet on the wire.
pub type VbanPacket = [u8; VBAN_PACKET_LEN];

/// The 8 packets of one block (#210's 48 kHz INT24 layout; test-only).
#[cfg(test)]
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

/// #233: the SR index of a rate SongPlayer sends (spec rev. 13, p. 8).
pub fn sr_index_of(rate_hz: u32) -> Option<u8> {
    match rate_hz {
        48_000 => Some(VBAN_FORMAT_SR_48K_AUDIO),
        96_000 => Some(4),
        192_000 => Some(5),
        44_100 => Some(16),
        88_200 => Some(17),
        _ => None,
    }
}

/// #233: the `format_bit` of a sample type (codec PCM).
pub fn format_bit_of(sample: VbanSampleFormat) -> u8 {
    match sample {
        VbanSampleFormat::Int16 => VBAN_FORMAT_BIT_INT16_PCM,
        VbanSampleFormat::Int24 => VBAN_FORMAT_BIT_INT24_PCM,
        VbanSampleFormat::Float32 => VBAN_FORMAT_BIT_FLOAT32_PCM,
    }
}

/// #233: the bytes of one sample of a type.
pub fn bytes_of(sample: VbanSampleFormat) -> usize {
    match sample {
        VbanSampleFormat::Int16 => 2,
        VbanSampleFormat::Int24 => VBAN_BYTES_PER_SAMPLE,
        VbanSampleFormat::Float32 => 4,
    }
}

/// #233: the most stereo frames one packet may carry at `bytes_per_sample`.
pub fn max_packet_frames(bytes_per_sample: usize) -> usize {
    (VBAN_DATA_MAX / (VBAN_CHANNELS * bytes_per_sample)).min(VBAN_MAX_FRAMES_PER_PACKET)
}

/// #233: the largest divisor of `n` that is at most `cap` (1 when there is none).
pub fn largest_divisor_at_most(n: usize, cap: usize) -> usize {
    (1..=cap.min(n)).rev().find(|d| n % d == 0).unwrap_or(1)
}

/// #233: one VBAN destination's wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VbanFormat {
    rate_hz: u32,
    sr_index: u8,
    sample: VbanSampleFormat,
}

impl VbanFormat {
    /// #210's format (FOH's): 48 kHz INT24, 8 packets of 200 frames.
    pub const PROGRAM: Self = Self {
        rate_hz: 48_000,
        sr_index: VBAN_FORMAT_SR_48K_AUDIO,
        sample: VbanSampleFormat::Int24,
    };

    /// A destination's format; a rate with no SR index is refused.
    pub fn new(rate_hz: u32, sample: VbanSampleFormat) -> Result<Self, String> {
        let sr_index =
            sr_index_of(rate_hz).ok_or_else(|| format!("VBAN carries no {rate_hz} Hz"))?;
        Ok(Self {
            rate_hz,
            sr_index,
            sample,
        })
    }

    pub fn rate_hz(self) -> u32 {
        self.rate_hz
    }

    pub fn sample(self) -> VbanSampleFormat {
        self.sample
    }

    pub fn sr_index(self) -> u8 {
        self.sr_index
    }

    pub fn format_bit(self) -> u8 {
        format_bit_of(self.sample)
    }

    pub fn bytes_per_sample(self) -> usize {
        bytes_of(self.sample)
    }

    /// Stereo frames per program boundary at this rate (`rate / 30`).
    pub fn block_frames(self) -> usize {
        (i64::from(self.rate_hz) / GENLOCK_GRID_FPS) as usize
    }

    pub fn packet_frames(self) -> usize {
        largest_divisor_at_most(
            self.block_frames(),
            max_packet_frames(self.bytes_per_sample()),
        )
    }

    pub fn packets_per_block(self) -> usize {
        self.block_frames() / self.packet_frames()
    }

    pub fn packet_len(self) -> usize {
        VBAN_HEADER_LEN + self.packet_frames() * VBAN_CHANNELS * self.bytes_per_sample()
    }
}

/// #233: one f32 sample as INT16: clamped, scaled by [`INT16_FULL_SCALE`],
/// rounded half away from zero; NaN is silence.
pub fn f32_to_int16(x: f32) -> i16 {
    (f64::from(x).clamp(-1.0, 1.0) * f64::from(INT16_FULL_SCALE)).round() as i16
}

/// #233: one f32 sample as VBAN FLOAT32 carries it: clamped to ±1, a
/// non-finite value as silence.
pub fn clean_f32(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn write_sample(sample: VbanSampleFormat, dst: &mut [u8], x: f32) {
    match sample {
        VbanSampleFormat::Int16 => dst.copy_from_slice(&f32_to_int16(x).to_le_bytes()),
        VbanSampleFormat::Int24 => dst.copy_from_slice(&int24_le(f32_to_int24(x))),
        VbanSampleFormat::Float32 => dst.copy_from_slice(&clean_f32(x).to_le_bytes()),
    }
}

/// #233: the 28-byte header of one packet in `fmt`.
pub fn write_header_as(
    fmt: VbanFormat,
    out: &mut [u8],
    name: &[u8; VBAN_STREAM_NAME_LEN],
    counter: u32,
) {
    out[0..4].copy_from_slice(&VBAN_MAGIC);
    out[4] = fmt.sr_index();
    out[5] = (fmt.packet_frames() - 1) as u8;
    out[6] = (VBAN_CHANNELS - 1) as u8;
    out[7] = fmt.format_bit();
    out[8..24].copy_from_slice(name);
    out[24..28].copy_from_slice(&counter.to_le_bytes());
}

/// Write the 28-byte header for one packet into `out[..VBAN_HEADER_LEN]`
/// (= the `PROGRAM` format; #210's tests).
#[cfg(test)]
pub fn write_header(out: &mut [u8], name: &[u8; VBAN_STREAM_NAME_LEN], counter: u32) {
    write_header_as(VbanFormat::PROGRAM, out, name, counter);
}

/// #233: a zeroed buffer for one block's packets in `fmt`.
pub fn empty_packets(fmt: VbanFormat) -> Vec<u8> {
    vec![0; fmt.packet_len() * fmt.packets_per_block()]
}

/// #233: offset of packet `k` from the block's first packet (100 ns): `k`
/// slots of `1 / (30 · packets)` s, floored.
pub fn packet_offset_in(fmt: VbanFormat, k: usize) -> i64 {
    k as i64 * UNITS_PER_SECOND / (fmt.packets_per_block() as i64 * GENLOCK_GRID_FPS)
}

/// #233: when packet `k` of the block for the boundary `due_100ns` is sent in
/// `fmt`: `due + latency + offset(k)`, on the program's wall domain.
pub fn packet_send_at_in(fmt: VbanFormat, due_100ns: i64, latency_100ns: i64, k: usize) -> i64 {
    due_100ns + latency_100ns + packet_offset_in(fmt, k)
}

/// Offset of packet `k` from its block's first packet (100 ns): `k / 240 s`,
/// floored — 0, 41 666, 83 333, 125 000, …, 291 666 (= the `PROGRAM` format;
/// #210's tests).
#[cfg(test)]
pub fn packet_offset_100ns(k: usize) -> i64 {
    packet_offset_in(VbanFormat::PROGRAM, k)
}

/// When packet `k` of the block for the boundary `due_100ns` is sent:
/// `due + latency + k / 240 s`, on the program's wall domain (= the
/// `PROGRAM` format; #210's tests).
#[cfg(test)]
pub fn packet_send_at_100ns(due_100ns: i64, latency_100ns: i64, k: usize) -> i64 {
    packet_send_at_in(VbanFormat::PROGRAM, due_100ns, latency_100ns, k)
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

    /// #233: encode one block in `fmt` into `out` (`fmt.packets_per_block()`
    /// packets of `fmt.packet_len()` bytes): packet `k` carries interleaved
    /// samples `k·n .. (k+1)·n` (`n` = packet frames × 2), or silence when
    /// `samples` is `None` or not exactly one block at `fmt`'s rate.
    pub fn encode_into(
        &mut self,
        fmt: VbanFormat,
        name: &[u8; VBAN_STREAM_NAME_LEN],
        samples: Option<&[f32]>,
        out: &mut [u8],
    ) {
        let per_packet = fmt.packet_frames() * VBAN_CHANNELS;
        let bytes = fmt.bytes_per_sample();
        let samples = samples.filter(|s| s.len() == fmt.block_frames() * VBAN_CHANNELS);
        let packets = out
            .chunks_exact_mut(fmt.packet_len())
            .take(fmt.packets_per_block());
        for (k, packet) in packets.enumerate() {
            write_header_as(fmt, &mut packet[..VBAN_HEADER_LEN], name, self.counter);
            self.counter = self.counter.wrapping_add(1);
            let payload = &mut packet[VBAN_HEADER_LEN..];
            match samples {
                Some(s) => {
                    let first = k * per_packet;
                    let chunk = &s[first..first + per_packet];
                    for (dst, &x) in payload.chunks_exact_mut(bytes).zip(chunk) {
                        write_sample(fmt.sample(), dst, x);
                    }
                }
                None => payload.fill(0),
            }
        }
    }

    /// Encode one 48 kHz INT24 block into `out` (= the `PROGRAM` format):
    /// packet `k` carries interleaved samples `k·400 .. (k+1)·400` of
    /// `samples`, or silence when `samples` is `None` or not exactly
    /// [`VBAN_BLOCK_SAMPLES`] long. #210's tests.
    #[cfg(test)]
    pub fn encode_block(
        &mut self,
        name: &[u8; VBAN_STREAM_NAME_LEN],
        samples: Option<&[f32]>,
        out: &mut VbanBlockPackets,
    ) {
        self.encode_into(VbanFormat::PROGRAM, name, samples, out.as_flattened_mut());
    }

    /// Test-only: start the counter at `counter` (the wrap test).
    #[cfg(test)]
    pub fn starting_at(counter: u32) -> Self {
        Self { counter }
    }
}

/// A block's packets, zeroed (#210's tests).
#[cfg(test)]
pub fn empty_block_packets() -> Box<VbanBlockPackets> {
    Box::new([[0u8; VBAN_PACKET_LEN]; VBAN_PACKETS_PER_BLOCK])
}

#[cfg(test)]
#[path = "vban_packet_tests.rs"]
pub(crate) mod tests;
#[cfg(test)]
#[path = "vban_packet_tests_format.rs"]
mod tests_format;
#[cfg(test)]
#[path = "vban_packet_tests_legacy.rs"]
pub(crate) mod tests_legacy;
