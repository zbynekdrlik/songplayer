//! #184 round G3 — the live preview's audio feeder timing, as a pure,
//! Linux-tested state machine. The feeder thread in `preview_encoder.rs`
//! (`mutants::skip` glue) only moves bytes: it pushes every tapped block into
//! [`AudioHold`] and writes whatever [`AudioHold::take_writes`] returns.
//!
//! Why (measured with `scripts/preview_latency_repro.py`, see
//! `.claude/rules/preview.md` "#184 round G3"): the decode-seam audio LEADS the
//! video by `lead_ms` (1500 ms on the SDK-clocked path). Round G2 put that lead
//! INTO the encoder's audio input — a lead-long silence preroll, then every
//! block the moment it arrived — so ~1.5 s (~576 KB) of PCM had to sit in flight
//! in the loopback socket, because ffmpeg consumes audio only in step with the
//! wall-clock video. With Windows-sized loopback buffers it does not fit:
//! `write_all` blocks, the bounded seam channel fills and keeps its OLDEST
//! blocks, and the aligner — which placed a block by when it was DEQUEUED —
//! wrote them seconds late while its `ahead_ms` still read 0. That invisible
//! wait was the owner's ~10 s fader-to-preview lag.
//!
//! [`AudioHold`] keeps the lead on OUR side instead: a block is written
//! `lead − write_ahead` after it ARRIVED at the seam (its [`AudioBlock`]
//! stamp) and is placed by that arrival — `align_block` against
//! `arrival + lead` — so the audio in flight is bounded to
//! [`AUDIO_WRITE_AHEAD_MS`] plus one block whatever the socket buffers are, and
//! a block that waited too long is trimmed, never delayed. The round-G2
//! guarantees stay: silence up to the wall when no audio comes (the encoder is
//! never starved), a burst trimmed to ≤ 300 ms ahead, whole frames only.
//!
//! Units: times are µs since the feeder started; positions are stereo frames on
//! the encoder's sample-count audio timeline ([`PREVIEW_AUDIO_FRAMES_PER_MS`]).
//!
//! [`AudioBlock`]: super::preview_stream::AudioBlock

use std::collections::VecDeque;

use super::preview_stream::{
    PREVIEW_AUDIO_FRAMES_PER_MS, align_block, align_timeout, block_tail_range,
};

/// How far ahead of the wall-clock video the written audio runs (ms), capped at
/// the seam lead. Enough that ffmpeg never waits for audio (a silence pad only
/// fires once the written audio is 150 ms behind its target, so the audio stays
/// ≥ 50 ms ahead of the video), small enough (~77 KB of f32 stereo) to fit any
/// loopback socket buffer — the whole point of round G3.
pub const AUDIO_WRITE_AHEAD_MS: u64 = 200;

/// Most blocks held at once — a safety bound only: the normal hold is one lead
/// (~1.3 s ≈ 40-80 seam blocks). Past it the OLDEST held block is dropped (a
/// flood that large is a catch-up burst whose head would be trimmed anyway).
pub const MAX_HELD_BLOCKS: usize = 512;

/// One write the feeder performs on the encoder's audio socket.
#[derive(Debug, PartialEq)]
pub enum AudioWrite {
    /// This many stereo frames of digital silence.
    Silence(usize),
    /// Interleaved-stereo f32 samples — whole frames only.
    Samples(Vec<f32>),
}

/// The feeder's audio timeline + the blocks it is holding (see the module doc).
#[derive(Debug)]
pub struct AudioHold {
    /// Stereo frames already written as the connect-gap preroll.
    base_frames: u64,
    /// The decode-seam A/V lead (µs).
    lead_us: u64,
    /// The write-ahead (µs): [`AUDIO_WRITE_AHEAD_MS`], never more than the lead.
    ahead_us: u64,
    /// Held blocks, oldest first: (arrival µs, interleaved-stereo samples).
    held: VecDeque<(u64, Vec<f32>)>,
    written_frames: u64,
    padded_frames: u64,
    skipped_frames: u64,
    dropped_blocks: u64,
}

/// Stereo frames in `us` microseconds of 48 kHz audio.
fn frames_in(us: u64) -> u64 {
    us * PREVIEW_AUDIO_FRAMES_PER_MS / 1000
}

impl AudioHold {
    /// A feeder that has just written `base_frames` of connect-gap preroll
    /// silence, for a pipeline whose seam audio leads its video by `lead_ms`.
    pub fn new(base_frames: u64, lead_ms: u32) -> Self {
        let lead_us = u64::from(lead_ms) * 1000;
        Self {
            base_frames,
            lead_us,
            ahead_us: lead_us.min(AUDIO_WRITE_AHEAD_MS * 1000),
            held: VecDeque::new(),
            written_frames: base_frames,
            padded_frames: 0,
            skipped_frames: 0,
            dropped_blocks: 0,
        }
    }

    /// The effective write-ahead (ms) — logged at feeder start.
    pub fn write_ahead_ms(&self) -> u64 {
        self.ahead_us / 1000
    }

    /// Where the written audio should be at `now_us`: the wall clock plus the
    /// write-ahead.
    pub fn position_at(&self, now_us: u64) -> u64 {
        self.base_frames + frames_in(now_us + self.ahead_us)
    }

    /// Where a block that ARRIVED at the seam at `arrival_us` must START: its
    /// arrival plus the seam lead (its content belongs to the video frame the
    /// wall shows `lead` later).
    pub fn block_target(&self, arrival_us: u64) -> u64 {
        self.base_frames + frames_in(arrival_us + self.lead_us)
    }

    /// When that block is due to be written: `lead − write_ahead` after it
    /// arrived, i.e. exactly when [`Self::position_at`] reaches its target.
    pub fn due_us(&self, arrival_us: u64) -> u64 {
        arrival_us + self.lead_us - self.ahead_us
    }

    /// Hold one tapped block (arrival µs since the feeder started). Past
    /// [`MAX_HELD_BLOCKS`] the OLDEST held block is dropped and counted.
    pub fn push(&mut self, arrival_us: u64, samples: Vec<f32>) {
        if self.held.len() >= MAX_HELD_BLOCKS {
            if let Some((_, oldest)) = self.held.pop_front() {
                self.skipped_frames += (oldest.len() / 2) as u64;
                self.dropped_blocks += 1;
            }
        }
        self.held.push_back((arrival_us, samples));
    }

    /// How long the feeder may block waiting for the next tapped block: until
    /// the oldest held block is due, at most `max_us` (so a silence pad still
    /// runs when nothing arrives).
    pub fn wait_us(&self, now_us: u64, max_us: u64) -> u64 {
        match self.held.front() {
            Some((arrival_us, _)) => self.due_us(*arrival_us).saturating_sub(now_us).min(max_us),
            None => max_us,
        }
    }

    /// Everything the feeder must write at `now_us`, in order: every DUE held
    /// block, each aligned by its ARRIVAL (`align_block` against
    /// [`Self::block_target`] — silence first if the audio is behind it, the
    /// block's oldest frames dropped if it would end > 300 ms past it), then
    /// silence up to [`Self::position_at`] once the audio is > 150 ms behind it
    /// (`align_timeout` — the encoder is never starved of audio).
    pub fn take_writes(&mut self, now_us: u64) -> Vec<AudioWrite> {
        let mut out = Vec::new();
        while let Some(arrival_us) = self.held.front().map(|(a, _)| *a) {
            if self.due_us(arrival_us) > now_us {
                break;
            }
            let Some((_, mut samples)) = self.held.pop_front() else {
                break;
            };
            let a = align_block(
                self.block_target(arrival_us),
                self.written_frames,
                samples.len() / 2,
            );
            self.pad(a.pad_frames, &mut out);
            self.skipped_frames += a.skip_frames as u64;
            let range = block_tail_range(a.skip_frames, samples.len());
            if !range.is_empty() {
                self.written_frames += (range.len() / 2) as u64;
                samples.truncate(range.end);
                samples.drain(..range.start);
                out.push(AudioWrite::Samples(samples));
            }
        }
        let pad = align_timeout(self.position_at(now_us), self.written_frames);
        self.pad(pad, &mut out);
        out
    }

    fn pad(&mut self, frames: usize, out: &mut Vec<AudioWrite>) {
        if frames > 0 {
            self.written_frames += frames as u64;
            self.padded_frames += frames as u64;
            out.push(AudioWrite::Silence(frames));
        }
    }

    /// Stereo frames written so far (preroll included).
    pub fn written_frames(&self) -> u64 {
        self.written_frames
    }
    /// Stereo frames of silence padded so far (preroll excluded).
    pub fn padded_frames(&self) -> u64 {
        self.padded_frames
    }
    /// Stereo frames of tapped audio dropped so far (trimmed or capped).
    pub fn skipped_frames(&self) -> u64 {
        self.skipped_frames
    }
    /// Held blocks dropped by the [`MAX_HELD_BLOCKS`] cap.
    pub fn dropped_blocks(&self) -> u64 {
        self.dropped_blocks
    }
    /// Stereo frames currently held (not yet due).
    pub fn held_frames(&self) -> u64 {
        self.held.iter().map(|(_, s)| (s.len() / 2) as u64).sum()
    }
}

#[cfg(test)]
#[path = "preview_audio_hold_tests.rs"]
mod tests;
