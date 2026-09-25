//! #148 v4 — the paced audio read-ahead cushion, end to end.
//!
//! Drives the PRODUCTION paced decoder opener ([`open_paced_decoder`], lead
//! [`PACED_AUDIO_LEAD_MS`]) over mock readers whose audio sample VALUES encode
//! their own media sample index (the `enc` technique of the parent
//! `pacer_tests_av_align.rs`). Each decoded frame is converted exactly like
//! `pipeline_paced::to_paced_frame` does it (0-based media time in
//! `timecode_100ns`) and fed through the pure [`Pacer`]. A mid-song video stall
//! (no pull for 5 boundaries ≈ 167 ms) must then play with **0 underruns**,
//! bit-exact audio across the stall and **0 A/V corrections**. The deeper
//! buffer never moves the A/V line, because the grid is aligned by media time.
//! A control run at the 40 ms pacing-OFF pairing deadline starves on the same
//! stall, which proves the stall is real.

use std::ops::RangeInclusive;

use sp_decoder::split_sync::DEFAULT_TOLERANCE_MS;
use sp_decoder::{
    AudioStream, DecodedAudioFrame, DecodedVideoFrame, DecoderError, MediaStream, PixelFormat,
    SplitSyncedDecoder, VideoStream,
};
use sp_ndi::AudioFrame;

use super::{CHUNK, RATE, Rec, SPB, anchored, assert_exact_stream, b, enc, frame};
use crate::playback::audio_grid::AudioGridBuffer;
use crate::playback::pacer::{PACED_AUDIO_LEAD_MS, PacedFrame, Pacer, open_paced_decoder};

/// The stall: 5 consecutive boundaries (≈ 167 ms) on which nothing is pulled.
const STALL: RangeInclusive<i64> = 61..=65;

/// 30-fps video with the decoder's integer-ms PTS: frame `j` at `⌊j·1000/30⌋`.
struct Video30 {
    next: u64,
    frames: u64,
}

impl MediaStream for Video30 {
    fn duration_ms(&self) -> u64 {
        self.frames * 1000 / 30
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl VideoStream for Video30 {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let ts = self.next * 1000 / 30;
        self.next += 1;
        Ok(Some(DecodedVideoFrame {
            data: vec![0u8; 12],
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: ts,
            pixel_format: PixelFormat::Nv12,
        }))
    }
    fn width(&self) -> u32 {
        4
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (30, 1)
    }
}

/// Contiguous stereo audio from media 0 in 48 ms chunks (`CHUNK` samples, an
/// integer-ms timestamp each); sample `s` is `enc(s)` on ch0 and `−enc(s)` on
/// ch1.
struct EncodedAudio {
    next: i64,
    chunks: i64,
}

impl MediaStream for EncodedAudio {
    fn duration_ms(&self) -> u64 {
        (self.chunks * CHUNK * 1000 / RATE) as u64
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl AudioStream for EncodedAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        if self.next >= self.chunks {
            return Ok(None);
        }
        let start = self.next * CHUNK;
        self.next += 1;
        let mut data = Vec::with_capacity((CHUNK * 2) as usize);
        for s in start..start + CHUNK {
            data.push(enc(s));
            data.push(-enc(s));
        }
        Ok(Some(DecodedAudioFrame {
            data,
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: (start * 1000 / RATE) as u64,
        }))
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

/// 10 s of video and 12 s of audio — neither runs dry within the runs below.
fn readers() -> (Box<dyn VideoStream>, Box<dyn AudioStream>) {
    (
        Box::new(Video30 {
            next: 0,
            frames: 300,
        }),
        Box::new(EncodedAudio {
            next: 0,
            chunks: 250,
        }),
    )
}

/// `pipeline_paced::to_paced_frame` with a 0 ms origin: the frame PTS and each
/// chunk's 0-based media time, in 100 ns.
fn to_paced(video: DecodedVideoFrame, audio: Vec<DecodedAudioFrame>) -> PacedFrame {
    let chunks = audio
        .into_iter()
        .map(|af| AudioFrame {
            timecode_100ns: Some(af.timestamp_ms as i64 * 10_000),
            data: af.data,
            channels: af.channels,
            sample_rate: af.sample_rate,
        })
        .collect();
    frame(video.timestamp_ms as i64 * 10_000, chunks)
}

/// Service boundaries `1..=to` on time, pulling from `dec` except on the
/// `stall` boundaries (the decoder delivers nothing there).
fn run_with_stall(
    mut dec: SplitSyncedDecoder,
    to: i64,
    stall: RangeInclusive<i64>,
) -> (Pacer, Rec) {
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    for k in 1..=to {
        clk.set(b(k));
        let stalled = stall.contains(&k);
        pacer.service(
            || {
                if stalled {
                    return None;
                }
                dec.next_synced()
                    .expect("mock readers never fail")
                    .map(|(v, a)| to_paced(v, a))
            },
            &mut rec,
        );
    }
    (pacer, rec)
}

#[test]
fn a_150_ms_video_stall_mid_song_plays_the_paced_audio_bit_exact_with_no_underrun() {
    let (v, a) = readers();
    let dec = open_paced_decoder(v, a).expect("valid mock readers");
    let (pacer, rec) = run_with_stall(dec, 150, STALL);

    assert_eq!(
        rec.blocks.len(),
        150,
        "every boundary emits (a frame, or a repeat during the stall)"
    );
    assert_eq!(
        pacer.audio_stats().underruns,
        0,
        "the {PACED_AUDIO_LEAD_MS} ms cushion carries the audio across the stall"
    );
    // Bit-exact and on the wall line before, across and after the stall.
    assert_exact_stream(&rec.blocks, 0);
    let s = pacer.stats();
    assert_eq!(
        s.av_corrections, 0,
        "no drop/pad: the audio never left the line"
    );
    assert_eq!(s.av_corrected_samples, 0);
    assert_eq!(s.av_align_err_ms, 0.0);
    assert_eq!(pacer.audio_stats().overflows, 0, "the 2 s cap is never hit");
    // The cushion is real: about the lead's worth of media stays buffered after
    // the last take (≥ 249 ms here), and that depth changed nothing above.
    assert!(
        pacer.audio_stats().buffer_ms >= PACED_AUDIO_LEAD_MS - 10,
        "buffered {} ms, want about the {PACED_AUDIO_LEAD_MS} ms lead",
        pacer.audio_stats().buffer_ms
    );
}

#[test]
fn the_same_stall_starves_the_audio_at_the_40_ms_pairing_deadline() {
    // Control: without the paced lead (the pacing-OFF pairing deadline) the
    // grid holds only ~40 ms past the parked frame, so the stall underruns.
    let (v, a) = readers();
    let dec = SplitSyncedDecoder::new(v, a).expect("valid mock readers");
    let (pacer, rec) = run_with_stall(dec, 150, STALL);
    assert_eq!(rec.blocks.len(), 150);
    assert!(
        pacer.audio_stats().underruns > 0,
        "a 5-boundary stall must starve a 40 ms cushion (else the test stall is not real)"
    );
}

#[test]
fn the_paced_opener_uses_the_250_ms_lead_and_pacing_off_keeps_40() {
    assert_eq!(PACED_AUDIO_LEAD_MS, 250);
    let (v, a) = readers();
    let paced = open_paced_decoder(v, a).expect("valid mock readers");
    assert_eq!(paced.audio_lead_ms(), PACED_AUDIO_LEAD_MS);
    // Pacing OFF: `pipeline_audio::open_synced_decoder` pairs at
    // `decoder_tolerance_ms(false)` — the unchanged 40 ms default.
    assert_eq!(
        crate::playback::pipeline::audio_emitter::decoder_tolerance_ms(false),
        DEFAULT_TOLERANCE_MS
    );
    assert_eq!(DEFAULT_TOLERANCE_MS, 40);
    let (v, a) = readers();
    let off = SplitSyncedDecoder::new(v, a).expect("valid mock readers");
    assert_eq!(off.audio_lead_ms(), DEFAULT_TOLERANCE_MS);
}

#[test]
fn the_2_s_grid_cap_holds_the_lead_plus_a_chunk_and_a_block() {
    // Worst case buffered: the lead, plus the one chunk the G5 bound allows past
    // it, plus the boundary block not yet taken.
    let cap = AudioGridBuffer::new(48_000).cap_samples() as i64;
    let lead = PACED_AUDIO_LEAD_MS as i64 * RATE / 1000;
    assert!(
        lead + CHUNK + SPB < cap,
        "lead {lead} + chunk {CHUNK} + block {SPB} samples must fit the {cap}-sample cap"
    );
}
