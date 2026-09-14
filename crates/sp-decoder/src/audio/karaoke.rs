//! Karaoke stem-mixing audio reader (#14).
//!
//! [`KaraokeAudioReader`] wraps TWO [`AudioStream`]s — the vocals stem and the
//! instrumental stem, both derived from the same normalized `{id}_audio.flac`
//! and therefore sample-aligned by construction — and presents a SINGLE
//! [`AudioStream`] whose samples are the two stems mixed with per-stream gain:
//!
//! ```text
//! out[i] = clamp(vocals[i] * vocal_gain + instrumental[i] * instrumental_gain, -1, 1)
//! ```
//!
//! Because it implements `AudioStream`, [`crate::split_sync::SplitSyncedDecoder`]
//! drives it exactly like a plain [`crate::SymphoniaAudioReader`] — the A/V sync,
//! pacer, genlock and NDI submit paths downstream see an identical
//! [`DecodedAudioFrame`] and are completely untouched.
//!
//! The two gains are read from `Arc<AtomicU32>` (f32 bits) on every emitted
//! chunk, so the dashboard's vocal-gain slider takes effect live, mid-song,
//! without rebuilding the pipeline.
//!
//! The stems are encoded independently, so their FLAC packet boundaries need not
//! line up. The reader buffers each stream and emits only the overlapping
//! portion each call, carrying the remainder forward. Timestamps come from a
//! cumulative sample counter (monotonic), not the inner readers' packet stamps.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::DecoderError;
use crate::stream::{AudioStream, MediaStream};
use crate::types::DecodedAudioFrame;

/// Pack an `f32` gain into the shared atomic representation.
pub fn gain_to_bits(g: f32) -> u32 {
    g.to_bits()
}

/// Read an `f32` gain out of the shared atomic representation.
pub fn gain_from_bits(bits: u32) -> f32 {
    f32::from_bits(bits)
}

/// Convenience: build a shared gain atomic initialised to `g`.
pub fn shared_gain(g: f32) -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(gain_to_bits(g)))
}

/// Mixes a vocals stem and an instrumental stem into one [`AudioStream`] with
/// live per-stream gain. See the module docs.
pub struct KaraokeAudioReader {
    vocals: Box<dyn AudioStream>,
    instrumental: Box<dyn AudioStream>,
    /// f32 bits — read live per emitted chunk (dashboard slider).
    vocal_gain: Arc<AtomicU32>,
    instrumental_gain: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    /// Interleaved f32 leftover from each stream (packet boundaries differ).
    vbuf: VecDeque<f32>,
    ibuf: VecDeque<f32>,
    veos: bool,
    ieos: bool,
    /// Per-channel sample-frames emitted so far — drives the output timestamp.
    emitted_frames: u64,
}

impl std::fmt::Debug for KaraokeAudioReader {
    // The boxed `dyn AudioStream` fields are not `Debug`, so skip them and print
    // the mixer's own observable state (enough for test assertions on the reader).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KaraokeAudioReader")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("duration_ms", &self.duration_ms)
            .field("vbuf_len", &self.vbuf.len())
            .field("ibuf_len", &self.ibuf.len())
            .field("veos", &self.veos)
            .field("ieos", &self.ieos)
            .field("emitted_frames", &self.emitted_frames)
            .finish()
    }
}

impl KaraokeAudioReader {
    /// Build from two owned stem readers plus the shared gain atomics.
    ///
    /// Both stems must agree on sample rate and channel count (they are derived
    /// from the same source, so they always do); a disagreement returns
    /// [`DecoderError::Mismatch`] rather than silently mixing misaligned audio.
    pub fn new(
        vocals: Box<dyn AudioStream>,
        instrumental: Box<dyn AudioStream>,
        vocal_gain: Arc<AtomicU32>,
        instrumental_gain: Arc<AtomicU32>,
    ) -> Result<Self, DecoderError> {
        if vocals.sample_rate() != instrumental.sample_rate() {
            return Err(DecoderError::Mismatch(format!(
                "stem sample rates differ: vocals {} vs instrumental {}",
                vocals.sample_rate(),
                instrumental.sample_rate()
            )));
        }
        if vocals.channels() != instrumental.channels() {
            return Err(DecoderError::Mismatch(format!(
                "stem channel counts differ: vocals {} vs instrumental {}",
                vocals.channels(),
                instrumental.channels()
            )));
        }
        let sample_rate = vocals.sample_rate();
        let channels = vocals.channels();
        // Master duration is the vocals stem (same source ⇒ equal to instrumental).
        let duration_ms = vocals.duration_ms();
        Ok(Self {
            vocals,
            instrumental,
            vocal_gain,
            instrumental_gain,
            sample_rate,
            channels,
            duration_ms,
            vbuf: VecDeque::new(),
            ibuf: VecDeque::new(),
            veos: false,
            ieos: false,
            emitted_frames: 0,
        })
    }

    /// Pull one packet into `buf` from `stream` if `buf` is empty and the stream
    /// has not ended. Returns `Ok(())`; sets `*eos` at end-of-stream.
    fn fill(
        stream: &mut dyn AudioStream,
        buf: &mut VecDeque<f32>,
        eos: &mut bool,
    ) -> Result<(), DecoderError> {
        // Loop past any zero-length packets so an empty (but non-EOS) packet
        // does not stall the mixer.
        while buf.is_empty() && !*eos {
            match stream.next_samples()? {
                Some(frame) => buf.extend(frame.data),
                None => *eos = true,
            }
        }
        Ok(())
    }
}

impl MediaStream for KaraokeAudioReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.vocals.seek(position_ms)?;
        self.instrumental.seek(position_ms)?;
        self.vbuf.clear();
        self.ibuf.clear();
        self.veos = false;
        self.ieos = false;
        // Re-anchor the output timestamp to the seek position.
        self.emitted_frames = position_ms.saturating_mul(self.sample_rate as u64) / 1000;
        Ok(())
    }
}

impl AudioStream for KaraokeAudioReader {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Self::fill(self.vocals.as_mut(), &mut self.vbuf, &mut self.veos)?;
        Self::fill(self.instrumental.as_mut(), &mut self.ibuf, &mut self.ieos)?;

        let ch = self.channels as usize;
        // How many interleaved samples we can emit this call: the overlap of the
        // two buffers, or — once one stem has ended — whatever remains of the
        // other (mixed against silence for the ended stem).
        let avail = match (self.vbuf.is_empty(), self.ibuf.is_empty()) {
            (false, false) => self.vbuf.len().min(self.ibuf.len()),
            (false, true) => self.vbuf.len(),
            (true, false) => self.ibuf.len(),
            (true, true) => return Ok(None), // both drained and at EOS
        };
        // Emit whole sample-frames only. Both buffers only ever grow by whole
        // interleaved packets and shrink by the same `n` (a multiple of `ch`), so
        // a buffer never carries a partial frame across calls — `n == 0` here
        // therefore means genuinely nothing is available yet, never a stranded
        // sub-frame remainder, so returning `Ok(None)` is a true end-of-input.
        let n = (avail / ch) * ch;
        if n == 0 {
            return Ok(None);
        }

        let vg = gain_from_bits(self.vocal_gain.load(Ordering::Relaxed));
        let ig = gain_from_bits(self.instrumental_gain.load(Ordering::Relaxed));

        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let v = self.vbuf.pop_front().unwrap_or(0.0);
            let i = self.ibuf.pop_front().unwrap_or(0.0);
            out.push((v * vg + i * ig).clamp(-1.0, 1.0));
        }

        let timestamp_ms = self.emitted_frames.saturating_mul(1000) / self.sample_rate as u64;
        self.emitted_frames += (n / ch) as u64;

        Ok(Some(DecodedAudioFrame {
            data: out,
            channels: self.channels as u32,
            sample_rate: self.sample_rate,
            timestamp_ms,
        }))
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
#[path = "karaoke_tests.rs"]
mod tests;
