//! Live N-stem mixing audio reader (#186).
//!
//! [`StemMixReader`] wraps N sample-aligned [`AudioStream`]s (all decoded from
//! the same normalized source, so they share sample rate + channel count) and
//! presents ONE [`AudioStream`] whose samples are the streams mixed with a
//! per-stream live gain:
//!
//! ```text
//! out[i] = clamp(Σ stream_k[i] * gain_k, -1, 1)
//! ```
//!
//! It replaces the old two-stream `KaraokeAudioReader`: karaoke MODES are now
//! gain PRESETS over the SAME open streams (`[original, vocals, instrumental]`),
//! so switching a mode is a gain write — the decoder is NEVER reopened, which is
//! the seconds-of-silence dropout #186 fixes. #183 (dub: voice/dub/ambient) and
//! #181 (D2 UI) reuse this exact type with different streams.
//!
//! ## Ramps — a preset change crossfades, never clicks
//!
//! Each stream's applied gain moves LINEARLY toward its atomic target over
//! `ramp_samples = sample_rate / 20` frames (50 ms): at most `1 / ramp_samples`
//! per output frame. So a preset change (e.g. vocals 1.0 -> 0.0) is a 50 ms
//! crossfade rather than a discontinuity. A song OPENS at its current preset
//! (gains initialised to the live targets), so there is no fade-in on play; only
//! LATER changes ramp.
//!
//! Because it implements `AudioStream`, [`crate::split_sync::SplitSyncedDecoder`]
//! drives it exactly like a plain [`crate::SymphoniaAudioReader`] — the A/V sync,
//! pacer, genlock and NDI submit paths downstream are completely untouched.
//!
//! The streams are encoded independently, so their FLAC packet boundaries need
//! not line up. The reader buffers each stream and emits only the overlapping
//! whole-frame portion each call, carrying the remainder forward; a stream that
//! has ended mixes as silence so the longest stream still reaches its end.
//! Timestamps come from a cumulative sample-frame counter (monotonic).
//!
//! ## Stage level probe (#184 round G4)
//!
//! Once a second the reader logs `stem-mix level` at INFO: the RMS of the
//! frames it EMITTED in that second (post-gain, so a fader at 0 must read the
//! floor here), the target and applied gain per stream, and `gains_id` — the
//! address of the first target atomic, which `playback/mix.rs` also logs for the
//! control's live sets, so the log proves whether this reader holds the SAME
//! atomics the faders write.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use tracing::info;

use crate::error::DecoderError;
use crate::level_probe::{LevelProbe, LevelReading};
use crate::stream::{AudioStream, MediaStream};
use crate::types::DecodedAudioFrame;

/// Ramp duration as a divisor of the sample rate: `sample_rate / 20` frames
/// = 50 ms. The one place the ramp length is defined.
const RAMP_DIVISOR: u32 = 20;

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

/// Identity of a live gain set: the address of its FIRST atomic (`0` for an
/// empty set). Two holders print the same id iff they share the same `Arc`s —
/// the #184 G4 probe compares a reader's id with the mixer control's.
pub fn gains_id(targets: &[Arc<AtomicU32>]) -> usize {
    targets.first().map_or(0, |t| Arc::as_ptr(t) as usize)
}

/// Render gains for a log line with 2 decimals: `[0.00,1.00,0.50]`.
pub fn format_gains(gains: &[f32]) -> String {
    let parts: Vec<String> = gains.iter().map(|g| format!("{g:.2}")).collect();
    format!("[{}]", parts.join(","))
}

/// Mixes N sample-aligned [`AudioStream`]s into one, with live per-stream gain
/// and 50 ms linear gain ramps. See the module docs.
pub struct StemMixReader {
    streams: Vec<Box<dyn AudioStream>>,
    /// Live target gain per stream (f32 bits) — written by the karaoke control,
    /// read live per output frame.
    targets: Vec<Arc<AtomicU32>>,
    /// The gain actually applied, ramped toward `targets` a step per frame.
    current: Vec<f32>,
    /// Frames the ramp takes to cross the full 0..=1 range (`sample_rate / 20`).
    ramp_samples: u32,
    /// Max gain change per output frame (`1 / ramp_samples`).
    step: f32,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    /// Interleaved f32 leftover per stream (packet boundaries differ).
    bufs: Vec<VecDeque<f32>>,
    eos: Vec<bool>,
    /// Per-channel sample-frames emitted so far — drives the output timestamp.
    emitted_frames: u64,
    /// Human label for the `stem-mix level` log line (#184 G4), e.g.
    /// `dub-4:<audio file stem>`. Defaults to `<n>-stream`.
    label: String,
    /// 1 Hz level probe over the emitted (post-gain) output.
    probe: LevelProbe,
}

impl std::fmt::Debug for StemMixReader {
    // The boxed `dyn AudioStream` streams are not `Debug`; print the mixer's own
    // observable state (enough for test assertions on the reader).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StemMixReader")
            .field("streams", &self.streams.len())
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("duration_ms", &self.duration_ms)
            .field("ramp_samples", &self.ramp_samples)
            .field("current", &self.current)
            .field("eos", &self.eos)
            .field("emitted_frames", &self.emitted_frames)
            .field("label", &self.label)
            .finish()
    }
}

impl StemMixReader {
    /// Build from N owned stream readers plus their live gain atomics (same
    /// length, at least one). All streams must agree on sample rate + channel
    /// count (they are derived from the same source, so they always do); a
    /// disagreement — or a stream/gain count mismatch, or an empty stream set —
    /// returns [`DecoderError::Mismatch`] rather than silently mixing misaligned
    /// audio, so the caller can fall back to the plain mix.
    pub fn new(
        streams: Vec<Box<dyn AudioStream>>,
        targets: Vec<Arc<AtomicU32>>,
    ) -> Result<Self, DecoderError> {
        if streams.is_empty() {
            return Err(DecoderError::Mismatch(
                "stem mixer needs at least one stream".to_string(),
            ));
        }
        if streams.len() != targets.len() {
            return Err(DecoderError::Mismatch(format!(
                "stem mixer stream/gain count mismatch: {} streams vs {} gains",
                streams.len(),
                targets.len()
            )));
        }
        let sample_rate = streams[0].sample_rate();
        let channels = streams[0].channels();
        let mut duration_ms = 0u64;
        for (idx, s) in streams.iter().enumerate() {
            if s.sample_rate() != sample_rate {
                return Err(DecoderError::Mismatch(format!(
                    "stem {idx} sample rate {} != {sample_rate}",
                    s.sample_rate()
                )));
            }
            if s.channels() != channels {
                return Err(DecoderError::Mismatch(format!(
                    "stem {idx} channels {} != {channels}",
                    s.channels()
                )));
            }
            // Master duration is the longest stem (same source => all equal).
            duration_ms = duration_ms.max(s.duration_ms());
        }

        let n = streams.len();
        let ramp_samples = (sample_rate / RAMP_DIVISOR).max(1);
        let step = 1.0 / ramp_samples as f32;
        // Open at the current preset (no fade-in): the applied gain starts at the
        // live target; only LATER target changes ramp.
        let current: Vec<f32> = targets
            .iter()
            .map(|t| gain_from_bits(t.load(Ordering::Relaxed)))
            .collect();
        Ok(Self {
            streams,
            targets,
            current,
            ramp_samples,
            step,
            sample_rate,
            channels,
            duration_ms,
            bufs: (0..n).map(|_| VecDeque::new()).collect(),
            eos: vec![false; n],
            emitted_frames: 0,
            label: format!("{n}-stream"),
            probe: LevelProbe::new(Instant::now()),
        })
    }

    /// Set the label printed on this reader's `stem-mix level` line (#184 G4).
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// The label printed on this reader's `stem-mix level` line.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Identity of the gain atomics this reader reads ([`gains_id`] of its
    /// targets).
    pub fn gains_id(&self) -> usize {
        gains_id(&self.targets)
    }

    /// Level + sample count of the output emitted in the probe's still-open
    /// window — exposed for tests that assert the probe measures post-gain.
    #[cfg(test)]
    pub(crate) fn pending_level(&self) -> (f32, u64) {
        self.probe.pending()
    }

    /// Keep the probe window open for the rest of a test (starts it an hour
    /// ahead), so a slow test run can never close it mid-assertion.
    #[cfg(test)]
    pub(crate) fn hold_probe_window(&mut self) {
        self.probe = LevelProbe::new(Instant::now() + std::time::Duration::from_secs(3600));
    }

    /// Emit the 1 Hz `stem-mix level` line for a closed probe window.
    fn log_level(&self, r: &LevelReading) {
        let targets: Vec<f32> = self
            .targets
            .iter()
            .map(|t| gain_from_bits(t.load(Ordering::Relaxed)))
            .collect();
        info!(
            label = %self.label,
            "stem-mix level rms_dbfs={:.1} targets={} applied={} gains_id={:#x} samples={} blocks={} window_ms={}",
            r.rms_dbfs,
            format_gains(&targets),
            format_gains(&self.current),
            self.gains_id(),
            r.samples,
            r.blocks,
            r.window_ms
        );
    }

    /// The ramp length (`sample_rate / 20`, >= 1) in output frames — exposed for
    /// tests that assert the 50 ms ramp directly.
    #[cfg(test)]
    pub(crate) fn ramp_samples(&self) -> u32 {
        self.ramp_samples
    }

    /// Pull one packet into `buf` if empty and not ended. Loops past any
    /// zero-length packets so an empty (but non-EOS) packet does not stall.
    fn fill(
        stream: &mut dyn AudioStream,
        buf: &mut VecDeque<f32>,
        eos: &mut bool,
    ) -> Result<(), DecoderError> {
        while buf.is_empty() && !*eos {
            match stream.next_samples()? {
                Some(frame) => buf.extend(frame.data),
                None => *eos = true,
            }
        }
        Ok(())
    }
}

impl MediaStream for StemMixReader {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        for s in &mut self.streams {
            s.seek(position_ms)?;
        }
        for b in &mut self.bufs {
            b.clear();
        }
        self.eos.fill(false);
        // Re-anchor the output timestamp to the seek position.
        self.emitted_frames = position_ms.saturating_mul(self.sample_rate as u64) / 1000;
        Ok(())
    }
}

impl AudioStream for StemMixReader {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let ch = self.channels as usize;
        let n_streams = self.streams.len();
        for k in 0..n_streams {
            Self::fill(
                self.streams[k].as_mut(),
                &mut self.bufs[k],
                &mut self.eos[k],
            )?;
        }

        // How many interleaved samples we can emit: the overlap of the streams
        // that still have buffered data. A stream that has ENDED (empty + EOS)
        // does not constrain the count — it mixes as silence for the remaining
        // frames of the longer streams.
        let mut avail: Option<usize> = None;
        for buf in &self.bufs {
            if !buf.is_empty() {
                avail = Some(avail.map_or(buf.len(), |a| a.min(buf.len())));
            }
        }
        let avail = match avail {
            Some(a) => a,
            None => return Ok(None), // every stream drained and at EOS
        };
        // Emit whole sample-frames only. Buffers grow by whole interleaved
        // packets and shrink by a multiple of `ch`, so `n == 0` here means
        // genuinely nothing available yet (a true end-of-input), never a
        // stranded sub-frame remainder.
        let n = (avail / ch) * ch;
        if n == 0 {
            return Ok(None);
        }
        let frames = n / ch;

        let mut out = Vec::with_capacity(n);
        for _ in 0..frames {
            // Advance each stream's applied gain toward its live target by at
            // most one `step`, ONCE per frame — so a preset change crossfades
            // over `ramp_samples` frames (50 ms) instead of stepping.
            for k in 0..n_streams {
                let tgt = gain_from_bits(self.targets[k].load(Ordering::Relaxed));
                let cur = self.current[k];
                let delta = tgt - cur;
                // Snap once the target is within one step, else move one step in
                // its direction (`copysign` — no separate up/down branch).
                self.current[k] = if delta.abs() <= self.step {
                    tgt
                } else {
                    cur + self.step.copysign(delta)
                };
            }
            for _c in 0..ch {
                let mut acc = 0.0f32;
                for k in 0..n_streams {
                    acc += self.bufs[k].pop_front().unwrap_or(0.0) * self.current[k];
                }
                out.push(acc.clamp(-1.0, 1.0));
            }
        }

        let timestamp_ms = self.emitted_frames.saturating_mul(1000) / self.sample_rate as u64;
        self.emitted_frames += frames as u64;

        // #184 G4: 1 Hz level of what this reader actually emits (post-gain).
        self.probe.add(&out);
        if let Some(reading) = self.probe.poll(Instant::now()) {
            self.log_level(&reading);
        }

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
#[path = "stem_mix_tests.rs"]
mod tests;
